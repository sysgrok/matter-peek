//! The Matter actor.
//!
//! One **persistent** task owns a single Matter stack for the whole process
//! lifetime: on startup it loads any persisted controller fabric
//! (`Matter::startup`) and ensures a CA ([`ControllerCa::ensure`]), then
//! runs the transport pump concurrently with a command loop. Each command is
//! serviced against the *shared* stack / CA / fabric, so commissioned devices
//! can be re-addressed over CASE without re-commissioning.
//!
//! The stack is `&'static` (via a `StaticCell`, fed `'static` device details) —
//! this sidesteps the self-referential `Matter<'a>` lifetime entirely: a
//! `&'static Matter` is freely shared between the pump and the command loop.

use core::cell::{Cell, RefCell};
use core::net::SocketAddr;
use core::num::NonZeroU8;
use core::pin::pin;

use std::collections::{HashMap, HashSet};
use std::fs::{File, TryLockError};
use std::io;
use std::net::UdpSocket;
use std::path::PathBuf;
use std::rc::Rc;

use directories::ProjectDirs;
use embassy_futures::select::{select, select4, Either, Either4};
use embassy_time::{with_timeout, Duration, Instant, Timer};
use futures_util::stream::{FuturesUnordered, StreamExt};

use log::{error, info, warn};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;

use rs_matter::crypto::{default_crypto, Crypto};
use rs_matter::dm::clusters::ota_prov::OtaRequestorId;
use rs_matter::dm::devices::test::{DAC_PRIVKEY, TEST_DEV_ATT, TEST_DEV_COMM, TEST_DEV_DET};
use rs_matter::error::{Error, ErrorCode};
use rs_matter::persist::DirKvBlobStore;
use rs_matter::transport::network::btp::Btp;
use rs_matter::transport::network::mdns::CommissionableFilter;
use rs_matter::transport::network::{Address, ChainedNetwork, NoNetwork};
use rs_matter::utils::init::InitMaybeUninit;
use rs_matter::utils::sync::Notification;
use rs_matter::Matter;

use static_cell::StaticCell;

use matter_peek_core::ble::{self, WirelessCreds};
use matter_peek_core::ca::{CaTier, ControllerCa};
use matter_peek_core::controller::FabricIdentity;
use matter_peek_core::node;
use matter_peek_core::ota::{OtaEvent, OtaProvider, OtaStore as _, OtaVerifier};
use matter_peek_core::persist::{kv_buf, ControllerStore};
use matter_peek_core::setup_code::SetupCode;
use matter_peek_core::subscribe::Reports;
use matter_peek_core::{Controller, DeviceHandle, DeviceIdentity, DiagSet, StoredDevice};

use matter_peek_proto as proto;

use crate::convert;
use crate::ota::{DirOtaStore, PartialImage};

/// The first device NodeID the controller hands out (see
/// [`ControllerStore::allocate_node_id`]).
const FIRST_DEVICE_NODE_ID: u64 = 112234;

/// How long after startup the first sync runs (the mDNS backend comes up first).
const FIRST_SYNC_DELAY_SECS: u64 = 5;
/// How often the device list is synced with our fabric.
const SYNC_PERIOD_SECS: u64 = 300;

/// How often a running subscription checks that a connection still watches it.
const WATCHERS_CHECK_SECS: u64 = 5;

/// How long a discovery scans for devices advertising over Bluetooth LE.
const BLE_SCAN_SECS: u16 = 5;

/// The persistent controller stack, fed `'static` device details so `Matter`'s
/// lifetime is `'static` and it can be shared freely across tasks.
static PERSIST_MATTER: StaticCell<Matter> = StaticCell::new();

/// A cooperative cancel flag for the in-flight commission, shared (single-threaded,
/// so `Rc<Cell>` — no atomics) between the server's dispatch (which sets it when
/// a [`proto::Request::Cancel`] arrives, bypassing the serial command queue) and
/// the actor (whose commission polls it at phase boundaries). See
/// [`commission_flow`].
pub type CancelFlag = Rc<Cell<bool>>;

/// A unit of work handed to the actor, with the channels to report back on.
pub struct Command {
    pub req_id: proto::ReqId,
    pub request: proto::Request,
    /// Progress events for this command (per-command; the relay closes it when
    /// the command's reply lands — used by the commission's `Step` events).
    pub events: async_channel::Sender<proto::Event>,
    /// The single final reply.
    pub reply: async_channel::Sender<proto::Response>,
    /// The **connection's** outbound sink — lives for the whole connection, not
    /// just this command. A detached health subscription streams its
    /// `HealthReport` / `SubscriptionState` frames here so they keep flowing long
    /// after the `Subscribe` command has replied. `None` for callers (e.g. the
    /// `ws_client` example paths) that don't host long-lived subscriptions.
    pub out: Option<async_channel::Sender<proto::ServerMsg>>,
}

/// The store directory, locked for one actor: two controllers sharing a store
/// would overwrite each other's fabric and device records.
pub struct Store {
    dir: PathBuf,
    _lock: File,
}

impl Store {
    /// Lock the store; fails if another matter-peek process holds it.
    pub fn lock() -> io::Result<Self> {
        let dir = store_dir();
        let lock = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join("lock"))?;

        match lock.try_lock() {
            Ok(()) => Ok(Self { dir, _lock: lock }),
            Err(TryLockError::WouldBlock) => Err(io::Error::other(format!(
                "the store at {} is in use by another matter-peek process; \
                 if it is a matter-peek-server, connect to it instead",
                dir.display()
            ))),
            Err(TryLockError::Error(e)) => Err(e),
        }
    }
}

/// Run the persistent actor: stand up one Matter stack for the process, restore
/// persisted state, ensure the controller CA + fabric, then service commands
/// against that shared stack until the channel closes. `periodic_sync` syncs
/// the device list shortly after startup and every few minutes after.
pub async fn run(
    rx: async_channel::Receiver<Command>,
    cancel: CancelFlag,
    store: Store,
    periodic_sync: bool,
) {
    let crypto = default_crypto(rand::rng(), DAC_PRIVKEY);

    // One `&'static` stack for the whole process (see module docs). Kept `&mut`
    // just long enough to `startup`, then reborrowed immutably for sharing.
    let matter: &'static mut Matter<'static> = PERSIST_MATTER.uninit().init_with(Matter::init(
        &TEST_DEV_DET,
        TEST_DEV_COMM,
        &TEST_DEV_ATT,
        0,
    ));

    // Directory-backed KV (desktop), under the platform-standard data dir so the
    // controller fabric + device records survive a reboot (unlike `/tmp`).
    // rs-matter's `SharedKvBlobStore` now borrows its scratch buffer, so the
    // actor owns one (`kv_buf`) for the whole run and lends it to the
    // `ControllerStore`. `startup` takes that store's KV access (which carries
    // the buffer) — a single argument now.
    let dir = store.dir.clone();
    info!("actor: persistence store at {}", dir.display());
    let kv_buf = kv_buf();
    // Held (locked) for as long as the actor runs.
    let _store = store;
    let store = ControllerStore::new(DirKvBlobStore::new(dir.clone()), &kv_buf);
    if let Err(e) = matter.startup(store.kv()) {
        error!("actor: startup failed: {e:?} (continuing with fresh state)");
    }
    let matter: &'static Matter<'static> = matter;
    let (ca, fab_idx) = match ControllerCa::ensure(
        &crypto,
        matter,
        &store,
        FabricIdentity::default(),
        CaTier::RcacDirect,
    ) {
        Ok(v) => v,
        Err(e) => {
            error!("actor: CA ensure failed: {e:?}");
            // Drain commands with an error so clients don't hang.
            while let Ok(cmd) = rx.recv().await {
                let _ = cmd
                    .reply
                    .send(proto::Response::Error {
                        message: "controller CA init failed".into(),
                    })
                    .await;
            }
            return;
        }
    };

    let socket = match bind_controller_socket() {
        Ok(s) => s,
        Err(e) => {
            error!("actor: socket bind failed: {e:?}");
            return;
        }
    };

    let controller = Controller::new(matter, &crypto);

    // The transport runs over UDP and BTP: commissioning over BLE uses the latter.
    let btp = Btp::new();

    // Our OTA Provider: the devices on our fabric may ask it for updates.
    if let Err(e) = node::allow_ota_requestors(matter, &store, fab_idx) {
        warn!("actor: cannot open our OTA Provider to the devices: {e:?}");
    }

    let ota_listeners = Rc::new(RefCell::new(Vec::new()));
    let ota = OtaProvider::new(
        DirOtaStore::new(dir.join("ota")),
        ota_events(ota_listeners.clone()),
    );

    // Channel over which the command handler hands new subscription *jobs* to the
    // concurrent driver. Jobs carry only owned data, so this is `'static` and can
    // be declared before `ctx`. Unbounded so `Subscribe` never blocks the loop.
    let (sub_submit, sub_recv) = async_channel::unbounded::<SubJob>();

    let ctx = ActorCtx {
        matter,
        crypto: &crypto,
        controller,
        ca,
        fab_idx,
        store,
        cancel,
        subs: RefCell::new(HashMap::new()),
        reports: Reports::new(),
        ota,
        ota_listeners,
        uploads: RefCell::new(HashMap::new()),
        next_upload_id: Cell::new(1),
        online: RefCell::new(None),
        transient: RefCell::new(Vec::new()),
        btp: &btp,
    };

    // Four concurrent arms, for the actor's lifetime:
    //   1. the transport pump (`matter.run`),
    //   2. the command loop (services requests one at a time),
    //   3. the subscription driver (polls all live subscriptions concurrently), and
    //   4. the mDNS backend, which services **operational resolves** — CASE
    //      re-addressing of a listed device (subscribe / peek / decommission
    //      after its secure session lapses) needs a running resolver; without it
    //      every such resolve times out with `NotFound`. (It also services
    //      discovery browses, so those must NOT start a second backend — the
    //      `discover` handler uses the `_bg` variant.)
    // Because subscriptions run in their own arm — not inside a command — the
    // command loop stays free while any number of subscriptions stream, so a
    // `Subscribe` returns immediately and multiple devices can be watched at once.
    let mut pump = pin!(matter.run(
        &crypto,
        ChainedNetwork::new(Address::is_udp, &socket, &btp),
        ChainedNetwork::new(Address::is_udp, &socket, &btp),
        NoNetwork,
    ));
    let mut loop_ = pin!(command_loop(rx, &ctx, &sub_submit, periodic_sync));
    let mut subs = pin!(run_subscriptions(sub_recv, &ctx));
    let mut mdns = pin!(ctx.controller.run_mdns_backend());
    // What devices reach on us: our OTA Provider and the subscription reports.
    let mut node = pin!(ctx.controller.run_node(&ctx.reports, &ctx.ota));

    match select4(
        &mut pump,
        &mut loop_,
        &mut subs,
        select(&mut mdns, &mut node),
    )
    .await
    {
        Either4::First(r) => error!("actor: transport pump exited: {r:?}"),
        Either4::Second(()) => info!("actor: command channel closed, exiting"),
        Either4::Third(()) => info!("actor: subscription driver exited"),
        Either4::Fourth(Either::First(r)) => error!("actor: mDNS backend exited: {r:?}"),
        Either4::Fourth(Either::Second(r)) => error!("actor: controller node exited: {r:?}"),
    }
}

/// Drive every live subscription concurrently. New subscription *jobs* arrive on
/// `recv` (submitted by the [`subscribe`] handler); the driver builds each one's
/// future against the shared stack (`ctx`) and adds it to the running set. A job's
/// future ends when its subscription is stopped (its `stop_rx` fires or its `out`
/// sink closes), then it's dropped from the set. Runs for the actor's lifetime.
async fn run_subscriptions<C: Crypto>(
    recv: async_channel::Receiver<SubJob>,
    ctx: &ActorCtx<'_, C>,
) {
    let mut running = FuturesUnordered::new();

    loop {
        if running.is_empty() {
            // Nothing running — just wait for the next job (polling an empty
            // `FuturesUnordered` returns `None` immediately, which would busy-loop).
            match recv.recv().await {
                Ok(job) => running.push(run_subscription(job, ctx)),
                Err(_) => return, // actor shutting down
            }
        } else {
            match select(recv.recv(), running.next()).await {
                Either::First(Ok(job)) => running.push(run_subscription(job, ctx)),
                Either::First(Err(_)) => return,
                // A subscription finished (stopped); already removed by `next()`.
                Either::Second(_) => {}
            }
        }
    }
}

/// The future for a single live subscription: stream `subscribe_health` output to
/// the connection sink until stopped, then emit a terminal `Stopped` and drop the
/// registry entry (if it's still ours). Built here (not in [`subscribe`]) so it
/// can borrow the shared stack without entangling the submission channel's type.
async fn run_subscription<C: Crypto>(job: SubJob, ctx: &ActorCtx<'_, C>) {
    let SubJob {
        node_id,
        handle,
        stop_rx,
        my_stop,
        retry,
    } = job;

    // The registry entry of this subscription, unless it was replaced or removed.
    let with_watch = |f: &mut dyn FnMut(&mut Watch)| {
        if let Some(watch) = ctx
            .subs
            .borrow_mut()
            .get_mut(&node_id)
            .filter(|watch| watch.stop.same_channel(&my_stop))
        {
            f(watch);
        }
    };

    let subscription = ctx.controller.subscribe_health(
        &ctx.reports,
        handle,
        DiagSet::ALL,
        &retry,
        |health| {
            let health = convert::health(health);
            with_watch(&mut |watch| {
                watch.reports += 1;
                watch.health.merge(health.clone());
                watch.broadcast(proto::Event::HealthReport {
                    node_id,
                    seq: watch.reports,
                    health: health.clone(),
                });
            });
        },
        |state| {
            let state = convert::sub_state(state);
            match state {
                proto::SubscriptionState::Live { .. } => set_online(ctx, node_id, true),
                proto::SubscriptionState::Offline => set_online(ctx, node_id, false),
                proto::SubscriptionState::Stopped => {}
            }
            with_watch(&mut |watch| {
                // Failed retries repeat `Offline`; tell the watchers once.
                if watch.state != Some(state) {
                    watch.state = Some(state);
                    watch.broadcast(proto::Event::SubscriptionState { node_id, state });
                }
            });
        },
    );

    // Stop on the explicit signal, or once no connection watches the device.
    let stop = async {
        loop {
            let check = Timer::after(Duration::from_secs(WATCHERS_CHECK_SECS));
            if let Either::First(_) = select(stop_rx.recv(), check).await {
                break;
            }

            let mut watched = false;
            with_watch(&mut |watch| watched = watch.watched());
            if !watched {
                break;
            }
        }
    };
    let _ = select(subscription, stop).await;

    // Drop our registry entry, but only if it's still *ours* (a quick
    // unsubscribe+resubscribe of the same device could have replaced it), and
    // tell its watchers.
    let watch = {
        let mut map = ctx.subs.borrow_mut();
        if map
            .get(&node_id)
            .is_some_and(|watch| watch.stop.same_channel(&my_stop))
        {
            map.remove(&node_id)
        } else {
            None
        }
    };
    if let Some(mut watch) = watch {
        watch.broadcast(proto::Event::SubscriptionState {
            node_id,
            state: proto::SubscriptionState::Stopped,
        });
    }
}

/// Shared state every command handler operates on.
struct ActorCtx<'a, C: Crypto> {
    matter: &'a Matter<'a>,
    crypto: &'a C,
    controller: Controller<'a, C>,
    ca: ControllerCa,
    fab_idx: NonZeroU8,
    store: ControllerStore<'a, DirKvBlobStore>,
    /// Cooperative cancel flag for the in-flight commission (set by the server's
    /// dispatch, polled by [`commission_flow`] at phase boundaries).
    cancel: CancelFlag,
    /// Live health subscriptions, keyed by device `node_id`, each shared by the
    /// connections watching that device. Present ⇒ that device is being watched.
    subs: RefCell<HashMap<u64, Watch>>,
    /// Routes the reports of the live subscriptions to them.
    reports: Reports,
    /// Our OTA Provider, over the images in the data directory.
    ota: OtaProvider<DirOtaStore, Box<dyn Fn(OtaEvent)>>,
    /// The connections following OTA updates.
    ota_listeners: Rc<RefCell<Vec<OtaListener>>>,
    /// The OTA image uploads in progress, by ID.
    uploads: RefCell<HashMap<u32, Upload<'a, C>>>,
    next_upload_id: Cell<u32>,
    /// The devices that advertised on our fabric at the last sync; `None`
    /// before the first one.
    online: RefCell<Option<HashSet<u64>>>,
    /// The listed devices we keep no record of (not remembered): commissioned
    /// without `remember`, or found by a sync, since the server started.
    transient: RefCell<Vec<StoredDevice>>,
    /// The Bluetooth side of the transport, for commissioning over BLE.
    btp: &'a Btp,
}

/// A live health subscription to a device, shared by every connection watching
/// it: its events go to all of them.
struct Watch {
    /// Fires (or drops) to stop the device subscription.
    stop: async_channel::Sender<()>,
    /// Cuts the wait before the next attempt to re-establish it short.
    retry: Rc<Notification>,
    /// The connections watching the device.
    watchers: Vec<async_channel::Sender<proto::ServerMsg>>,
    /// The subscription's state, and the health merged from its reports, so
    /// far: what a connection joining it now is told first.
    state: Option<proto::SubscriptionState>,
    health: proto::DeviceHealth,
    reports: u64,
}

impl Watch {
    /// Send `event` to the watchers still connected, forgetting the others.
    fn broadcast(&mut self, event: proto::Event) {
        self.watchers.retain(|watcher| !watcher.is_closed());

        for watcher in &self.watchers {
            let _ = watcher.try_send(proto::ServerMsg::Event {
                event: event.clone(),
            });
        }
    }

    /// Add `watcher`, telling it the subscription's state and health so far.
    fn join(&mut self, node_id: u64, watcher: async_channel::Sender<proto::ServerMsg>) {
        if self.watchers.iter().any(|w| w.same_channel(&watcher)) {
            return;
        }

        if self.reports > 0 {
            let _ = watcher.try_send(proto::ServerMsg::Event {
                event: proto::Event::HealthReport {
                    node_id,
                    seq: self.reports,
                    health: self.health.clone(),
                },
            });
        }
        if let Some(state) = self.state {
            let _ = watcher.try_send(proto::ServerMsg::Event {
                event: proto::Event::SubscriptionState { node_id, state },
            });
        }

        self.watchers.push(watcher);
    }

    /// Whether a connection still watches the device.
    fn watched(&mut self) -> bool {
        self.watchers.retain(|watcher| !watcher.is_closed());
        !self.watchers.is_empty()
    }
}

/// An OTA image file being uploaded in chunks: written as it arrives, and
/// checked as it is.
struct Upload<'a, C: Crypto + 'a> {
    verifier: OtaVerifier<C::Hash<'a>>,
    partial: PartialImage,
    size: u64,
    received: u64,
    /// The connection uploading it; the upload is dropped once it closes.
    out: async_channel::Sender<proto::ServerMsg>,
}

/// A connection following the OTA update of a device.
struct OtaListener {
    node_id: u64,
    name: String,
    out: async_channel::Sender<proto::ServerMsg>,
}

/// A subscription request submitted to the concurrent driver. Carries only owned
/// data (no borrow of the shared stack), so the submission channel is `'static`;
/// the driver itself borrows the stack (`&ActorCtx`) to build+run the future.
struct SubJob {
    node_id: u64,
    handle: DeviceHandle,
    /// Fires (or its sender drops) to stop the subscription.
    stop_rx: async_channel::Receiver<()>,
    /// A clone of the registry stop-sender, so the driver can verify at cleanup
    /// that the registry entry is still this subscription before removing it.
    my_stop: async_channel::Sender<()>,
    /// See [`Watch::retry`].
    retry: Rc<Notification>,
}

/// Pull commands and service them one at a time against the shared stack.
///
/// `ctx` is borrowed for the actor's whole lifetime (`'a`) because a `Subscribe`
/// submits a subscription future that borrows `ctx.controller` for that long.
async fn command_loop<C: Crypto>(
    rx: async_channel::Receiver<Command>,
    ctx: &ActorCtx<'_, C>,
    sub_submit: &async_channel::Sender<SubJob>,
    periodic_sync: bool,
) {
    let after = |secs| {
        if periodic_sync {
            Instant::now() + Duration::from_secs(secs)
        } else {
            Instant::MAX
        }
    };

    let mut next_sync = after(FIRST_SYNC_DELAY_SECS);

    loop {
        match select(rx.recv(), Timer::at(next_sync)).await {
            Either::First(Ok(cmd)) => {
                let response = service(&cmd, ctx, sub_submit).await;

                if matches!(cmd.request, proto::Request::Sync) {
                    next_sync = after(SYNC_PERIOD_SECS);
                }

                // Best-effort: if the client vanished, dropping the reply is fine.
                let _ = cmd.reply.send(response).await;
            }
            Either::First(Err(_)) => return,
            Either::Second(()) => {
                if let Err(e) = sync_devices(ctx, false).await {
                    warn!("sync failed: {e:?}");
                }

                next_sync = after(SYNC_PERIOD_SECS);
            }
        }
    }
}

/// Service one command against the shared stack, emitting step events as it goes.
async fn service<C: Crypto>(
    cmd: &Command,
    ctx: &ActorCtx<'_, C>,
    sub_submit: &async_channel::Sender<SubJob>,
) -> proto::Response {
    match &cmd.request {
        proto::Request::Commission {
            via,
            setup_code,
            remember,
        } => commission(cmd, ctx, via, setup_code, *remember).await,
        proto::Request::ReadQrImage { image } => read_qr_image(image),
        proto::Request::Discover {
            discriminator,
            setup_code,
            max,
        } => run_discover(ctx, *discriminator, setup_code.as_deref(), *max).await,
        proto::Request::ListDevices => list_devices(ctx),
        proto::Request::Sync => sync(ctx).await,
        proto::Request::PeekDevice { node_id } => peek_device(ctx, *node_id).await,
        proto::Request::RetrieveLogs { node_id, intent } => {
            retrieve_logs(ctx, *node_id, *intent).await
        }
        proto::Request::Identify { node_id, secs } => identify(ctx, *node_id, *secs).await,
        proto::Request::Subscribe { node_id } => subscribe(cmd, ctx, sub_submit, *node_id).await,
        proto::Request::Unsubscribe { node_id } => unsubscribe(cmd, ctx, *node_id),
        proto::Request::Decommission { node_id, force } => {
            decommission(ctx, *node_id, *force).await
        }
        proto::Request::Remember { node_id } => remember(ctx, *node_id),
        proto::Request::SetTags { node_id, tags } => set_tags(ctx, *node_id, tags),
        // `Cancel` is normally intercepted by the server (it sets the shared flag
        // out-of-band rather than enqueuing). If one reaches here, no commission
        // is running — clear any stale flag and ack.
        proto::Request::Cancel => {
            ctx.cancel.set(false);
            proto::Response::Ok
        }
        proto::Request::OtaImages => ota_images(ctx),
        proto::Request::OtaUploadBegin { size } => ota_upload_begin(cmd, ctx, *size),
        proto::Request::OtaUploadChunk {
            upload_id,
            offset,
            data,
        } => ota_upload_chunk(ctx, *upload_id, *offset, data),
        proto::Request::OtaUploadEnd { upload_id } => ota_upload_end(ctx, *upload_id),
        proto::Request::OtaDelete { name } => ota_delete(ctx, name),
        proto::Request::OtaUpdate { node_id, name } => ota_update(cmd, ctx, *node_id, name).await,
    }
}

fn ota_images<C: Crypto>(ctx: &ActorCtx<'_, C>) -> proto::Response {
    proto::Response::OtaImages {
        images: ctx
            .ota
            .store()
            .images()
            .into_iter()
            .map(convert::ota_image)
            .collect(),
    }
}

/// The largest upload chunk taken, decoded.
const MAX_UPLOAD_CHUNK_LEN: usize = 4 * 1024 * 1024;

fn ota_upload_begin<'a, C: Crypto>(
    cmd: &Command,
    ctx: &ActorCtx<'a, C>,
    size: u64,
) -> proto::Response {
    let Some(out) = cmd.out.clone() else {
        return proto::Response::Error {
            message: "uploads require a streaming connection".into(),
        };
    };

    drop_closed_uploads(ctx);

    let crypto: &'a C = ctx.crypto;
    let upload_id = ctx.next_upload_id.get();
    ctx.next_upload_id.set(upload_id.wrapping_add(1));

    let upload = crypto.hash().and_then(|hasher| {
        Ok(Upload {
            verifier: OtaVerifier::new(hasher),
            partial: ctx.ota.store().begin(upload_id)?,
            size,
            received: 0,
            out,
        })
    });

    match upload {
        Ok(upload) => {
            ctx.uploads.borrow_mut().insert(upload_id, upload);
            proto::Response::OtaUploadStarted { upload_id }
        }
        Err(e) => proto::Response::Error {
            message: format!("cannot start the upload: {:?}", e.code()),
        },
    }
}

fn ota_upload_chunk<C: Crypto>(
    ctx: &ActorCtx<'_, C>,
    upload_id: u32,
    offset: u64,
    data: &str,
) -> proto::Response {
    let chunk = match BASE64.decode(data) {
        Ok(chunk) if chunk.len() <= MAX_UPLOAD_CHUNK_LEN => chunk,
        Ok(_) => return ota_upload_fail(ctx, upload_id, "a chunk is too big".into()),
        Err(e) => return ota_upload_fail(ctx, upload_id, format!("a chunk is not base64: {e}")),
    };

    let mut uploads = ctx.uploads.borrow_mut();
    let Some(upload) = uploads.get_mut(&upload_id) else {
        return proto::Response::Error {
            message: format!("no upload {upload_id}"),
        };
    };

    if offset != upload.received {
        let message = format!("expected the chunk at {}, got {offset}", upload.received);
        drop(uploads);
        return ota_upload_fail(ctx, upload_id, message);
    }

    let taken = if upload.received + chunk.len() as u64 > upload.size {
        Err("more data than announced".to_string())
    } else if let Err(e) = upload.verifier.update(&chunk) {
        Err(format!("not a valid OTA image: {:?}", e.code()))
    } else if let Err(e) = upload.partial.write(&chunk) {
        Err(format!("cannot write the image: {:?}", e.code()))
    } else {
        upload.received += chunk.len() as u64;
        Ok(())
    };

    drop(uploads);

    match taken {
        Ok(()) => proto::Response::Ok,
        Err(message) => ota_upload_fail(ctx, upload_id, message),
    }
}

fn ota_upload_end<C: Crypto>(ctx: &ActorCtx<'_, C>, upload_id: u32) -> proto::Response {
    let Some(upload) = ctx.uploads.borrow_mut().remove(&upload_id) else {
        return proto::Response::Error {
            message: format!("no upload {upload_id}"),
        };
    };

    let store = ctx.ota.store();

    if upload.received != upload.size {
        store.discard(upload.partial);
        return proto::Response::Error {
            message: format!(
                "the upload ended at {} of {} bytes",
                upload.received, upload.size
            ),
        };
    }

    let image = match upload.verifier.finish() {
        Ok(image) => image,
        Err(e) => {
            store.discard(upload.partial);
            return proto::Response::Error {
                message: format!("not a valid OTA image: {:?}", e.code()),
            };
        }
    };

    match store.finish(upload.partial, &image).map(|()| image) {
        Ok(image) => {
            info!(
                "actor: stored OTA image {} ({})",
                image.name, image.version_str
            );
            ota_images(ctx)
        }
        Err(e) => proto::Response::Error {
            message: format!("cannot store the OTA image: {:?}", e.code()),
        },
    }
}

/// Drop upload `upload_id` and its partial file, answering with `message`.
fn ota_upload_fail<C: Crypto>(
    ctx: &ActorCtx<'_, C>,
    upload_id: u32,
    message: String,
) -> proto::Response {
    if let Some(upload) = ctx.uploads.borrow_mut().remove(&upload_id) {
        ctx.ota.store().discard(upload.partial);
    }

    proto::Response::Error { message }
}

/// Drop the uploads of the connections that closed.
fn drop_closed_uploads<C: Crypto>(ctx: &ActorCtx<'_, C>) {
    let mut uploads = ctx.uploads.borrow_mut();

    let closed: Vec<u32> = uploads
        .iter()
        .filter(|(_, upload)| upload.out.is_closed())
        .map(|(id, _)| *id)
        .collect();

    for id in closed {
        if let Some(upload) = uploads.remove(&id) {
            warn!("actor: dropping OTA upload {id}: its connection closed");
            ctx.ota.store().discard(upload.partial);
        }
    }
}

fn ota_delete<C: Crypto>(ctx: &ActorCtx<'_, C>, name: &str) -> proto::Response {
    match ctx.ota.store().remove(name) {
        Ok(()) => ota_images(ctx),
        Err(e) => proto::Response::Error {
            message: format!("cannot delete OTA image {name}: {:?}", e.code()),
        },
    }
}

/// Allow the device image `name`, follow its update on the connection, and
/// announce us to it as its OTA Provider.
async fn ota_update<C: Crypto>(
    cmd: &Command,
    ctx: &ActorCtx<'_, C>,
    node_id: u64,
    name: &str,
) -> proto::Response {
    let Some(dev) = lookup_device(ctx, node_id) else {
        return no_device(node_id);
    };

    if !ctx
        .ota
        .store()
        .images()
        .iter()
        .any(|image| image.name == name)
    {
        return proto::Response::Error {
            message: format!("no OTA image {name}"),
        };
    }

    // Before the announcement: the device asks for the update right away.
    ctx.ota.allow(
        OtaRequestorId {
            fab_idx: dev.handle.fabric_index,
            node_id,
        },
        name,
    );

    if let Some(out) = cmd.out.clone() {
        let mut listeners = ctx.ota_listeners.borrow_mut();
        listeners.retain(|listener| listener.node_id != node_id);
        listeners.push(OtaListener {
            node_id,
            name: name.to_string(),
            out,
        });
    }

    match ctx.controller.announce_ota_provider(dev.handle).await {
        Ok(()) => proto::Response::Ok,
        Err(e) => proto::Response::Error {
            message: format!("cannot announce us to the device: {:?}", e.code()),
        },
    }
}

/// Send each OTA event to the connections following that update.
fn ota_events(listeners: Rc<RefCell<Vec<OtaListener>>>) -> Box<dyn Fn(OtaEvent)> {
    Box::new(move |event| {
        let mut listeners = listeners.borrow_mut();
        listeners.retain(|listener| !listener.out.is_closed());

        let (followers, progress): (Vec<&OtaListener>, _) = match event {
            OtaEvent::Offered { node, image } => (
                listeners
                    .iter()
                    .filter(|l| l.node_id == node.node_id)
                    .collect(),
                proto::OtaProgress::Offered { name: image },
            ),
            OtaEvent::NotOffered { node } => (
                listeners
                    .iter()
                    .filter(|l| l.node_id == node.node_id)
                    .collect(),
                proto::OtaProgress::NotOffered,
            ),
            OtaEvent::Downloaded {
                image,
                offset,
                size,
            } => (
                listeners.iter().filter(|l| l.name == image).collect(),
                proto::OtaProgress::Downloading {
                    percent: (offset as u128 * 100 / size.max(1) as u128) as u8,
                },
            ),
            OtaEvent::Applying { node, version } => (
                listeners
                    .iter()
                    .filter(|l| l.node_id == node.node_id)
                    .collect(),
                proto::OtaProgress::Applying { version },
            ),
            OtaEvent::Applied { node, version } => (
                listeners
                    .iter()
                    .filter(|l| l.node_id == node.node_id)
                    .collect(),
                proto::OtaProgress::Applied { version },
            ),
        };

        for listener in followers {
            let _ = listener.out.try_send(proto::ServerMsg::Event {
                event: proto::Event::OtaProgress {
                    node_id: listener.node_id,
                    progress: progress.clone(),
                },
            });
        }
    })
}

/// Browse for commissionable devices and return a pick-list. Uses the shared
/// stack's transport/mDNS rendezvous (the actor's pump is already running).
async fn run_discover<C: Crypto>(
    ctx: &ActorCtx<'_, C>,
    discriminator: Option<u16>,
    setup_code: Option<&str>,
    max: u8,
) -> proto::Response {
    let filter = match (discriminator, setup_code) {
        (Some(discriminator), _) => CommissionableFilter {
            discriminator: Some(discriminator),
            ..Default::default()
        },
        (None, Some(code)) => match SetupCode::parse(code) {
            Ok(code) => code.filter,
            Err(e) => return invalid_setup_code(e),
        },
        (None, None) => CommissionableFilter::default(),
    };

    // A device can advertise as commissionable while not taking commissioning
    // (`CM=0`, e.g. once decommissioned); listing it would only invite a failure.
    let filter = CommissionableFilter {
        commissioning_mode_only: true,
        ..filter
    };

    // The actor already runs the mDNS backend (a permanent arm); use the `_bg`
    // variant so discovery doesn't start a competing second backend.
    let (devices, ble_devices) = embassy_futures::join::join(
        ctx.controller.discover_all_bg(&filter, max.max(1), None),
        ble::scan_ble(&filter, BLE_SCAN_SECS),
    )
    .await;

    let ble_devices = ble_devices
        .unwrap_or_else(|e| {
            warn!("discover: no Bluetooth scan: {e:?}");
            Vec::new()
        })
        .into_iter()
        .take(max.into())
        .map(|d| proto::BleDevice {
            addr: d.addr.to_string(),
            discriminator: d.discriminator,
            vendor_id: d.vendor_id,
            product_id: d.product_id,
        })
        .collect();

    let devices = devices
        .into_iter()
        .map(|d| proto::DiscoveredDevice {
            addr: match d.address {
                rs_matter::transport::network::Address::Udp(a) => a.to_string(),
                other => format!("{other:?}"),
            },
            commissionable_id: d.commissionable_id,
        })
        .collect();

    proto::Response::Discovered {
        devices,
        ble_devices,
    }
}

/// List the devices on our fabric (see [`proto::Request::ListDevices`]).
fn list_devices<C: Crypto>(ctx: &ActorCtx<'_, C>) -> proto::Response {
    list_devices_with(ctx, None)
}

/// As [`list_devices`], but attaches a non-fatal `notice` about the operation
/// that produced the list (e.g. a decommission that could not reach the device).
fn list_devices_with<C: Crypto>(ctx: &ActorCtx<'_, C>, notice: Option<String>) -> proto::Response {
    let online = ctx.online.borrow();
    let transient = ctx.transient.borrow();

    match ctx.store.load_devices() {
        Ok(remembered) => proto::Response::Devices {
            devices: remembered
                .into_iter()
                .map(|d| (d, true))
                .chain(transient.iter().cloned().map(|d| (d, false)))
                .map(|(d, remembered)| {
                    let node_id = d.handle.node_id;
                    let online = online.as_ref().map(|o| o.contains(&node_id));
                    convert::device(d, online, remembered)
                })
                .collect(),
            notice,
        },
        Err(e) => proto::Response::Error {
            message: format!("load devices failed: {e:?}"),
        },
    }
}

/// Re-address a listed device by NodeID over CASE (no re-commission) and read
/// its health.
async fn peek_device<C: Crypto>(ctx: &ActorCtx<'_, C>, node_id: u64) -> proto::Response {
    let Some(dev) = lookup_device(ctx, node_id) else {
        return no_device(node_id);
    };

    let health = match ctx.controller.read_health(dev.handle, DiagSet::ALL).await {
        Ok(health) => health,
        Err(e) => {
            return proto::Response::Error {
                message: format!("device 0x{node_id:016x} did not answer ({:?})", e.code()),
            }
        }
    };

    // It answers, so an offline subscription of it can be re-established now.
    set_online(ctx, node_id, true);
    if let Some(watch) = ctx.subs.borrow().get(&node_id) {
        if watch.state == Some(proto::SubscriptionState::Offline) {
            watch.retry.notify();
        }
    }

    proto::Response::Peek {
        handle: convert::handle(dev.handle),
        health: convert::health(health),
    }
}

/// Retrieve a diagnostic log of a listed device.
async fn retrieve_logs<C: Crypto>(
    ctx: &ActorCtx<'_, C>,
    node_id: u64,
    intent: proto::LogIntent,
) -> proto::Response {
    let Some(dev) = lookup_device(ctx, node_id) else {
        return no_device(node_id);
    };

    match ctx
        .controller
        .retrieve_logs(dev.handle, convert::log_intent(intent))
        .await
    {
        Ok(log) => proto::Response::Logs {
            node_id,
            intent,
            data: BASE64.encode(log),
        },
        Err(e) => proto::Response::Error {
            message: format!("retrieving the logs failed: {e:?}"),
        },
    }
}

/// Blink a listed device (Identify cluster) on its application endpoint.
async fn identify<C: Crypto>(ctx: &ActorCtx<'_, C>, node_id: u64, secs: u16) -> proto::Response {
    let Some(dev) = lookup_device(ctx, node_id) else {
        return no_device(node_id);
    };
    match ctx.controller.identify(dev.handle, 1, secs).await {
        Ok(()) => proto::Response::Ok,
        Err(e) => proto::Response::Error {
            message: format!("identify failed: {e:?}"),
        },
    }
}

/// Start a live health subscription to a listed device and reply
/// **immediately** — the subscription then runs as its own arm of the actor (see
/// [`run_subscriptions`]), streaming `HealthReport` / `SubscriptionState` frames
/// to the connection sink until [`unsubscribe`]d or the connection drops. Because
/// it does not run *inside* this command, the command loop stays free and many
/// devices can be watched at once.
async fn subscribe<C: Crypto>(
    cmd: &Command,
    ctx: &ActorCtx<'_, C>,
    sub_submit: &async_channel::Sender<SubJob>,
    node_id: u64,
) -> proto::Response {
    let Some(dev) = lookup_device(ctx, node_id) else {
        return no_device(node_id);
    };

    // Events stream to the connection's outbound sink, which outlives this
    // command. Without one (non-WS caller) there's nowhere to stream, so decline.
    let Some(out) = cmd.out.clone() else {
        return proto::Response::Error {
            message: "subscriptions require a streaming connection".into(),
        };
    };

    // Already watching this device? Join the running subscription.
    if let Some(watch) = ctx.subs.borrow_mut().get_mut(&node_id) {
        watch.join(node_id, out);

        return proto::Response::Subscribed {
            node_id,
            already: true,
        };
    }

    // Stop signal: `Unsubscribe` (or the last watcher leaving) fires it. Register
    // the sender under `node_id`; `my_stop` (a clone) lets the driver verify
    // ownership at cleanup.
    let (stop_tx, stop_rx) = async_channel::bounded::<()>(1);
    let my_stop = stop_tx.clone();
    let retry = Rc::new(Notification::new());
    ctx.subs.borrow_mut().insert(
        node_id,
        Watch {
            stop: stop_tx,
            retry: retry.clone(),
            watchers: vec![out],
            state: None,
            health: proto::DeviceHealth::default(),
            reports: 0,
        },
    );

    // Hand a job (owned data only) to the driver, which builds+runs the actual
    // subscription against the shared stack. Reply immediately.
    let job = SubJob {
        node_id,
        handle: dev.handle,
        stop_rx,
        my_stop,
        retry,
    };
    if sub_submit.send(job).await.is_err() {
        ctx.subs.borrow_mut().remove(&node_id);
        return proto::Response::Error {
            message: "subscription driver unavailable".into(),
        };
    }

    proto::Response::Subscribed {
        node_id,
        already: false,
    }
}

/// Stop watching a device on this connection; the device subscription stops
/// with its last watcher. Idempotent — unsubscribing an unknown device is `Ok`.
fn unsubscribe<C: Crypto>(cmd: &Command, ctx: &ActorCtx<'_, C>, node_id: u64) -> proto::Response {
    let mut subs = ctx.subs.borrow_mut();

    if let Some(watch) = subs.get_mut(&node_id) {
        match &cmd.out {
            Some(out) => watch.watchers.retain(|watcher| !watcher.same_channel(out)),
            None => watch.watchers.clear(),
        }

        // The last watcher gone: stop the device subscription. Best-effort: if
        // the task is already ending, dropping `stop` wakes it all the same.
        if !watch.watched() {
            if let Some(watch) = subs.remove(&node_id) {
                let _ = watch.stop.try_send(());
            }
            info!("unsubscribed node 0x{node_id:016x}");
        }
    }

    proto::Response::Ok
}

/// How many times to try reaching the device for `RemoveFabric` before giving
/// up. A listed device is addressed over CASE, which first needs an
/// *operational mDNS resolve* — a device that is asleep, slow to answer, or
/// re-announcing itself can miss one attempt and answer the next, so a single
/// failure is not proof that it is gone.
const DECOMMISSION_ATTEMPTS: u32 = 3;

/// Pause between those attempts, to give a slow resolve room to land.
const DECOMMISSION_RETRY_DELAY_SECS: u64 = 2;

/// How long those attempts may take in all. Each address of an unreachable
/// device costs a full CASE timeout, and a cached advertisement can outlive the
/// device.
const DECOMMISSION_TIMEOUT_SECS: u64 = 30;

/// `RemoveFabric` on the device, retried up to [`DECOMMISSION_ATTEMPTS`] times.
/// `Err` carries the last failure, formatted, once every attempt is spent.
async fn try_remove_fabric<C: Crypto>(
    ctx: &ActorCtx<'_, C>,
    handle: DeviceHandle,
    node_id: u64,
) -> Result<(), String> {
    let mut last = String::new();

    for attempt in 1..=DECOMMISSION_ATTEMPTS {
        match ctx.controller.decommission(handle).await {
            Ok(()) => {
                info!("decommissioned node 0x{node_id:016x} (RemoveFabric ok, attempt {attempt})");
                return Ok(());
            }
            Err(e) => {
                warn!(
                    "decommission node 0x{node_id:016x}: attempt {attempt}/{DECOMMISSION_ATTEMPTS} failed: {e:?}"
                );
                last = format!("{e:?}");
                if attempt < DECOMMISSION_ATTEMPTS {
                    Timer::after(Duration::from_secs(DECOMMISSION_RETRY_DELAY_SECS)).await;
                }
            }
        }
    }

    Err(last)
}

/// Decommission a listed device: stop any live subscription, `RemoveFabric` our
/// fabric on the device (freeing its fabric slot), then drop it from the list
/// and its record, if any. Returns the refreshed device list.
///
/// The device leaves the list even if it cannot be reached, or with `force` is
/// not contacted at all. Such a device still holds our fabric, so a sync lists
/// it again once it advertises.
async fn decommission<C: Crypto>(
    ctx: &ActorCtx<'_, C>,
    node_id: u64,
    force: bool,
) -> proto::Response {
    let Some(dev) = lookup_device(ctx, node_id) else {
        return no_device(node_id);
    };

    // Stop any live subscription first (its CASE session is about to be torn
    // down), and tell its watchers.
    let watch = ctx.subs.borrow_mut().remove(&node_id);
    if let Some(mut watch) = watch {
        let _ = watch.stop.try_send(());
        watch.broadcast(proto::Event::SubscriptionState {
            node_id,
            state: proto::SubscriptionState::Stopped,
        });
    }

    let removed = if force {
        info!("decommission node 0x{node_id:016x}: forced - not contacting the device");
        false
    } else {
        let attempts = try_remove_fabric(ctx, dev.handle, node_id);
        match with_timeout(Duration::from_secs(DECOMMISSION_TIMEOUT_SECS), attempts).await {
            Ok(result) => result.is_ok(),
            Err(_) => {
                warn!("decommission node 0x{node_id:016x}: gave up after {DECOMMISSION_TIMEOUT_SECS}s");
                false
            }
        }
    };

    // Drop any secure session we still hold, but only if `RemoveFabric` did not
    // go through: on success, the exchange that carried it still has a trailing
    // close to send on that session.
    if !removed {
        ctx.controller.forget_sessions(dev.handle);
    }

    set_online(ctx, node_id, false);
    ctx.transient
        .borrow_mut()
        .retain(|d| d.handle.node_id != node_id);

    let notice = (!removed && !force).then(|| {
        format!(
            "Device 0x{node_id:016x} could not be reached, so it was only removed from the \
             list and still holds our fabric. Once it is back online, a sync lists it again \
             and it can be decommissioned."
        )
    });

    update_devices(
        ctx,
        |devices| devices.retain(|d| d.handle.node_id != node_id),
        notice,
    )
}

/// Apply `f` to the device records, then return the refreshed device list.
fn update_devices<C: Crypto>(
    ctx: &ActorCtx<'_, C>,
    f: impl FnOnce(&mut alloc::vec::Vec<matter_peek_core::StoredDevice>),
    notice: Option<String>,
) -> proto::Response {
    let updated = ctx.store.load_devices().and_then(|mut devices| {
        f(&mut devices);
        ctx.store.save_devices(&devices)
    });

    match updated {
        Ok(()) => list_devices_with(ctx, notice),
        Err(e) => proto::Response::Error {
            message: format!("failed to update store: {e:?}"),
        },
    }
}

/// Re-sync the device list with our fabric (see [`proto::Request::Sync`]),
/// refreshing the identity of every reachable device, and answer with the list.
async fn sync<C: Crypto>(ctx: &ActorCtx<'_, C>) -> proto::Response {
    match sync_devices(ctx, true).await {
        Ok(notice) => list_devices_with(ctx, notice),
        Err(e) => proto::Response::Error {
            message: format!("sync failed: {e:?}"),
        },
    }
}

/// Align the device list with the devices advertising on our fabric: list
/// those not listed yet (keeping no record of them), drop those without a record
/// that are gone and, if `refresh`, re-read the others' identity. Notes which
/// devices are online. Returns a notice of what changed, if anything did.
async fn sync_devices<C: Crypto>(
    ctx: &ActorCtx<'_, C>,
    refresh: bool,
) -> Result<Option<String>, Error> {
    let advertised = ctx.controller.browse_fabric(ctx.fab_idx).await?;
    info!("sync: {} device(s) on our fabric", advertised.len());

    let mut remembered = ctx.store.load_devices()?;
    let mut found = 0;

    for &node_id in &advertised {
        // In use on our fabric, so never to be handed out again.
        ctx.store.reserve_node_id(node_id)?;

        let handle = DeviceHandle {
            fabric_index: ctx.fab_idx,
            node_id,
        };

        let record = remembered.iter().position(|d| d.handle.node_id == node_id);
        let listed = record.is_some()
            || ctx
                .transient
                .borrow()
                .iter()
                .any(|d| d.handle.node_id == node_id);

        if listed && !refresh {
            continue;
        }

        let ident = ctx.controller.read_basic_info(handle).await;

        // An unusable fingerprint means the read failed. Only a device that
        // answers is listed: an advertisement can outlive its device (cached
        // until its TTL runs out).
        if !ident.fingerprint.is_usable() {
            if !listed {
                warn!(
                    "sync: node 0x{node_id:016x} is advertised on our fabric but does not answer"
                );
            }
            continue;
        }

        if let Some(i) = record {
            refresh_identity(&mut remembered[i], ident);
            ctx.store.save_devices(&remembered)?;
            continue;
        }

        let mut transient = ctx.transient.borrow_mut();
        match transient.iter_mut().find(|d| d.handle.node_id == node_id) {
            Some(device) => refresh_identity(device, ident),
            None => {
                info!("sync: listing node 0x{node_id:016x}, found on our fabric");
                transient.push(new_record(handle, &ident));
                found += 1;
            }
        }
    }

    // Without a record, a device is listed only while it is on our fabric.
    ctx.transient
        .borrow_mut()
        .retain(|d| advertised.contains(&d.handle.node_id));

    *ctx.online.borrow_mut() = Some(advertised.into_iter().collect());

    Ok((found > 0)
        .then(|| format!("{found} device(s) found on our fabric were added to the list.")))
}

/// Update a device's identity from a fresh read, keeping a label the device no
/// longer serves.
fn refresh_identity(device: &mut StoredDevice, ident: DeviceIdentity) {
    if !ident.node_label.is_empty() {
        device.node_label = ident.node_label;
    }
    device.unique_id = ident.unique_id;
    device.serial_number = ident.serial_number;
    device.vendor_name = ident.vendor_name;
    device.product_name = ident.product_name;
    device.fingerprint = ident.fingerprint;
}

/// Keep a record of a listed device (see [`proto::Request::Remember`]).
fn remember<C: Crypto>(ctx: &ActorCtx<'_, C>, node_id: u64) -> proto::Response {
    let position = ctx
        .transient
        .borrow()
        .iter()
        .position(|d| d.handle.node_id == node_id);

    let Some(i) = position else {
        // Remembered already, or not listed at all.
        return match lookup_device(ctx, node_id) {
            Some(_) => list_devices(ctx),
            None => no_device(node_id),
        };
    };

    let device = ctx.transient.borrow_mut().remove(i);
    let ident = DeviceIdentity {
        unique_id: device.unique_id,
        serial_number: device.serial_number,
        node_label: device.node_label,
        vendor_name: device.vendor_name,
        product_name: device.product_name,
        fingerprint: device.fingerprint,
    };

    update_devices(ctx, |devices| adopt(devices, device.handle, &ident), None)
}

/// Add a record for a device on our fabric. A record of the same unit under
/// another node ID is stale (a device holds our fabric only once), so it is
/// replaced, keeping its tags and label.
fn adopt(devices: &mut Vec<StoredDevice>, handle: DeviceHandle, ident: &DeviceIdentity) {
    use matter_peek_core::{match_device, MatchResult};

    let mut record = new_record(handle, ident);

    if let MatchResult::Exact { node_id } = match_device(ident, devices) {
        if let Some(i) = devices.iter().position(|d| d.handle.node_id == node_id) {
            let stale = devices.remove(i);

            record.tags = stale.tags;
            if record.node_label.is_empty() {
                record.node_label = stale.node_label;
            }
        }
    }

    devices.push(record);
}

/// Note whether `node_id` is online, once a sync has run.
fn set_online<C: Crypto>(ctx: &ActorCtx<'_, C>, node_id: u64, online: bool) {
    if let Some(nodes) = ctx.online.borrow_mut().as_mut() {
        if online {
            nodes.insert(node_id);
        } else {
            nodes.remove(&node_id);
        }
    }
}

/// Replace a remembered device's local tags, then return the refreshed device
/// list (so the UI updates in one round-trip).
fn set_tags<C: Crypto>(ctx: &ActorCtx<'_, C>, node_id: u64, tags: &[String]) -> proto::Response {
    match ctx.store.set_tags(node_id, tags) {
        Ok(true) => list_devices(ctx),
        Ok(false) if lookup_device(ctx, node_id).is_some() => proto::Response::Error {
            message: format!("device 0x{node_id:016x} is not remembered; remember it to tag it"),
        },
        Ok(false) => no_device(node_id),
        Err(e) => proto::Response::Error {
            message: format!("set tags failed: {e:?}"),
        },
    }
}

/// The listed devices: the remembered ones, then the others.
fn listed_devices<C: Crypto>(ctx: &ActorCtx<'_, C>) -> Vec<StoredDevice> {
    let mut devices = ctx.store.load_devices().unwrap_or_default();
    devices.extend(ctx.transient.borrow().iter().cloned());

    devices
}

/// Find a listed device by NodeID.
fn lookup_device<C: Crypto>(ctx: &ActorCtx<'_, C>, node_id: u64) -> Option<StoredDevice> {
    listed_devices(ctx)
        .into_iter()
        .find(|d| d.handle.node_id == node_id)
}

fn no_device(node_id: u64) -> proto::Response {
    proto::Response::Error {
        message: format!("no device with node_id 0x{node_id:016x}"),
    }
}

/// How a commission reaches the device.
enum Reach {
    Ip(SocketAddr),
    Ble {
        /// Matches the device's advertisement (from its setup code).
        filter: CommissionableFilter,
        addr: Option<rs_matter::transport::network::BtAddr>,
        network: Option<proto::NetworkCredentials>,
    },
}

/// Commission a device onto our fabric, using the shared persistent stack + CA.
/// With `remember`, also keep a record of it.
async fn commission<C: Crypto>(
    cmd: &Command,
    ctx: &ActorCtx<'_, C>,
    via: &proto::CommissionVia,
    setup_code: &str,
    remember: bool,
) -> proto::Response {
    let code = match SetupCode::parse(setup_code) {
        Ok(code) => code,
        Err(e) => return invalid_setup_code(e),
    };

    let reach = match via {
        proto::CommissionVia::Ip { addr } => match addr.parse() {
            Ok(addr) => Reach::Ip(addr),
            Err(e) => {
                return proto::Response::Error {
                    message: format!("invalid peer addr: {e}"),
                }
            }
        },
        proto::CommissionVia::Ble { addr, network } => {
            let addr = match addr.as_deref().map(ble::parse_bt_addr).transpose() {
                Ok(addr) => addr,
                Err(_) => {
                    return proto::Response::Error {
                        message: format!(
                            "invalid Bluetooth address: {}",
                            addr.as_deref().unwrap_or("")
                        ),
                    }
                }
            };

            Reach::Ble {
                filter: code.filter,
                addr,
                network: network.clone(),
            }
        }
    };

    commission_flow(cmd, ctx, reach, code.passcode, remember).await
}

/// Read the setup code out of a picture of a QR code (PNG or JPEG, base64).
fn read_qr_image(image: &str) -> proto::Response {
    use rs_matter::pairing::qr::{scan, QrPayload};

    let read = || -> Result<String, String> {
        let file = BASE64
            .decode(image)
            .map_err(|e| format!("the image is not base64: {e}"))?;
        let luma = image::load_from_memory(&file)
            .map_err(|e| format!("cannot read the image: {e}"))?
            .to_luma8();

        let text = scan::scan_luma(luma.width() as _, luma.height() as _, luma.as_raw())
            .map_err(|_| "no Matter QR code found in the image".to_string())?;

        // Only a Matter onboarding payload is a setup code.
        let mut buf = [0; 128];
        QrPayload::parse(&text, &mut buf).map_err(|_| format!("not a Matter QR code: {text}"))?;

        Ok(text)
    };

    match read() {
        Ok(setup_code) => proto::Response::SetupCode { setup_code },
        Err(message) => proto::Response::Error { message },
    }
}

fn invalid_setup_code(e: Error) -> proto::Response {
    proto::Response::Error {
        message: format!(
            "invalid setup code ({:?}): expected a passcode, a manual pairing code or an MT: QR code payload",
            e.code()
        ),
    }
}

/// The commission proper, reporting each step as a [`proto::Event`]: commission,
/// then list the device and, with `remember`, keep a record of it.
async fn commission_flow<C: Crypto>(
    cmd: &Command,
    ctx: &ActorCtx<'_, C>,
    reach: Reach,
    passcode: u32,
    remember: bool,
) -> proto::Response {
    use proto::CommissionStep as Step;

    // Fresh cancel state for this run (any stale request from a prior commission
    // is cleared). The flag is set out-of-band by the server's dispatch and polled
    // at the phase boundaries below.
    ctx.cancel.set(false);

    // A NodeID never handed out before, and never to be handed out again (the
    // counter is saved right away): the device keeps it, remembered or not, and
    // even if this run goes wrong.
    let device_node_id = match ctx.store.allocate_node_id(FIRST_DEVICE_NODE_ID) {
        Ok(node_id) => node_id,
        Err(e) => {
            return proto::Response::Error {
                message: format!("cannot allocate a node ID: {e:?}"),
            }
        }
    };

    // SAFEPOINT 1 — before commission: the device has no fabric of ours yet, so
    // cancelling here is side-effect-free.
    if ctx.cancel.get() {
        return cancelled(false);
    }

    // --- Commission ---
    emit(cmd, Step::Commission, started()).await;

    let peer_addr = match reach {
        Reach::Ip(peer_addr) => peer_addr,
        Reach::Ble {
            filter,
            addr,
            network,
        } => {
            let outcome =
                commission_ble(ctx, &filter, addr, network, passcode, device_node_id).await;

            return match outcome {
                Ok(outcome) => {
                    emit(cmd, Step::Commission, done()).await;
                    commissioned(ctx, outcome, remember).await
                }
                Err(e) => {
                    emit(cmd, Step::Commission, failed(&e)).await;
                    proto::Response::Error {
                        message: format!("commission failed: {e:?}"),
                    }
                }
            };
        }
    };

    // Cancellable up to AddNOC: the closure lets `commission_cancellable` poll
    // the flag and abandon the (often-hung-on-a-closed-window) orphan sweep / PASE
    // — all pre-AddNOC, so abandoning is safe and needs no cleanup. A `Cancelled`
    // here means no device state was created.
    let cancel = ctx.cancel.clone();
    // Fabrics we still have a record for are live commissionings, not leaks —
    // the sweep must not reclaim them (that would decommission the device we are
    // about to re-commission). Everything else of ours on the device is fair game.
    let live_node_ids = live_node_ids(ctx);
    let outcome = match ctx
        .controller
        .commission_cancellable(
            &ctx.ca,
            ctx.fab_idx,
            peer_addr,
            passcode,
            device_node_id,
            &live_node_ids,
            move || cancel.get(),
        )
        .await
    {
        Ok(matter_peek_core::controller::CommissionFlow::Done(o)) => {
            emit(cmd, Step::Commission, done()).await;
            o
        }
        Ok(matter_peek_core::controller::CommissionFlow::Cancelled) => {
            emit(cmd, Step::Commission, failed_msg("cancelled")).await;
            return cancelled(false);
        }
        Err(e) => {
            emit(cmd, Step::Commission, failed(&e)).await;
            return proto::Response::Error {
                message: format!("commission failed: {e:?}"),
            };
        }
    };

    // SAFEPOINT 2 — after commission: the device now holds our fabric. We don't
    // interrupt `commission()` itself mid-protocol (it's atomic), but a cancel
    // request that arrived during it takes effect here. Leaving the fabric would
    // orphan it, so clean up by decommissioning (RemoveFabric) before returning.
    if ctx.cancel.get() {
        let removed = ctx.controller.decommission(outcome.handle).await.is_ok();
        if !removed {
            error!("actor: cancel cleanup decommission failed for the just-commissioned device");
        }
        return cancelled(removed);
    }

    commissioned(ctx, outcome, remember).await
}

/// Commission a device over BLE, provisioning it with `network`.
async fn commission_ble<C: Crypto>(
    ctx: &ActorCtx<'_, C>,
    filter: &CommissionableFilter,
    addr: Option<rs_matter::transport::network::BtAddr>,
    network: Option<proto::NetworkCredentials>,
    passcode: u32,
    device_node_id: u64,
) -> Result<matter_peek_core::CommissionOutcome, Error> {
    let dataset;
    let creds = match &network {
        None => None,
        Some(proto::NetworkCredentials::Wifi { ssid, password }) => Some(WirelessCreds::Wifi {
            ssid: ssid.as_bytes(),
            pass: password.as_bytes(),
        }),
        Some(proto::NetworkCredentials::Thread { dataset: hex }) => {
            dataset = ble::decode_hex(hex)?;
            Some(WirelessCreds::Thread {
                dataset_tlv: &dataset,
            })
        }
    };

    ble::commission_ble(
        ctx.matter,
        ctx.crypto,
        &ctx.ca,
        ctx.fab_idx,
        ctx.btp,
        filter,
        addr,
        passcode,
        device_node_id,
        creds.as_ref(),
    )
    .await
}

/// The device holds our fabric now: keep our fabric across restarts, list the
/// device and, with `remember`, keep a record of it. Re-matching reuses the
/// record of a re-commissioned unit; a fingerprint-only hit comes back as a
/// candidate to surface for linking.
async fn commissioned<C: Crypto>(
    ctx: &ActorCtx<'_, C>,
    outcome: matter_peek_core::CommissionOutcome,
    remember: bool,
) -> proto::Response {
    if let Err(e) = ctx
        .matter
        .with_state(|state| ctx.store.save_fabric(state.fabrics.fabric(ctx.fab_idx)?))
    {
        error!("actor: saving our fabric failed: {e:?}");
    }

    let rematch_candidate = if remember {
        match persist_commissioned(ctx, outcome.handle).await {
            Ok(candidate) => candidate,
            Err(e) => {
                error!("actor: remembering the commissioned device failed: {e:?}");
                None
            }
        }
    } else {
        let ident = ctx.controller.read_basic_info(outcome.handle).await;
        ctx.transient
            .borrow_mut()
            .push(new_record(outcome.handle, &ident));
        None
    };

    set_online(ctx, outcome.handle.node_id, true);

    proto::Response::Commissioned(proto::CommissionResult {
        handle: convert::handle(outcome.handle),
        rematch_candidate,
    })
}

/// Keep a record of a freshly-commissioned device, re-matching it against the
/// store so a re-commissioned unit reuses its existing record (and the user's
/// tags / label) rather than spawning a duplicate.
///
/// Returns `Some(existing_node_id)` when only the per-model fingerprint matched
/// (a [`MatchResult::Candidate`]) — surfaced so the UI can offer to *link* the
/// two; a strong-id ([`MatchResult::Exact`]) merge and a brand-new device both
/// return `None`.
async fn persist_commissioned<C: Crypto>(
    ctx: &ActorCtx<'_, C>,
    handle: DeviceHandle,
) -> Result<Option<u64>, Error> {
    use matter_peek_core::{match_device, MatchResult, StoredDevice};

    // Read the device's stable identity (best-effort) and classify it.
    let ident = ctx.controller.read_basic_info(handle).await;
    let mut devices = ctx.store.load_devices().unwrap_or_default();
    let outcome = match_device(&ident, &devices);

    // Drop any record carrying this exact node_id (a clean re-run of the same id).
    devices.retain(|d| d.handle.node_id != handle.node_id);

    let mut candidate = None;
    match outcome {
        // Strong-id match: this is the same physical unit, re-addressed. Reuse
        // its record — carry over the user's tags + node_label — and just update
        // the handle + refresh the read identity.
        MatchResult::Exact { node_id } => {
            let prior = devices.iter().position(|d| d.handle.node_id == node_id);
            let (tags, label) = prior
                .map(|i| {
                    let d = devices.remove(i);
                    (d.tags, d.node_label)
                })
                .unwrap_or_default();
            // Keep an existing user label if BasicInformation didn't serve one.
            let node_label = if ident.node_label.is_empty() {
                label
            } else {
                ident.node_label.clone()
            };
            devices.push(StoredDevice {
                handle,
                unique_id: ident.unique_id,
                serial_number: ident.serial_number,
                node_label,
                vendor_name: ident.vendor_name,
                product_name: ident.product_name,
                fingerprint: ident.fingerprint,
                tags,
            });
        }
        // Fingerprint-only: possibly the same unit, possibly a sibling of the
        // same model. Add it as a NEW record (don't silently merge) but flag the
        // candidate so the UI can offer to link them.
        MatchResult::Candidate { node_id } => {
            candidate = Some(node_id);
            devices.push(new_record(handle, &ident));
        }
        MatchResult::New => {
            devices.push(new_record(handle, &ident));
        }
    }

    ctx.store.save_devices(&devices)?;
    Ok(candidate)
}

extern crate alloc;

/// A brand-new store record (no tags yet) from a fresh identity read.
fn new_record(
    handle: DeviceHandle,
    ident: &matter_peek_core::DeviceIdentity,
) -> matter_peek_core::StoredDevice {
    matter_peek_core::StoredDevice {
        handle,
        unique_id: ident.unique_id.clone(),
        serial_number: ident.serial_number.clone(),
        node_label: ident.node_label.clone(),
        vendor_name: ident.vendor_name.clone(),
        product_name: ident.product_name.clone(),
        fingerprint: ident.fingerprint,
        tags: alloc::vec::Vec::new(),
    }
}

/// The NodeIDs of the devices we are commissioned into and mean to stay — the
/// listed ones: the orphan sweep must not remove our fabric from those (see
/// `sweep_orphaned_fabrics`).
fn live_node_ids<C: Crypto>(ctx: &ActorCtx<'_, C>) -> Vec<u64> {
    listed_devices(ctx)
        .iter()
        .map(|d| d.handle.node_id)
        .collect()
}

async fn emit(cmd: &Command, step: proto::CommissionStep, status: proto::StepStatus) {
    let _ = cmd
        .events
        .send(proto::Event::Step {
            req_id: cmd.req_id,
            step,
            status,
        })
        .await;
}

fn started() -> proto::StepStatus {
    proto::StepStatus::Started
}
fn done() -> proto::StepStatus {
    proto::StepStatus::Done
}
fn failed(e: &rs_matter::error::Error) -> proto::StepStatus {
    proto::StepStatus::Failed {
        message: format!("{e:?}"),
    }
}

fn failed_msg(msg: &str) -> proto::StepStatus {
    proto::StepStatus::Failed {
        message: msg.into(),
    }
}

/// The reply for a commission cancelled at a safepoint. `decommissioned` says
/// whether a just-installed device fabric had to be cleaned up on the way out.
fn cancelled(decommissioned: bool) -> proto::Response {
    proto::Response::Cancelled { decommissioned }
}

/// Bind an ephemeral dual-stack IPv6 UDP socket for the controller transport.
///
/// Socket creation is the binary's job (so `matter-peek-core` stays `no_std`).
/// A single `[::]:0` socket serves both IPv4 and IPv6 peers — rs-matter
/// canonicalizes IPv4-mapped-IPv6 peer addresses so session lookup matches
/// regardless of the socket's family.
fn bind_controller_socket() -> Result<async_io::Async<UdpSocket>, Error> {
    let bind_addr: SocketAddr = "[::]:0".parse().unwrap();
    let socket =
        async_io::Async::<UdpSocket>::bind(bind_addr).map_err(|_| ErrorCode::NoNetworkInterface)?;
    info!(
        "controller bound on {}",
        socket.get_ref().local_addr().unwrap()
    );
    Ok(socket)
}

/// The directory the persistence store lives in: the platform-standard
/// per-user data dir (`~/.local/share/matter-peek` on Linux,
/// `~/Library/Application Support/com.sysgrok.matter-peek` on macOS), created if
/// missing. Falls back to a `matter-peek` dir under the temp dir if no data dir
/// can be determined (e.g. `HOME` is unset) or it can't be created — the store
/// still works, it just won't survive a reboot.
fn store_dir() -> PathBuf {
    let preferred = ProjectDirs::from("com", "sysgrok", "matter-peek")
        .map(|dirs| dirs.data_dir().to_path_buf());

    if let Some(dir) = preferred {
        match std::fs::create_dir_all(&dir) {
            Ok(()) => return dir,
            Err(e) => warn!(
                "actor: could not create data dir {} ({e}); falling back to temp",
                dir.display()
            ),
        }
    } else {
        warn!("actor: no platform data dir available; falling back to temp");
    }

    std::env::temp_dir().join("matter-peek")
}
