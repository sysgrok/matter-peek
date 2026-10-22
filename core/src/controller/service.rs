//! The service behind [`MatterPeek`](crate::api::MatterPeek): it owns the
//! Matter stack and runs the operations of its handles.
//!
//! One task, for the controller's lifetime: it loads the persisted controller
//! fabric, ensures the CA, then runs side by side the transport pump, the mDNS
//! backend (for operational resolves), our own node (OTA Provider, BDX,
//! subscription reports), the operations (one at a time, plus a periodic sync)
//! and the health subscriptions (detached, so they never block operations).
//!
//! It is `no_std`: the host hands it the stack, the crypto, the KV store, the OTA
//! image store and the UDP network (see [`crate::host`] for this host's).

use core::cell::{Cell, RefCell};
use core::num::NonZeroU8;
use core::pin::pin;
use core::sync::atomic::AtomicBool;

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use embassy_futures::select::{select, select4, Either, Either4};
use embassy_time::{Duration, Instant, Timer};

use log::{error, info, warn};

use rs_matter::crypto::Crypto;
use rs_matter::error::Error;
use rs_matter::persist::KvBlobStore;
use rs_matter::transport::network::{NetworkReceive, NetworkSend, NoNetwork};
use rs_matter::Matter;

use crate::api::{Op, PeekConfig, PeekError, Service};
use crate::ca::{CaTier, ControllerCa};
use crate::matter::{Controller, FabricIdentity};
use crate::mdns::Mdns;
use crate::model::StoredDevice;
use crate::node;
use crate::ota::{OtaEvent, OtaImageStore, OtaProvider};
use crate::persist::{ControllerStore, KvBuf};
use crate::subscribe::Reports;

use ota::{OtaFollower, Upload};
use watch::{SubJob, Watch};

mod commission;
mod convert;
mod devices;
mod netconfig;
mod ota;
mod watch;

/// The first device NodeID the controller hands out (see
/// [`ControllerStore::allocate_node_id`]).
const FIRST_DEVICE_NODE_ID: u64 = 112234;

/// How long after startup the first sync runs (the mDNS backend comes up first).
const FIRST_SYNC_DELAY_SECS: u64 = 5;
/// How often the device list is synced with our fabric.
const SYNC_PERIOD_SECS: u64 = 300;

/// What the service runs on, provided by the host.
pub struct Platform<'a, C, S, O, N, M> {
    /// The stack, not started yet: the controller's device details are the
    /// host's to choose.
    pub matter: &'a mut Matter<'a>,
    pub crypto: C,
    /// The KV store of the controller's fabric and device records.
    pub kv: S,
    /// Its scratch buffer (see [`kv_buf`](crate::persist::kv_buf)).
    pub kv_buf: &'a KvBuf,
    /// The OTA images.
    pub ota: O,
    /// The UDP network, for Matter.
    pub network: N,
    /// The mDNS backend.
    pub mdns: M,
}

/// The state every operation works on.
struct Ctx<'a, C: Crypto, S, O: OtaImageStore> {
    matter: &'a Matter<'a>,
    crypto: &'a C,
    controller: Controller<'a, C>,
    ca: ControllerCa,
    fab_idx: NonZeroU8,
    store: ControllerStore<'a, S>,
    /// Set by `MatterPeek::cancel_commission`, checked by the commission.
    cancel: Arc<AtomicBool>,
    /// The live health subscriptions, by device, each shared by its listeners.
    subs: RefCell<BTreeMap<u64, Watch>>,
    next_watcher: Cell<u64>,
    /// New subscriptions, for the subscription driver to run.
    sub_jobs: async_channel::Sender<SubJob>,
    /// Routes the reports of the live subscriptions to them.
    reports: Reports,
    /// Our OTA Provider.
    ota: OtaProvider<O, Box<dyn Fn(OtaEvent)>>,
    /// Who follows which OTA update.
    ota_followers: Rc<RefCell<Vec<OtaFollower>>>,
    /// The OTA image uploads in progress, by ID.
    uploads: RefCell<BTreeMap<u32, Upload<'a, C, O>>>,
    next_upload_id: Cell<u32>,
    /// The devices that are reachable, as far as we know; `None` before the
    /// first sync.
    online: RefCell<Option<BTreeSet<u64>>>,
    /// The listed devices we keep no record of (not remembered): commissioned
    /// without `remember`, or found by a sync, since the controller started.
    transient: RefCell<Vec<StoredDevice>>,
    /// The names of Thread networks, by extended PAN ID, as border routers
    /// advertise them.
    thread_names: RefCell<BTreeMap<u64, String>>,
    /// When the border routers were last asked for names they did not have.
    thread_names_missed: Cell<Option<Instant>>,
    /// The Bluetooth side of the transport, for commissioning over BLE.
    #[cfg(feature = "ble")]
    btp: &'a rs_matter::transport::network::btp::Btp,
}

impl<'a, C: Crypto, S: KvBlobStore, O: OtaImageStore> Ctx<'a, C, S, O> {
    /// Run the operations one at a time, syncing periodically if asked to,
    /// until every handle is dropped.
    async fn run_ops(&self, service: Service, periodic_sync: bool) {
        let after = |secs| {
            if periodic_sync {
                Instant::now() + Duration::from_secs(secs)
            } else {
                Instant::MAX
            }
        };

        let mut next_sync = after(FIRST_SYNC_DELAY_SECS);

        loop {
            match select(service.ops.recv(), Timer::at(next_sync)).await {
                Either::First(Ok(op)) => {
                    if matches!(op, Op::Sync(_)) {
                        next_sync = after(SYNC_PERIOD_SECS);
                    }

                    self.run_op(op).await;
                }
                Either::First(Err(_)) => return,
                Either::Second(()) => {
                    if let Err(e) = self.sync_devices(false).await {
                        warn!("sync failed: {e:?}");
                    }

                    next_sync = after(SYNC_PERIOD_SECS);
                }
            }
        }
    }

    /// Run one operation and send its result. A handle that went away meanwhile
    /// does not care for it.
    async fn run_op(&self, op: Op) {
        match op {
            Op::Discover {
                discriminator,
                setup_code,
                max,
                reply,
            } => {
                let found = self
                    .discover(discriminator, setup_code.as_deref(), max)
                    .await;
                let _ = reply.send(found).await;
            }
            Op::ReadQrImage { image, reply } => {
                let _ = reply.send(read_qr_image(&image)).await;
            }
            Op::Commission {
                via,
                setup_code,
                remember,
                progress,
                reply,
            } => {
                let commissioned = self
                    .commission(&via, &setup_code, remember, progress.as_deref())
                    .await;
                let _ = reply.send(commissioned).await;
            }
            Op::ListDevices(reply) => {
                let _ = reply.send(self.list_devices(None)).await;
            }
            Op::Assistant(reply) => {
                let model = self
                    .store
                    .load_assistant()
                    .map_err(|e| failed("cannot read the assistant's model", e));
                let _ = reply.send(model).await;
            }
            Op::SetAssistant { model, reply } => {
                let saved = self
                    .store
                    .save_assistant(model.as_ref())
                    .map_err(|e| failed("cannot keep the assistant's model", e));
                let _ = reply.send(saved).await;
            }
            Op::Sync(reply) => {
                let _ = reply.send(self.sync().await).await;
            }
            Op::Remember { node_id, reply } => {
                let _ = reply.send(self.remember(node_id)).await;
            }
            Op::SetTags {
                node_id,
                tags,
                reply,
            } => {
                let _ = reply.send(self.set_tags(node_id, &tags)).await;
            }
            Op::Decommission {
                node_id,
                force,
                reply,
            } => {
                let _ = reply.send(self.decommission(node_id, force).await).await;
            }
            Op::PeekDevice { node_id, reply } => {
                let _ = reply.send(self.peek_device(node_id).await).await;
            }
            Op::ReadNetwork { node_id, reply } => {
                let _ = reply.send(self.read_network(node_id).await).await;
            }
            Op::ChangeNetwork {
                node_id,
                endpoint,
                change,
                reply,
            } => {
                let changed = self.change_network(node_id, endpoint, change).await;
                let _ = reply.send(changed).await;
            }
            Op::RetrieveLogs {
                node_id,
                intent,
                reply,
            } => {
                let _ = reply.send(self.retrieve_logs(node_id, intent).await).await;
            }
            Op::Identify {
                node_id,
                secs,
                reply,
            } => {
                let _ = reply.send(self.identify(node_id, secs).await).await;
            }
            Op::Watch {
                node_id,
                listener,
                reply,
            } => {
                let _ = reply.send(self.watch(node_id, listener).await).await;
            }
            Op::Unwatch { node_id, watcher } => self.unwatch(node_id, watcher),
            Op::OtaImages(reply) => {
                let _ = reply.send(Ok(self.ota_images())).await;
            }
            Op::OtaUploadBegin { size, reply } => {
                let _ = reply.send(self.ota_upload_begin(size)).await;
            }
            Op::OtaUploadChunk {
                id,
                offset,
                data,
                reply,
            } => {
                let _ = reply.send(self.ota_upload_chunk(id, offset, &data)).await;
            }
            Op::OtaUploadEnd { id, reply } => {
                let _ = reply.send(self.ota_upload_end(id)).await;
            }
            Op::OtaUploadDiscard { id } => self.ota_upload_discard(id),
            Op::OtaDelete { name, reply } => {
                let _ = reply.send(self.ota_delete(&name)).await;
            }
            Op::OtaUpdate {
                node_id,
                name,
                progress,
                reply,
            } => {
                let _ = reply
                    .send(self.ota_update(node_id, &name, progress).await)
                    .await;
            }
        }
    }

    /// The listed devices: the remembered ones, then the others.
    fn listed_devices(&self) -> Vec<StoredDevice> {
        let mut devices = self.store.load_devices().unwrap_or_default();
        devices.extend(self.transient.borrow().iter().cloned());

        devices
    }

    /// Find a listed device by NodeID.
    fn lookup_device(&self, node_id: u64) -> Result<StoredDevice, PeekError> {
        self.listed_devices()
            .into_iter()
            .find(|d| d.handle.node_id == node_id)
            .ok_or(PeekError::NoDevice { node_id })
    }

    /// Note whether `node_id` is online, once a sync has run.
    fn set_online(&self, node_id: u64, online: bool) {
        if let Some(nodes) = self.online.borrow_mut().as_mut() {
            if online {
                nodes.insert(node_id);
            } else {
                nodes.remove(&node_id);
            }
        }
    }
}

impl Service {
    /// Run the controller on `platform`, until every handle to it is dropped.
    pub async fn run<'a, C, S, O, N, M>(
        self,
        platform: Platform<'a, C, S, O, N, M>,
        config: &PeekConfig,
    ) where
        C: Crypto,
        S: KvBlobStore,
        O: OtaImageStore,
        N: NetworkSend + NetworkReceive + Copy,
        M: Mdns,
    {
        run(self, platform, config).await
    }
}

/// See [`Service::run`].
async fn run<'a, C, S, O, N, M>(
    service: Service,
    platform: Platform<'a, C, S, O, N, M>,
    config: &PeekConfig,
) where
    C: Crypto,
    S: KvBlobStore,
    O: OtaImageStore,
    N: NetworkSend + NetworkReceive + Copy,
    M: Mdns,
{
    let Platform {
        matter,
        crypto,
        kv,
        kv_buf,
        ota,
        network,
        mut mdns,
    } = platform;

    let store = ControllerStore::new(kv, kv_buf);
    if let Err(e) = matter.startup(store.kv()) {
        error!("service: startup failed: {e:?} (continuing with fresh state)");
    }
    let matter: &Matter = matter;

    let (ca, fab_idx) = match ControllerCa::ensure(
        &crypto,
        matter,
        &store,
        FabricIdentity::default(),
        CaTier::RcacDirect,
    ) {
        Ok(v) => v,
        Err(e) => {
            error!("service: CA init failed: {e:?}");
            refuse_all(
                service,
                PeekError::failed("the controller CA cannot be set up"),
            )
            .await;
            return;
        }
    };

    let controller = Controller::new(matter, &crypto);

    #[cfg(feature = "ble")]
    let btp = rs_matter::transport::network::btp::Btp::new();

    // Our OTA Provider: the devices on our fabric may ask it for updates.
    if let Err(e) = node::allow_ota_requestors(matter, &store, fab_idx) {
        warn!("service: cannot open our OTA Provider to the devices: {e:?}");
    }

    let ota_followers = Rc::new(RefCell::new(Vec::new()));
    let ota = OtaProvider::new(ota, ota::events(ota_followers.clone()));

    let (sub_jobs, sub_jobs_rx) = async_channel::unbounded();

    let ctx = Ctx {
        matter,
        crypto: &crypto,
        controller,
        ca,
        fab_idx,
        store,
        cancel: service.cancel.clone(),
        subs: RefCell::new(BTreeMap::new()),
        next_watcher: Cell::new(1),
        sub_jobs,
        reports: Reports::new(),
        ota,
        ota_followers,
        uploads: RefCell::new(BTreeMap::new()),
        next_upload_id: Cell::new(1),
        online: RefCell::new(None),
        transient: RefCell::new(Vec::new()),
        thread_names: RefCell::new(BTreeMap::new()),
        thread_names_missed: Cell::new(None),
        #[cfg(feature = "ble")]
        btp: &btp,
    };

    #[cfg(feature = "ble")]
    let (send, recv) = {
        use rs_matter::transport::network::{Address, ChainedNetwork};

        (
            ChainedNetwork::new(Address::is_udp, network, &btp),
            ChainedNetwork::new(Address::is_udp, network, &btp),
        )
    };
    #[cfg(not(feature = "ble"))]
    let (send, recv) = (network, network);

    let mut pump = pin!(matter.run(&crypto, send, recv, NoNetwork));
    let mut ops = pin!(ctx.run_ops(service, config.periodic_sync));
    let mut subs = pin!(ctx.run_subscriptions(sub_jobs_rx));
    // Operational resolves (re-addressing a listed device over CASE) need it;
    // without it the rest still works.
    let mut mdns = pin!(async {
        if let Err(e) = mdns.run(matter, &crypto).await {
            error!("service: mDNS backend exited: {e:?}");
        }
        core::future::pending::<()>().await
    });
    // What devices reach on us: our OTA Provider and the subscription reports.
    let mut node = pin!(ctx.controller.run_node(&ctx.reports, &ctx.ota));

    match select4(&mut pump, &mut ops, &mut subs, select(&mut mdns, &mut node)).await {
        Either4::First(r) => error!("service: transport pump exited: {r:?}"),
        Either4::Second(()) => info!("service: every handle dropped, exiting"),
        Either4::Third(()) => info!("service: subscription driver exited"),
        Either4::Fourth(Either::First(())) => {}
        Either4::Fourth(Either::Second(r)) => error!("service: controller node exited: {r:?}"),
    }
}

/// Read the setup code out of a picture of a QR code.
fn read_qr_image(image: &[u8]) -> Result<String, PeekError> {
    #[cfg(feature = "std")]
    return crate::host::read_qr_image(image);

    #[cfg(not(feature = "std"))]
    {
        let _ = image;
        Err(PeekError::failed("this build cannot read pictures"))
    }
}

/// `context: <what the error means> (<its code>)`, as a failure.
fn failed(context: &str, e: Error) -> PeekError {
    PeekError::failed(format!("{context}: {}", crate::explain::describe(e.code())))
}

/// Answer every operation with `error`, until every handle is dropped.
pub(crate) async fn refuse_all(service: Service, error: PeekError) {
    while let Ok(op) = service.ops.recv().await {
        let error = error.clone();

        match op {
            Op::Discover { reply, .. } => refuse(reply, error).await,
            Op::ReadQrImage { reply, .. } => refuse(reply, error).await,
            Op::Commission { reply, .. } => refuse(reply, error).await,
            Op::ListDevices(reply) | Op::Sync(reply) => refuse(reply, error).await,
            Op::Remember { reply, .. }
            | Op::SetTags { reply, .. }
            | Op::Decommission { reply, .. } => refuse(reply, error).await,
            Op::PeekDevice { reply, .. } => refuse(reply, error).await,
            Op::ReadNetwork { reply, .. } | Op::ChangeNetwork { reply, .. } => {
                refuse(reply, error).await
            }
            Op::RetrieveLogs { reply, .. } => refuse(reply, error).await,
            Op::Identify { reply, .. } | Op::OtaUpdate { reply, .. } => refuse(reply, error).await,
            Op::Watch { reply, .. } => refuse(reply, error).await,
            Op::OtaImages(reply) | Op::OtaUploadEnd { reply, .. } | Op::OtaDelete { reply, .. } => {
                refuse(reply, error).await
            }
            Op::OtaUploadBegin { reply, .. } => refuse(reply, error).await,
            Op::OtaUploadChunk { reply, .. } => refuse(reply, error).await,
            Op::Assistant(reply) => refuse(reply, error).await,
            Op::SetAssistant { reply, .. } => refuse(reply, error).await,
            Op::Unwatch { .. } | Op::OtaUploadDiscard { .. } => {}
        }
    }
}

async fn refuse<T>(reply: crate::api::Reply<T>, error: PeekError) {
    let _ = reply.send(Err(error)).await;
}
