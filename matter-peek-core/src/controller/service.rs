//! The service behind [`MatterPeek`](crate::api::MatterPeek): it owns the
//! Matter stack and runs the operations of its handles.
//!
//! One task, for the controller's lifetime: it loads the persisted controller
//! fabric, ensures the CA, then runs side by side the transport pump, the mDNS
//! backend (for operational resolves), our own node (OTA Provider, BDX,
//! subscription reports), the operations (side by side unless they conflict,
//! see [`locks`], plus a periodic sync) and the health subscriptions (detached,
//! so they never block operations).
//!
//! It is `no_std`: the host hands it the stack, the crypto, the KV store, the OTA
//! image store and the UDP network (see [`crate::host`] for this host's).

use core::cell::{Cell, RefCell};
use core::future::Future;
use core::pin::{pin, Pin};
use core::sync::atomic::AtomicBool;

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use embassy_futures::select::{select, select3, select4, Either, Either3, Either4};
use embassy_time::{Duration, Instant, Timer};

use futures_util::stream::{FuturesUnordered, StreamExt};

use log::{error, info, warn};

use rs_matter::crypto::Crypto;
use rs_matter::error::Error;
use rs_matter::persist::KvBlobStore;
use rs_matter::transport::network::{NetworkReceive, NetworkSend, NoNetwork};
use rs_matter::Matter;

use crate::api::{
    BorderRouter, DeviceInfo, DevicesListener, FabricRef, ModelScope, Op, PeekConfig, PeekError,
    Service,
};
use crate::fabrics::OurFabrics;
use crate::matter::Controller;
use crate::mdns::Mdns;
use crate::model::StoredDevice;
use crate::node;
use crate::ota::{OtaEvent, OtaImageStore, OtaProvider};
use crate::persist::{ControllerStore, KvBuf};
use crate::subscribe::{Coverage, Gate, Reports};

use locks::{Key, Locks, Mode};
use ota::{OtaFollower, TrackedUpdate, Upload};
use watch::{SubJob, Watch};
use window::OpenedWindow;

mod advertised;
mod commission;
mod convert;
mod datamodel;
mod devices;
mod fabrics;
mod locks;
mod mesh;
mod netconfig;
mod ota;
mod time;
mod watch;
mod window;

/// The first device NodeID the controller hands out (see
/// [`ControllerStore::allocate_node_id`]).
const FIRST_DEVICE_NODE_ID: u64 = 112234;

/// How long after startup the first sync runs (the mDNS backend comes up first).
const FIRST_SYNC_DELAY_SECS: u64 = 5;
/// How often the device list is synced with our fabric.
const SYNC_PERIOD_SECS: u64 = 300;

/// How many devices on Thread prime a data model subscription at once.
const MODEL_PRIMINGS_OVER_THREAD: usize = 2;

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

/// A started operation, borrowing the service's state.
type Run<'r> = Pin<Box<dyn Future<Output = ()> + 'r>>;

/// The state every operation works on.
struct Ctx<'a, C: Crypto, S, O: OtaImageStore> {
    matter: &'a Matter<'a>,
    crypto: &'a C,
    controller: Controller<'a, C>,
    /// Our fabrics, and the default.
    fabrics: RefCell<OurFabrics>,
    store: ControllerStore<'a, S>,
    /// Which operations may run now.
    locks: Locks,
    /// Set while a commission runs: one at a time.
    commissioning: Cell<bool>,
    /// The node ID the running commission gives its device, which only the
    /// commission lists.
    commissioned_node: Cell<Option<u64>>,
    /// Set by `MatterPeek::cancel_commission`, checked by the commission.
    cancel: Arc<AtomicBool>,
    /// The live health subscriptions, by device, each shared by its listeners.
    subs: RefCell<BTreeMap<u64, Watch>>,
    next_watcher: Cell<u64>,
    /// New subscriptions, for the subscription driver to run.
    sub_jobs: async_channel::Sender<SubJob>,
    /// Routes the reports of the live subscriptions to them.
    reports: Reports,
    /// Lets a few subscriptions over Thread at a time prime a data model.
    model_gate: Gate,
    /// Our OTA Provider.
    ota: OtaProvider<O, Box<dyn Fn(OtaEvent)>>,
    /// What it tells of the updates, and what the service finds out itself.
    ota_events: Rc<dyn Fn(OtaEvent)>,
    /// Who follows which OTA update.
    ota_followers: Rc<RefCell<Vec<OtaFollower>>>,
    /// The latest OTA update of each device, by node ID.
    ota_updates: Rc<RefCell<BTreeMap<u64, TrackedUpdate>>>,
    /// The commissioning windows we opened, by node ID.
    windows: RefCell<BTreeMap<u64, OpenedWindow>>,
    /// The OTA image uploads in progress, by ID.
    uploads: RefCell<BTreeMap<u32, Upload<'a, C, O>>>,
    next_upload_id: Cell<u32>,
    /// The devices that are reachable, as far as we know; `None` before the
    /// first sync.
    online: RefCell<Option<BTreeSet<u64>>>,
    /// The listed devices we keep no record of (not remembered): commissioned
    /// without `remember`, or found by a sync, since the controller started.
    transient: RefCell<Vec<StoredDevice>>,
    /// Who follows the device list, by watcher ID.
    devices_listeners: RefCell<Vec<(u64, Arc<dyn DevicesListener>)>>,
    /// The device list as they were last told it.
    devices_told: RefCell<Option<Vec<DeviceInfo>>>,
    /// The names of Thread networks, by extended PAN ID, as border routers
    /// advertise them.
    thread_names: RefCell<BTreeMap<u64, String>>,
    /// When the border routers were last asked for names they did not have.
    thread_names_missed: Cell<Option<Instant>>,
    /// The border routers seen lately, by name, and when.
    border_routers: RefCell<BTreeMap<String, (BorderRouter, Instant)>>,
    /// The Bluetooth side of the transport, for commissioning over BLE.
    #[cfg(feature = "ble")]
    btp: &'a rs_matter::transport::network::btp::Btp,
}

impl<'a, C: Crypto, S: KvBlobStore, O: OtaImageStore> Ctx<'a, C, S, O> {
    /// Run the operations, side by side unless they conflict (see [`Locks`]),
    /// syncing periodically if asked to, until every handle is dropped.
    async fn run_ops(&self, service: Service, periodic_sync: bool) {
        let after = |secs| {
            if periodic_sync {
                Instant::now() + Duration::from_secs(secs)
            } else {
                Instant::MAX
            }
        };

        let mut next_sync = after(FIRST_SYNC_DELAY_SECS);
        let mut running = FuturesUnordered::<Run<'_>>::new();

        loop {
            let done = async {
                // An empty `FuturesUnordered` is ready at once (with `None`).
                if running.is_empty() {
                    core::future::pending::<()>().await;
                }
                running.next().await;
            };

            match select3(service.ops.recv(), Timer::at(next_sync), done).await {
                Either3::First(Ok(op)) => {
                    if matches!(op, Op::Sync { .. }) {
                        next_sync = after(SYNC_PERIOD_SECS);
                    }

                    match self.start(op).await {
                        Some(run) => running.push(run),
                        None => self.tell_devices(),
                    }
                }
                Either3::First(Err(_)) => return,
                Either3::Second(()) => {
                    next_sync = after(SYNC_PERIOD_SECS);

                    // A sync queued already does.
                    if !self.locks.busy(Key::Sync) {
                        let guard = self.locks.queue(&self.sync_keys());
                        running.push(Box::pin(async move {
                            guard.ready().await;
                            if let Err(e) = self.sync_devices(false).await {
                                warn!("sync failed: {e:?}");
                            }
                        }));
                    }
                }
                Either3::Third(()) => self.tell_devices(),
            }
        }
    }

    /// Start `op`. One without device I/O is answered at once; any other comes
    /// back, to run once the [`Locks`] let it.
    async fn start(&self, op: Op) -> Option<Run<'_>> {
        let op = match op {
            Op::Commission { reply, .. } if self.commissioning.get() => {
                let busy =
                    PeekError::failed("another commission is running: try again once it is done");
                let _ = reply.send(Err(busy)).await;
                return None;
            }
            op => op,
        };

        let Some(keys) = self.keys(&op) else {
            self.run_op(op).await;
            return None;
        };

        let commission = matches!(op, Op::Commission { .. });
        if commission {
            self.commissioning.set(true);
        }

        let guard = self.locks.queue(&keys);

        Some(Box::pin(async move {
            guard.ready().await;
            self.run_op(op).await;

            if commission {
                self.commissioning.set(false);
                self.commissioned_node.set(None);
            }
        }))
    }

    /// The keys `op` runs under, or `None` if it does no device I/O.
    fn keys(&self, op: &Op) -> Option<Vec<(Key, Mode)>> {
        let keys = match op {
            Op::ReadQrImage { .. }
            | Op::Fabrics(_)
            | Op::CreateFabric { .. }
            | Op::RenameFabric { .. }
            | Op::ListDevices { .. }
            | Op::Assistant(_)
            | Op::SetAssistant { .. }
            | Op::Remember { .. }
            | Op::SetTags { .. }
            | Op::Unwatch { .. }
            | Op::WatchDevices { .. }
            | Op::UnwatchDevices { .. }
            | Op::OtaImages(_)
            | Op::OtaUpdates(_)
            | Op::OtaUploadBegin { .. }
            | Op::OtaUploadChunk { .. }
            | Op::OtaUploadEnd { .. }
            | Op::OtaUploadDiscard { .. }
            | Op::OtaDelete { .. } => return None,
            Op::Discover { .. } | Op::AdvertisedDevices(_) => Vec::new(),
            Op::Sync { .. } => self.sync_keys(),
            // The fabric must not go meanwhile.
            Op::Commission { fabric, .. }
            | Op::SetDefaultFabric { fabric, .. }
            | Op::ReadThreadMesh { fabric, .. } => self.fabric_keys(fabric, Mode::Read),
            Op::DeleteFabric { fabric, .. } => self.fabric_keys(fabric, Mode::Write),
            Op::PeekDevice { node_id, .. }
            | Op::ReadModel { node_id, .. }
            | Op::ReadEvents { node_id, .. }
            | Op::ReadNetwork { node_id, .. }
            | Op::RetrieveLogs { node_id, .. }
            | Op::Identify { node_id, .. }
            | Op::Watch { node_id, .. }
            | Op::WatchModel { node_id, .. }
            | Op::Window { node_id, .. }
            | Op::DeviceTime { node_id, .. } => self.device_keys(*node_id, Mode::Read),
            Op::Decommission { node_id, .. }
            | Op::WriteAttribute { node_id, .. }
            | Op::InvokeCommand { node_id, .. }
            | Op::ChangeNetwork { node_id, .. }
            | Op::RemoveDeviceFabric { node_id, .. }
            | Op::OpenWindow { node_id, .. }
            | Op::CloseWindow { node_id, .. }
            | Op::SetDeviceTime { node_id, .. }
            | Op::OtaUpdate { node_id, .. } => self.device_keys(*node_id, Mode::Write),
        };

        Some(keys)
    }

    /// A listed device, used as `mode`, and its fabric, read.
    fn device_keys(&self, node_id: u64, mode: Mode) -> Vec<(Key, Mode)> {
        let mut keys = alloc::vec![(Key::Node(node_id), mode)];
        if let Ok(device) = self.lookup_device(node_id) {
            keys.push((Key::Fabric(device.handle.fabric_index.get()), Mode::Read));
        }

        keys
    }

    /// The fabrics `fabric` names, used as `mode`.
    fn fabric_keys(&self, fabric: &FabricRef, mode: Mode) -> Vec<(Key, Mode)> {
        self.our_fabrics(fabric)
            .map(|fabrics| {
                fabrics
                    .iter()
                    .map(|fabric| (Key::Fabric(fabric.fab_idx.get()), mode))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// One sync at a time, reading every fabric of ours.
    fn sync_keys(&self) -> Vec<(Key, Mode)> {
        let mut keys = self.fabric_keys(&FabricRef::All, Mode::Read);
        keys.push((Key::Sync, Mode::Write));

        keys
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
            Op::AdvertisedDevices(reply) => {
                let _ = reply.send(self.advertised_devices().await).await;
            }
            Op::ReadModel {
                node_id,
                endpoint,
                cluster,
                reply,
            } => {
                let model = self.read_model(node_id, endpoint, cluster.as_deref()).await;
                let _ = reply.send(model).await;
            }
            Op::ReadEvents {
                node_id,
                endpoint,
                cluster,
                reply,
            } => {
                let events = self
                    .read_events(node_id, endpoint, cluster.as_deref())
                    .await;
                let _ = reply.send(events).await;
            }
            Op::WriteAttribute {
                node_id,
                endpoint,
                cluster,
                attribute,
                value,
                reply,
            } => {
                let written = self
                    .write_attribute(node_id, endpoint, &cluster, &attribute, &value)
                    .await;
                let _ = reply.send(written).await;
            }
            Op::InvokeCommand {
                node_id,
                endpoint,
                cluster,
                command,
                fields,
                reply,
            } => {
                let outcome = self
                    .invoke_command(node_id, endpoint, &cluster, &command, &fields)
                    .await;
                let _ = reply.send(outcome).await;
            }
            Op::ReadQrImage { image, reply } => {
                let _ = reply.send(read_qr_image(&image)).await;
            }
            Op::Commission {
                via,
                setup_code,
                remember,
                fabric,
                progress,
                reply,
            } => {
                let commissioned = self
                    .commission(&via, &setup_code, remember, &fabric, progress.as_deref())
                    .await;
                let _ = reply.send(commissioned).await;
            }
            Op::Fabrics(reply) => {
                let _ = reply.send(Ok(self.fabric_infos())).await;
            }
            Op::CreateFabric { name, reply } => {
                let _ = reply.send(self.create_fabric(&name)).await;
            }
            Op::RenameFabric {
                fabric,
                name,
                reply,
            } => {
                let _ = reply.send(self.rename_fabric(&fabric, &name)).await;
            }
            Op::SetDefaultFabric { fabric, reply } => {
                let _ = reply.send(self.set_default_fabric(&fabric)).await;
            }
            Op::DeleteFabric {
                fabric,
                deletion,
                reply,
            } => {
                let _ = reply
                    .send(self.delete_fabric(&fabric, deletion).await)
                    .await;
            }
            Op::ListDevices {
                fabric,
                filter,
                reply,
            } => {
                let list = self.list_matching(&fabric, filter.as_deref().unwrap_or_default());
                let _ = reply.send(list).await;
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
            Op::Sync { fabric, reply } => {
                let _ = reply.send(self.sync(&fabric).await).await;
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
            Op::ReadThreadMesh { fabric, reply } => {
                let _ = reply.send(self.read_thread_mesh(&fabric).await).await;
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
            Op::WatchModel {
                node_id,
                scope,
                listener,
                reply,
            } => {
                let coverage = match scope {
                    ModelScope::Application => Coverage::Application,
                    ModelScope::All => Coverage::All,
                };
                let _ = reply
                    .send(self.watch_model(node_id, coverage, listener).await)
                    .await;
            }
            Op::Unwatch { node_id, watcher } => self.unwatch(node_id, watcher),
            Op::WatchDevices { listener, reply } => {
                let _ = reply.send(self.watch_devices(listener)).await;
            }
            Op::UnwatchDevices { watcher } => self.unwatch_devices(watcher),
            Op::OtaImages(reply) => {
                let _ = reply.send(Ok(self.ota_images())).await;
            }
            Op::OtaUpdates(reply) => {
                let _ = reply.send(Ok(self.ota_updates())).await;
            }
            Op::RemoveDeviceFabric {
                node_id,
                fabric_index,
                reply,
            } => {
                let _ = reply
                    .send(self.remove_device_fabric(node_id, fabric_index).await)
                    .await;
            }
            Op::OpenWindow {
                node_id,
                timeout_secs,
                reply,
            } => {
                let _ = reply
                    .send(self.open_window(node_id, timeout_secs).await)
                    .await;
            }
            Op::CloseWindow { node_id, reply } => {
                let _ = reply.send(self.close_window(node_id).await).await;
            }
            Op::Window { node_id, reply } => {
                let _ = reply.send(self.window(node_id).await).await;
            }
            Op::DeviceTime { node_id, reply } => {
                let _ = reply.send(self.device_time(node_id).await).await;
            }
            Op::SetDeviceTime { node_id, reply } => {
                let _ = reply.send(self.set_device_time(node_id).await).await;
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

    let fabrics = match OurFabrics::load(&crypto, matter, &store) {
        Ok(fabrics) => fabrics,
        Err(e) => {
            error!("service: our fabrics cannot be loaded: {e:?}");
            refuse_all(
                service,
                PeekError::failed("the controller's fabrics cannot be set up"),
            )
            .await;
            return;
        }
    };

    let controller = Controller::new(matter, &crypto);

    #[cfg(feature = "ble")]
    let btp = rs_matter::transport::network::btp::Btp::new();

    // Our node: the devices on our fabrics may ask its OTA Provider for
    // updates, and read its clock.
    for fabric in fabrics.iter() {
        if let Err(e) = node::allow_devices(matter, &store, fabric.fab_idx) {
            warn!(
                "service: cannot open our node to the devices on {}: {e:?}",
                fabric.name
            );
        }
    }

    let ota_followers = Rc::new(RefCell::new(Vec::new()));
    let ota_updates = Rc::new(RefCell::new(BTreeMap::new()));
    let (apply_jobs, apply_jobs_rx) = async_channel::unbounded();
    let ota_events = ota::events(ota_followers.clone(), ota_updates.clone(), apply_jobs);
    let ota = OtaProvider::new(ota, {
        let ota_events = ota_events.clone();
        Box::new(move |event| ota_events(event)) as Box<dyn Fn(OtaEvent)>
    });

    let (sub_jobs, sub_jobs_rx) = async_channel::unbounded();

    let ctx = Ctx {
        matter,
        crypto: &crypto,
        controller,
        fabrics: RefCell::new(fabrics),
        store,
        locks: Locks::new(),
        commissioning: Cell::new(false),
        commissioned_node: Cell::new(None),
        cancel: service.cancel.clone(),
        subs: RefCell::new(BTreeMap::new()),
        next_watcher: Cell::new(1),
        sub_jobs,
        reports: Reports::new(),
        model_gate: Gate::new(MODEL_PRIMINGS_OVER_THREAD),
        ota,
        ota_followers,
        ota_updates,
        ota_events,
        windows: RefCell::new(BTreeMap::new()),
        uploads: RefCell::new(BTreeMap::new()),
        next_upload_id: Cell::new(1),
        online: RefCell::new(None),
        transient: RefCell::new(Vec::new()),
        devices_listeners: RefCell::new(Vec::new()),
        devices_told: RefCell::new(None),
        thread_names: RefCell::new(BTreeMap::new()),
        thread_names_missed: Cell::new(None),
        border_routers: RefCell::new(BTreeMap::new()),
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
    let mut applies = pin!(ctx.run_apply_jobs(apply_jobs_rx));
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

    match select4(
        &mut pump,
        &mut ops,
        select(&mut subs, &mut applies),
        select(&mut mdns, &mut node),
    )
    .await
    {
        Either4::First(r) => error!("service: transport pump exited: {r:?}"),
        Either4::Second(()) => info!("service: every handle dropped, exiting"),
        Either4::Third(Either::First(())) => info!("service: subscription driver exited"),
        Either4::Third(Either::Second(())) => info!("service: OTA apply driver exited"),
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
            Op::AdvertisedDevices(reply) => refuse(reply, error).await,
            Op::ReadModel { reply, .. } | Op::WriteAttribute { reply, .. } => {
                refuse(reply, error).await
            }
            Op::InvokeCommand { reply, .. } => refuse(reply, error).await,
            Op::ReadEvents { reply, .. } => refuse(reply, error).await,
            Op::ReadQrImage { reply, .. } => refuse(reply, error).await,
            Op::Commission { reply, .. } => refuse(reply, error).await,
            Op::ListDevices { reply, .. } | Op::Sync { reply, .. } => refuse(reply, error).await,
            Op::Fabrics(reply)
            | Op::CreateFabric { reply, .. }
            | Op::RenameFabric { reply, .. }
            | Op::SetDefaultFabric { reply, .. }
            | Op::DeleteFabric { reply, .. } => refuse(reply, error).await,
            Op::Remember { reply, .. }
            | Op::SetTags { reply, .. }
            | Op::Decommission { reply, .. } => refuse(reply, error).await,
            Op::PeekDevice { reply, .. } => refuse(reply, error).await,
            Op::ReadNetwork { reply, .. } | Op::ChangeNetwork { reply, .. } => {
                refuse(reply, error).await
            }
            Op::RetrieveLogs { reply, .. } => refuse(reply, error).await,
            Op::Identify { reply, .. } | Op::OtaUpdate { reply, .. } => refuse(reply, error).await,
            Op::Watch { reply, .. } | Op::WatchModel { reply, .. } => refuse(reply, error).await,
            Op::OtaImages(reply) | Op::OtaUploadEnd { reply, .. } | Op::OtaDelete { reply, .. } => {
                refuse(reply, error).await
            }
            Op::OtaUploadBegin { reply, .. } => refuse(reply, error).await,
            Op::OtaUpdates(reply) => refuse(reply, error).await,
            Op::OpenWindow { reply, .. }
            | Op::CloseWindow { reply, .. }
            | Op::Window { reply, .. } => refuse(reply, error).await,
            Op::RemoveDeviceFabric { reply, .. } => refuse(reply, error).await,
            Op::DeviceTime { reply, .. } => refuse(reply, error).await,
            Op::SetDeviceTime { reply, .. } => refuse(reply, error).await,
            Op::OtaUploadChunk { reply, .. } => refuse(reply, error).await,
            Op::Assistant(reply) => refuse(reply, error).await,
            Op::ReadThreadMesh { reply, .. } => refuse(reply, error).await,
            Op::SetAssistant { reply, .. } => refuse(reply, error).await,
            Op::WatchDevices { reply, .. } => refuse(reply, error).await,
            Op::Unwatch { .. } | Op::UnwatchDevices { .. } | Op::OtaUploadDiscard { .. } => {}
        }
    }
}

async fn refuse<T>(reply: crate::api::Reply<T>, error: PeekError) {
    let _ = reply.send(Err(error)).await;
}
