//! OTA images and updates.

use core::cell::RefCell;

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use embassy_futures::select::{select, Either};
use embassy_time::{with_timeout, Duration, Instant, Timer};

use futures_util::stream::{FuturesUnordered, StreamExt};

use log::{info, warn};

use rs_matter::crypto::Crypto;
use rs_matter::dm::clusters::ota_prov::OtaRequestorId;
use rs_matter::persist::KvBlobStore;
use rs_matter::utils::sync::blocking::Mutex;
use rs_matter::utils::sync::{MaybeSend, MaybeSendSync};

use crate::api::{display, OtaImageInfo, OtaListener, OtaProgress, OtaUpdateInfo, PeekError};
use crate::model::{DeviceHandle, DiagSet};
use crate::ota::{OtaEvent, OtaImageStore, OtaVerifier};

use super::{convert, failed, Ctx};

/// The largest upload chunk taken.
const MAX_UPLOAD_CHUNK_LEN: usize = 4 * 1024 * 1024;

/// How long a device told to apply an update may take to restart with it.
const APPLY_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// How often such a device is looked at meanwhile.
const APPLY_POLL: Duration = Duration::from_secs(15);

/// How long one look may take.
const APPLY_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// An OTA image file being uploaded in chunks: written as it arrives, and
/// checked as it is.
pub(super) struct Upload<'a, C: Crypto + 'a, O: OtaImageStore> {
    verifier: OtaVerifier<C::Hash<'a>>,
    partial: O::Partial,
    size: u64,
    received: u64,
}

/// How long an update stays listed after its last progress.
const UPDATES_KEPT: Duration = Duration::from_secs(60 * 60);

/// Follows the OTA update of a device.
pub(super) struct OtaFollower {
    node_id: u64,
    name: String,
    listener: Arc<dyn OtaListener>,
}

/// What the OTA Provider's events update, from the node's tasks, and the
/// service reads.
pub(super) type Shared<T> = Arc<Mutex<RefCell<T>>>;

/// What the OTA Provider tells of the updates, from the node's tasks.
pub(super) trait OtaEventFn: Fn(OtaEvent) + MaybeSendSync {}

impl<T: Fn(OtaEvent) + MaybeSendSync> OtaEventFn for T {}

/// The OTA events the OTA Provider tells, from the node's tasks.
pub(super) type OtaEvents = Arc<dyn OtaEventFn>;

/// A new [`Shared`] `value`.
pub(super) fn shared<T>(value: T) -> Shared<T> {
    Arc::new(Mutex::new(RefCell::new(value)))
}

/// Run `f` on the value of `shared`.
fn with<T, R>(shared: &Shared<T>, f: impl FnOnce(&mut T) -> R) -> R {
    shared.lock(|value| f(&mut value.borrow_mut()))
}

/// The latest OTA update of a device.
pub(super) struct TrackedUpdate {
    name: Option<String>,
    progress: OtaProgress,
    downloaded: Option<u64>,
    size: Option<u64>,
    started: Instant,
    last: Instant,
}

/// A device told to apply `version`, at `since`: see [`Ctx::follow_apply`].
pub(super) struct ApplyJob {
    node: OtaRequestorId,
    version: u32,
    since: Instant,
}

impl<'a, C: Crypto, S: KvBlobStore + MaybeSend, O: OtaImageStore> Ctx<'a, C, S, O> {
    pub(super) fn ota_images(&self) -> Vec<OtaImageInfo> {
        self.ota
            .store()
            .images()
            .into_iter()
            .map(convert::ota_image)
            .collect()
    }

    pub(super) fn ota_upload_begin(&self, size: u64) -> Result<u32, PeekError> {
        let crypto: &'a C = self.crypto;
        let id = self.next_upload_id.get();
        self.next_upload_id.set(id.wrapping_add(1));

        let upload = crypto
            .hash()
            .and_then(|hasher| {
                Ok(Upload {
                    verifier: OtaVerifier::new(hasher),
                    partial: self.ota.store().begin(id)?,
                    size,
                    received: 0,
                })
            })
            .map_err(|e| failed("cannot start the upload", e))?;

        self.uploads.borrow_mut().insert(id, upload);

        Ok(id)
    }

    pub(super) fn ota_upload_chunk(
        &self,
        id: u32,
        offset: u64,
        chunk: &[u8],
    ) -> Result<(), PeekError> {
        if chunk.len() > MAX_UPLOAD_CHUNK_LEN {
            return self.ota_upload_fail(id, "a chunk is too big".into());
        }

        let mut uploads = self.uploads.borrow_mut();
        let Some(upload) = uploads.get_mut(&id) else {
            return Err(no_upload(id));
        };

        let taken = if offset != upload.received {
            Err(format!(
                "expected the chunk at {}, got {offset}",
                upload.received
            ))
        } else if upload.received + chunk.len() as u64 > upload.size {
            Err("more data than announced".to_string())
        } else if let Err(e) = upload.verifier.update(chunk) {
            Err(format!("not a valid OTA image ({:?})", e.code()))
        } else if let Err(e) = self.ota.store().write(&mut upload.partial, chunk) {
            Err(format!("cannot write the image ({:?})", e.code()))
        } else {
            upload.received += chunk.len() as u64;
            Ok(())
        };

        drop(uploads);

        match taken {
            Ok(()) => Ok(()),
            Err(message) => self.ota_upload_fail(id, message),
        }
    }

    pub(super) fn ota_upload_end(&self, id: u32) -> Result<Vec<OtaImageInfo>, PeekError> {
        let Some(upload) = self.uploads.borrow_mut().remove(&id) else {
            return Err(no_upload(id));
        };

        let store = self.ota.store();

        if upload.received != upload.size {
            store.discard(upload.partial);
            return Err(PeekError::failed(format!(
                "the upload ended at {} of {} bytes",
                upload.received, upload.size
            )));
        }

        let image = match upload.verifier.finish() {
            Ok(image) => image,
            Err(e) => {
                store.discard(upload.partial);
                return Err(failed("not a valid OTA image", e));
            }
        };

        store
            .finish(upload.partial, &image)
            .map_err(|e| failed("cannot store the OTA image", e))?;
        info!(
            "service: stored OTA image {} ({})",
            image.name, image.version_str
        );

        Ok(self.ota_images())
    }

    /// Drop upload `id` and its partial file.
    pub(super) fn ota_upload_discard(&self, id: u32) {
        if let Some(upload) = self.uploads.borrow_mut().remove(&id) {
            warn!("service: dropping OTA upload {id}");
            self.ota.store().discard(upload.partial);
        }
    }

    /// Drop upload `id`, failing with `message`.
    fn ota_upload_fail(&self, id: u32, message: String) -> Result<(), PeekError> {
        self.ota_upload_discard(id);
        Err(PeekError::failed(message))
    }

    pub(super) fn ota_delete(&self, name: &str) -> Result<Vec<OtaImageInfo>, PeekError> {
        self.ota
            .store()
            .remove(name)
            .map_err(|e| failed(&format!("cannot delete OTA image {name}"), e))?;

        Ok(self.ota_images())
    }

    /// The latest OTA update of each device, of the last hour.
    pub(super) fn ota_updates(&self) -> Vec<OtaUpdateInfo> {
        let now = Instant::now();

        with(&self.ota_updates, |updates| {
            updates.retain(|_, update| now - update.last < UPDATES_KEPT);

            updates
                .iter()
                .map(|(node_id, update)| OtaUpdateInfo {
                    node_id: *node_id,
                    name: update.name.clone(),
                    progress: update.progress.clone(),
                    downloaded: update.downloaded,
                    size: update.size,
                    elapsed_secs: (now - update.started).as_secs(),
                    idle_secs: (now - update.last).as_secs(),
                })
                .collect()
        })
    }

    /// Allow the device image `name`, have `progress` follow its update, and
    /// announce us to it as its OTA Provider.
    pub(super) async fn ota_update(
        &self,
        node_id: u64,
        name: &str,
        progress: Option<Arc<dyn OtaListener>>,
    ) -> Result<(), PeekError> {
        let dev = self.lookup_device(node_id)?;

        if !self
            .ota
            .store()
            .images()
            .iter()
            .any(|image| image.name == name)
        {
            return Err(PeekError::failed(format!("no OTA image {name}")));
        }

        // Before the announcement: the device asks for the update right away.
        self.ota.allow(
            OtaRequestorId {
                fab_idx: dev.handle.fabric_index,
                node_id,
            },
            name,
        );

        with(&self.ota_followers, |followers| {
            followers.retain(|follower| follower.node_id != node_id);
            if let Some(listener) = progress {
                followers.push(OtaFollower {
                    node_id,
                    name: name.to_string(),
                    listener,
                });
            }
        });

        self.controller
            .announce_ota_provider(dev.handle)
            .await
            .map_err(|e| failed("cannot announce us to the device", e))
    }

    /// Follow the devices told to apply an update, taking new ones from
    /// `jobs`, until it closes.
    pub(super) async fn run_apply_jobs(&self, jobs: async_channel::Receiver<ApplyJob>) {
        let mut running = FuturesUnordered::new();

        loop {
            if running.is_empty() {
                // An empty `FuturesUnordered` is ready at once (with `None`).
                match jobs.recv().await {
                    Ok(job) => running.push(self.follow_apply(job)),
                    Err(_) => return,
                }
            } else {
                match select(jobs.recv(), running.next()).await {
                    Either::First(Ok(job)) => running.push(self.follow_apply(job)),
                    Either::First(Err(_)) => return,
                    Either::Second(_) => {}
                }
            }
        }
    }

    /// Follow a device told to apply an update: once it restarts, it runs the
    /// update's version, or the update failed. A device need not say it
    /// applied one, nor ever say that it did not. Its record is refreshed too.
    async fn follow_apply(&self, job: ApplyJob) {
        let ApplyJob {
            node,
            version,
            since,
        } = job;
        let handle = DeviceHandle {
            fabric_index: node.fab_idx,
            node_id: node.node_id,
        };

        loop {
            Timer::after(APPLY_POLL).await;

            let progress = with(&self.ota_updates, |updates| {
                updates
                    .get(&node.node_id)
                    .map(|update| update.progress.clone())
            });
            match progress {
                Some(OtaProgress::Applying { version: applying }) if applying == version => {}
                // As the device said.
                Some(OtaProgress::Applied { .. }) => {
                    self.refresh_identity_of(handle).await;
                    return;
                }
                // Another update since.
                _ => return,
            }

            if Instant::now() - since > APPLY_TIMEOUT {
                (self.ota_events)(OtaEvent::Failed {
                    node,
                    reason: format!(
                        "the device did not restart with version {} within {} min",
                        display::hex_u32(version),
                        APPLY_TIMEOUT.as_secs() / 60
                    ),
                });
                return;
            }

            // Up for less than since then: it restarted.
            let health = with_timeout(
                APPLY_READ_TIMEOUT,
                self.controller.read_health(handle, DiagSet::LIVE),
            )
            .await;
            let Some(up_secs) = health
                .ok()
                .and_then(Result::ok)
                .and_then(|health| health.general?.up_time_secs)
            else {
                continue;
            };
            if Duration::from_secs(up_secs) >= Instant::now() - since {
                continue;
            }

            let Some(running) = self.refresh_identity_of(handle).await else {
                continue;
            };

            (self.ota_events)(if running == version {
                OtaEvent::Applied { node, version }
            } else {
                OtaEvent::Failed {
                    node,
                    reason: format!(
                        "the device restarted with version {} still, not {}",
                        display::hex_u32(running),
                        display::hex_u32(version)
                    ),
                }
            });
            return;
        }
    }

    /// Read a device's identity afresh into its listing; answers the version
    /// it runs.
    async fn refresh_identity_of(&self, handle: DeviceHandle) -> Option<u32> {
        let ident = with_timeout(APPLY_READ_TIMEOUT, self.controller.read_basic_info(handle))
            .await
            .ok()?;
        let running = ident.fingerprint.software_version?;

        self.refresh_listed(handle.node_id, ident);

        Some(running)
    }
}

/// Hand each OTA event to the followers of that update, and track it; an
/// update that ended loses its followers. A device told to apply an update goes
/// to `apply_jobs`, to see whether it does.
// The events are `Send` and `Sync` with `send-sync` only.
#[cfg_attr(not(feature = "send-sync"), allow(clippy::arc_with_non_send_sync))]
pub(super) fn events(
    followers: Shared<Vec<OtaFollower>>,
    updates: Shared<BTreeMap<u64, TrackedUpdate>>,
    apply_jobs: async_channel::Sender<ApplyJob>,
) -> OtaEvents {
    Arc::new(move |event| {
        let event = with(&updates, |updates| not_applied(updates, event));

        if let OtaEvent::Applying { node, version } = &event {
            let _ = apply_jobs.try_send(ApplyJob {
                node: *node,
                version: *version,
                since: Instant::now(),
            });
        }

        with(&updates, |updates| track(updates, &event));

        let (node_id, image, progress) = match event {
            OtaEvent::Offered { node, image } => (
                Some(node.node_id),
                None,
                OtaProgress::Offered { name: image },
            ),
            OtaEvent::NotOffered { node } => (Some(node.node_id), None, OtaProgress::NotOffered),
            OtaEvent::Downloaded {
                image,
                offset,
                size,
            } => (
                None,
                Some(image),
                OtaProgress::Downloading {
                    percent: (offset as u128 * 100 / size.max(1) as u128) as u8,
                },
            ),
            OtaEvent::Applying { node, version } => {
                (Some(node.node_id), None, OtaProgress::Applying { version })
            }
            OtaEvent::Applied { node, version } => {
                (Some(node.node_id), None, OtaProgress::Applied { version })
            }
            OtaEvent::Failed { node, reason } => {
                (Some(node.node_id), None, OtaProgress::Failed { reason })
            }
        };

        with(&followers, |followers| {
            for follower in followers.iter() {
                let follows = match (&node_id, &image) {
                    (Some(node_id), _) => follower.node_id == *node_id,
                    (None, Some(image)) => follower.name == *image,
                    (None, None) => false,
                };
                if follows {
                    follower
                        .listener
                        .on_progress(follower.node_id, progress.clone());
                }
            }

            if let (
                Some(node_id),
                OtaProgress::Applied { .. } | OtaProgress::NotOffered | OtaProgress::Failed { .. },
            ) = (node_id, &progress)
            {
                followers.retain(|follower| follower.node_id != node_id);
            }
        })
    })
}

/// `event`, or a failure if it says a device applying an update runs another
/// version.
fn not_applied(updates: &BTreeMap<u64, TrackedUpdate>, event: OtaEvent) -> OtaEvent {
    let OtaEvent::Applied { node, version } = event else {
        return event;
    };

    match updates.get(&node.node_id).map(|update| &update.progress) {
        Some(OtaProgress::Applying { version: applying }) if *applying != version => {
            OtaEvent::Failed {
                node,
                reason: format!(
                    "the device runs version {}, not {}",
                    display::hex_u32(version),
                    display::hex_u32(*applying)
                ),
            }
        }
        _ => OtaEvent::Applied { node, version },
    }
}

fn no_upload(id: u32) -> PeekError {
    PeekError::failed(format!("no upload {id}"))
}

/// Record `event` in the latest update of its device: a download, of the
/// devices offered its image.
fn track(updates: &mut BTreeMap<u64, TrackedUpdate>, event: &OtaEvent) {
    let now = Instant::now();

    match event {
        OtaEvent::Offered { node, image } => {
            updates.insert(
                node.node_id,
                TrackedUpdate {
                    name: Some(image.clone()),
                    progress: OtaProgress::Offered {
                        name: image.clone(),
                    },
                    downloaded: None,
                    size: None,
                    started: now,
                    last: now,
                },
            );
        }
        OtaEvent::NotOffered { node } => {
            updates.insert(
                node.node_id,
                TrackedUpdate {
                    name: None,
                    progress: OtaProgress::NotOffered,
                    downloaded: None,
                    size: None,
                    started: now,
                    last: now,
                },
            );
        }
        OtaEvent::Downloaded {
            image,
            offset,
            size,
        } => {
            let downloading = updates.values_mut().filter(|update| {
                update.name.as_ref() == Some(image)
                    && matches!(
                        update.progress,
                        OtaProgress::Offered { .. } | OtaProgress::Downloading { .. }
                    )
            });
            for update in downloading {
                update.progress = OtaProgress::Downloading {
                    percent: (*offset as u128 * 100 / (*size).max(1) as u128) as u8,
                };
                update.downloaded = Some(*offset);
                update.size = Some(*size);
                update.last = now;
            }
        }
        OtaEvent::Applying { node, version } | OtaEvent::Applied { node, version } => {
            let progress = match event {
                OtaEvent::Applying { .. } => OtaProgress::Applying { version: *version },
                _ => OtaProgress::Applied { version: *version },
            };
            let update = updates.entry(node.node_id).or_insert(TrackedUpdate {
                name: None,
                progress: progress.clone(),
                downloaded: None,
                size: None,
                started: now,
                last: now,
            });
            update.progress = progress;
            update.last = now;
        }
        OtaEvent::Failed { node, reason } => {
            if let Some(update) = updates.get_mut(&node.node_id) {
                update.progress = OtaProgress::Failed {
                    reason: reason.clone(),
                };
                update.last = now;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use core::num::NonZeroU8;

    use super::*;

    const IMAGE: &str = "fff2-8002-257-f344bc.ota";

    fn node(node_id: u64) -> OtaRequestorId {
        OtaRequestorId {
            fab_idx: NonZeroU8::new(1).unwrap(),
            node_id,
        }
    }

    #[test]
    fn a_download_is_tracked_for_the_devices_offered_its_image() {
        let mut updates = BTreeMap::new();

        track(
            &mut updates,
            &OtaEvent::Offered {
                node: node(7),
                image: IMAGE.to_string(),
            },
        );
        track(&mut updates, &OtaEvent::NotOffered { node: node(8) });
        track(
            &mut updates,
            &OtaEvent::Downloaded {
                image: IMAGE.to_string(),
                offset: 250,
                size: 1000,
            },
        );

        assert_eq!(
            updates[&7].progress,
            OtaProgress::Downloading { percent: 25 }
        );
        assert_eq!(
            (updates[&7].downloaded, updates[&7].size),
            (Some(250), Some(1000))
        );
        assert_eq!(updates[&8].progress, OtaProgress::NotOffered);

        track(
            &mut updates,
            &OtaEvent::Applied {
                node: node(7),
                version: 257,
            },
        );
        assert_eq!(updates[&7].progress, OtaProgress::Applied { version: 257 });
        assert_eq!(updates[&7].name.as_deref(), Some(IMAGE));
    }

    #[test]
    fn a_device_reporting_another_version_failed_its_update() {
        let mut updates = BTreeMap::new();
        track(
            &mut updates,
            &OtaEvent::Applying {
                node: node(7),
                version: 0x0001_06FF,
            },
        );

        let event = not_applied(
            &updates,
            OtaEvent::Applied {
                node: node(7),
                version: 0x0001_05FF,
            },
        );
        assert!(matches!(event, OtaEvent::Failed { .. }));

        track(&mut updates, &event);
        assert!(matches!(updates[&7].progress, OtaProgress::Failed { .. }));
        assert!(!updates[&7].progress.in_progress());

        // The version it was told to apply is an update applied.
        assert!(matches!(
            not_applied(
                &BTreeMap::from([(
                    7,
                    TrackedUpdate {
                        name: None,
                        progress: OtaProgress::Applying {
                            version: 0x0001_06FF
                        },
                        downloaded: None,
                        size: None,
                        started: Instant::now(),
                        last: Instant::now(),
                    }
                )]),
                OtaEvent::Applied {
                    node: node(7),
                    version: 0x0001_06FF,
                },
            ),
            OtaEvent::Applied { .. }
        ));
    }

    #[test]
    fn the_time_left_follows_the_pace_so_far() {
        let update = OtaUpdateInfo {
            node_id: 7,
            name: Some(IMAGE.to_string()),
            progress: OtaProgress::Downloading { percent: 25 },
            downloaded: Some(250),
            size: Some(1000),
            elapsed_secs: 60,
            idle_secs: 1,
        };

        assert!(update.in_progress());
        assert_eq!(update.remaining_secs(), Some(180));
    }
}
