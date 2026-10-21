//! [`MatterPeek`]: the controller's API.
//!
//! A handle to the service that owns the Matter stack: each call is an
//! operation sent to it, answered over its own channel. The handle is `Send +
//! Sync` and its futures are `Send`, so any thread (and UniFFI) may use it; the
//! stack itself stays on the service's thread.

use core::sync::atomic::{AtomicBool, Ordering};

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use super::types::*;

/// A controller of Matter devices: commissions them onto its fabric, keeps a
/// list of them, and reads, follows, updates and decommissions them.
#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
pub struct MatterPeek {
    ops: async_channel::Sender<Op>,
    /// Shared with the service, which checks it while commissioning.
    cancel: Arc<AtomicBool>,
}

impl MatterPeek {
    /// A controller whose service the host runs itself (see [`Service::run`]),
    /// e.g. without `std`. With `std`, [`MatterPeek::start`] does that.
    pub fn new() -> (Self, Service) {
        let (ops, ops_rx) = async_channel::unbounded();
        let cancel = Arc::new(AtomicBool::new(false));

        let peek = Self {
            ops,
            cancel: cancel.clone(),
        };

        (
            peek,
            Service {
                ops: ops_rx,
                cancel,
            },
        )
    }

    /// Send the operation `op` builds around its reply channel, and wait for
    /// the reply.
    async fn call<T>(&self, op: impl FnOnce(Reply<T>) -> Op) -> Result<T, PeekError> {
        let (reply, answer) = async_channel::bounded(1);

        self.ops.send(op(reply)).await.map_err(|_| stopped())?;
        answer.recv().await.map_err(|_| stopped())?
    }
}

#[cfg_attr(feature = "uniffi", uniffi::export)]
impl MatterPeek {
    /// Find the commissionable devices: on the network via mDNS, and over
    /// Bluetooth LE. `discriminator`, or else `setup_code`, narrows the search
    /// to one device; at most `max` devices are listed.
    pub async fn discover(
        &self,
        discriminator: Option<u16>,
        setup_code: Option<String>,
        max: u8,
    ) -> Result<Discovered, PeekError> {
        self.call(|reply| Op::Discover {
            discriminator,
            setup_code,
            max,
            reply,
        })
        .await
    }

    /// Read the setup code out of a picture of a device's QR code (a PNG or
    /// JPEG file).
    pub async fn read_qr_image(&self, image: Vec<u8>) -> Result<String, PeekError> {
        self.call(|reply| Op::ReadQrImage { image, reply }).await
    }

    /// Commission a device onto our fabric, reaching it `via` IP or Bluetooth,
    /// with its passcode, manual pairing code or QR code payload. It is listed
    /// from then on; `remember` also keeps a record of it. `progress` follows
    /// the steps.
    pub async fn commission(
        &self,
        via: CommissionVia,
        setup_code: String,
        remember: bool,
        progress: Option<Arc<dyn CommissionListener>>,
    ) -> Result<CommissionResult, PeekError> {
        self.call(|reply| Op::Commission {
            via,
            setup_code,
            remember,
            progress,
            reply,
        })
        .await
    }

    /// Cancel the running commission at its next safe point: before the device
    /// has our fabric it stops there; after, the device is decommissioned. The
    /// commission then fails with [`PeekError::Cancelled`].
    pub fn cancel_commission(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    /// The devices on our fabric.
    pub async fn list_devices(&self) -> Result<DeviceList, PeekError> {
        self.call(Op::ListDevices).await
    }

    /// Sync the device list with the devices on our fabric: list the ones not
    /// listed yet, refresh the others' identity, and note which are online.
    pub async fn sync(&self) -> Result<DeviceList, PeekError> {
        self.call(Op::Sync).await
    }

    /// Keep a record of a listed device, so it can be tagged and stays listed
    /// while offline and across restarts.
    pub async fn remember(&self, node_id: u64) -> Result<DeviceList, PeekError> {
        self.call(|reply| Op::Remember { node_id, reply }).await
    }

    /// Replace the local tags of a remembered device.
    pub async fn set_tags(&self, node_id: u64, tags: Vec<String>) -> Result<DeviceList, PeekError> {
        self.call(|reply| Op::SetTags {
            node_id,
            tags,
            reply,
        })
        .await
    }

    /// Remove our fabric from a listed device and drop it from the list, and
    /// its record. The device leaves the list even if it cannot be reached
    /// within 30 s, or with `force` is not contacted at all; one that still
    /// holds our fabric is listed again by a sync.
    pub async fn decommission(&self, node_id: u64, force: bool) -> Result<DeviceList, PeekError> {
        self.call(|reply| Op::Decommission {
            node_id,
            force,
            reply,
        })
        .await
    }

    /// Read the health of a listed device. Re-establishes its subscription at
    /// once if that was offline.
    pub async fn peek_device(&self, node_id: u64) -> Result<DeviceHealth, PeekError> {
        self.call(|reply| Op::PeekDevice { node_id, reply }).await
    }

    /// Retrieve a diagnostic log of a listed device; empty if it has none of
    /// that kind.
    pub async fn retrieve_logs(
        &self,
        node_id: u64,
        intent: LogIntent,
    ) -> Result<Vec<u8>, PeekError> {
        self.call(|reply| Op::RetrieveLogs {
            node_id,
            intent,
            reply,
        })
        .await
    }

    /// Make a listed device identify itself (e.g. blink) for `secs` seconds.
    pub async fn identify(&self, node_id: u64, secs: u16) -> Result<(), PeekError> {
        self.call(|reply| Op::Identify {
            node_id,
            secs,
            reply,
        })
        .await
    }

    /// Follow the health of a listed device live, until the returned
    /// subscription is stopped or dropped. `listener` first gets the health and
    /// state known so far, if any. One device subscription serves all of a
    /// device's listeners.
    pub async fn subscribe(
        &self,
        node_id: u64,
        listener: Arc<dyn HealthListener>,
    ) -> Result<Arc<Subscription>, PeekError> {
        let watcher = self
            .call(|reply| Op::Watch {
                node_id,
                listener,
                reply,
            })
            .await?;

        Ok(Arc::new(Subscription {
            node_id,
            watcher,
            ops: self.ops.clone(),
            stopped: AtomicBool::new(false),
        }))
    }

    /// The stored OTA images.
    pub async fn ota_images(&self) -> Result<Vec<OtaImageInfo>, PeekError> {
        self.call(Op::OtaImages).await
    }

    /// Start storing an OTA image file (`.ota`) of `size` bytes, written in
    /// chunks. The file is checked as it arrives.
    pub async fn ota_upload(&self, size: u64) -> Result<Arc<OtaUpload>, PeekError> {
        let id = self
            .call(|reply| Op::OtaUploadBegin { size, reply })
            .await?;

        Ok(Arc::new(OtaUpload {
            id,
            ops: self.ops.clone(),
            done: AtomicBool::new(false),
        }))
    }

    /// Delete stored OTA image `name`.
    pub async fn ota_delete(&self, name: String) -> Result<Vec<OtaImageInfo>, PeekError> {
        self.call(|reply| Op::OtaDelete { name, reply }).await
    }

    /// Update a listed device to OTA image `name`: allow it that image, and
    /// announce us to it as its OTA Provider. `progress` follows the update.
    pub async fn ota_update(
        &self,
        node_id: u64,
        name: String,
        progress: Option<Arc<dyn OtaListener>>,
    ) -> Result<(), PeekError> {
        self.call(|reply| Op::OtaUpdate {
            node_id,
            name,
            progress,
            reply,
        })
        .await
    }
}

/// A live health subscription to a device; stops when dropped.
#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
pub struct Subscription {
    node_id: u64,
    watcher: u64,
    ops: async_channel::Sender<Op>,
    stopped: AtomicBool,
}

#[cfg_attr(feature = "uniffi", uniffi::export)]
impl Subscription {
    /// The device followed.
    pub fn node_id(&self) -> u64 {
        self.node_id
    }

    /// Stop following the device.
    pub fn stop(&self) {
        if !self.stopped.swap(true, Ordering::SeqCst) {
            let _ = self.ops.try_send(Op::Unwatch {
                node_id: self.node_id,
                watcher: self.watcher,
            });
        }
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.stop();
    }
}

/// An OTA image file being stored; dropped unfinished, it is discarded.
#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
pub struct OtaUpload {
    id: u32,
    ops: async_channel::Sender<Op>,
    done: AtomicBool,
}

impl OtaUpload {
    pub(crate) fn id(&self) -> u32 {
        self.id
    }

    async fn call<T>(&self, op: impl FnOnce(Reply<T>) -> Op) -> Result<T, PeekError> {
        let (reply, answer) = async_channel::bounded(1);

        self.ops.send(op(reply)).await.map_err(|_| stopped())?;
        answer.recv().await.map_err(|_| stopped())?
    }
}

#[cfg_attr(feature = "uniffi", uniffi::export)]
impl OtaUpload {
    /// Write the next chunk of the file, which starts at `offset`. A chunk
    /// that is out of place, or makes the file invalid, fails the upload.
    pub async fn write(&self, offset: u64, data: Vec<u8>) -> Result<(), PeekError> {
        let written = self
            .call(|reply| Op::OtaUploadChunk {
                id: self.id,
                offset,
                data,
                reply,
            })
            .await;

        if written.is_err() {
            self.done.store(true, Ordering::SeqCst);
        }

        written
    }

    /// Check the whole file and store it; answers with the stored images.
    pub async fn finish(&self) -> Result<Vec<OtaImageInfo>, PeekError> {
        self.done.store(true, Ordering::SeqCst);
        self.call(|reply| Op::OtaUploadEnd { id: self.id, reply })
            .await
    }
}

impl Drop for OtaUpload {
    fn drop(&mut self) {
        if !self.done.load(Ordering::SeqCst) {
            let _ = self.ops.try_send(Op::OtaUploadDiscard { id: self.id });
        }
    }
}

/// Follows a device's health; see [`MatterPeek::subscribe`]. Called on the
/// controller's thread: return quickly.
#[cfg_attr(feature = "uniffi", uniffi::export(with_foreign))]
pub trait HealthListener: Send + Sync {
    /// A report: the first after each (re-)establishment carries all the
    /// health, later ones only what changed (see [`DeviceHealth::merge`]).
    /// `seq` counts the reports.
    fn on_report(&self, node_id: u64, seq: u64, health: DeviceHealth);

    /// The subscription's state changed.
    fn on_state(&self, node_id: u64, state: SubscriptionState);
}

/// Follows a commission; see [`MatterPeek::commission`]. Called on the
/// controller's thread: return quickly.
#[cfg_attr(feature = "uniffi", uniffi::export(with_foreign))]
pub trait CommissionListener: Send + Sync {
    fn on_step(&self, step: CommissionStep, status: StepStatus);
}

/// Follows an OTA update; see [`MatterPeek::ota_update`]. Called on the
/// controller's thread: return quickly.
#[cfg_attr(feature = "uniffi", uniffi::export(with_foreign))]
pub trait OtaListener: Send + Sync {
    fn on_progress(&self, node_id: u64, progress: OtaProgress);
}

/// The controller behind a [`MatterPeek`] made with [`MatterPeek::new`]: run it
/// with [`Service::run`].
pub struct Service {
    pub(crate) ops: async_channel::Receiver<Op>,
    pub(crate) cancel: Arc<AtomicBool>,
}

/// Where an operation's result goes.
pub(crate) type Reply<T> = async_channel::Sender<Result<T, PeekError>>;

/// An operation for the service, with where its result goes.
pub(crate) enum Op {
    Discover {
        discriminator: Option<u16>,
        setup_code: Option<String>,
        max: u8,
        reply: Reply<Discovered>,
    },
    ReadQrImage {
        image: Vec<u8>,
        reply: Reply<String>,
    },
    Commission {
        via: CommissionVia,
        setup_code: String,
        remember: bool,
        progress: Option<Arc<dyn CommissionListener>>,
        reply: Reply<CommissionResult>,
    },
    ListDevices(Reply<DeviceList>),
    Sync(Reply<DeviceList>),
    Remember {
        node_id: u64,
        reply: Reply<DeviceList>,
    },
    SetTags {
        node_id: u64,
        tags: Vec<String>,
        reply: Reply<DeviceList>,
    },
    Decommission {
        node_id: u64,
        force: bool,
        reply: Reply<DeviceList>,
    },
    PeekDevice {
        node_id: u64,
        reply: Reply<DeviceHealth>,
    },
    RetrieveLogs {
        node_id: u64,
        intent: LogIntent,
        reply: Reply<Vec<u8>>,
    },
    Identify {
        node_id: u64,
        secs: u16,
        reply: Reply<()>,
    },
    /// Add a listener to the device's subscription; answers with its ID.
    Watch {
        node_id: u64,
        listener: Arc<dyn HealthListener>,
        reply: Reply<u64>,
    },
    /// Remove a listener; the device subscription stops with its last one.
    Unwatch {
        node_id: u64,
        watcher: u64,
    },
    OtaImages(Reply<Vec<OtaImageInfo>>),
    /// Start an upload; answers with its ID.
    OtaUploadBegin {
        size: u64,
        reply: Reply<u32>,
    },
    OtaUploadChunk {
        id: u32,
        offset: u64,
        data: Vec<u8>,
        reply: Reply<()>,
    },
    OtaUploadEnd {
        id: u32,
        reply: Reply<Vec<OtaImageInfo>>,
    },
    OtaUploadDiscard {
        id: u32,
    },
    OtaDelete {
        name: String,
        reply: Reply<Vec<OtaImageInfo>>,
    },
    OtaUpdate {
        node_id: u64,
        name: String,
        progress: Option<Arc<dyn OtaListener>>,
        reply: Reply<()>,
    },
}

fn stopped() -> PeekError {
    PeekError::failed("the controller stopped")
}
