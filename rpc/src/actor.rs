//! The Matter actor.
//!
//! One **persistent** task owns a single Matter stack for the whole process
//! lifetime: on startup it loads any persisted controller fabric
//! (`Matter::startup`) and ensures a CA ([`ControllerCa::ensure`]), then
//! runs the transport pump concurrently with a command loop. Each command is
//! serviced against the *shared* stack / CA / fabric, so persistently-
//! commissioned devices can be re-addressed over CASE without re-commissioning.
//!
//! The stack is `&'static` (via a `StaticCell`, fed `'static` device details) —
//! this sidesteps the self-referential `Matter<'a>` lifetime entirely: a
//! `&'static Matter` is freely shared between the pump and the command loop.

use core::cell::{Cell, RefCell};
use core::net::SocketAddr;
use core::num::NonZeroU8;
use core::pin::pin;

use std::collections::HashMap;
use std::net::UdpSocket;
use std::path::PathBuf;
use std::rc::Rc;

use directories::ProjectDirs;
use embassy_futures::select::{select, select4, Either, Either4};
use futures_util::stream::{FuturesUnordered, StreamExt};

use log::{error, info, warn};

use rs_matter::crypto::{default_crypto, Crypto};
use rs_matter::dm::devices::test::{DAC_PRIVKEY, TEST_DEV_ATT, TEST_DEV_COMM, TEST_DEV_DET};
use rs_matter::error::{Error, ErrorCode};
use rs_matter::persist::DirKvBlobStore;
use rs_matter::transport::network::mdns::CommissionableFilter;
use rs_matter::transport::network::NoNetwork;
use rs_matter::utils::init::InitMaybeUninit;
use rs_matter::Matter;

use static_cell::StaticCell;

use matter_peek_core::ca::{CaTier, ControllerCa};
use matter_peek_core::controller::FabricIdentity;
use matter_peek_core::persist::{kv_buf, ControllerStore};
use matter_peek_core::{Controller, DeviceHandle, DiagSet, LogIntent};

use matter_peek_proto as proto;

use crate::convert;

/// The device NodeID the controller assigns to the **next** newly-commissioned
/// device. Persistent mode allocates a fresh id per device (Phase 1: a simple
/// incrementing counter seeded past the store's max).
const FIRST_DEVICE_NODE_ID: u64 = 112234;

/// The persistent controller stack, fed `'static` device details so `Matter`'s
/// lifetime is `'static` and it can be shared freely across tasks.
static PERSIST_MATTER: StaticCell<Matter> = StaticCell::new();

/// A cooperative cancel flag for the in-flight vertical, shared (single-threaded,
/// so `Rc<Cell>` — no atomics) between the server's dispatch (which sets it when
/// a [`proto::Request::Cancel`] arrives, bypassing the serial command queue) and
/// the actor (whose vertical polls it at phase boundaries). See [`vertical_flow`].
pub type CancelFlag = Rc<Cell<bool>>;

/// A unit of work handed to the actor, with the channels to report back on.
pub struct Command {
    pub req_id: proto::ReqId,
    pub request: proto::Request,
    /// Progress events for this command (per-command; the relay closes it when
    /// the command's reply lands — used by the vertical's `Step` events).
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

/// Run the persistent actor: stand up one Matter stack for the process, restore
/// persisted state, ensure the controller CA + fabric, then service commands
/// against that shared stack until the channel closes.
pub async fn run(rx: async_channel::Receiver<Command>, cancel: CancelFlag) {
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
    let dir = store_dir();
    info!("actor: persistence store at {}", dir.display());
    let kv_buf = kv_buf();
    let store = ControllerStore::new(DirKvBlobStore::new(dir), &kv_buf);
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

    // Channel over which the command handler hands new subscription *jobs* to the
    // concurrent driver. Jobs carry only owned data, so this is `'static` and can
    // be declared before `ctx`. Unbounded so `Subscribe` never blocks the loop.
    let (sub_submit, sub_recv) = async_channel::unbounded::<SubJob>();

    let ctx = ActorCtx {
        matter,
        controller,
        ca,
        fab_idx,
        store,
        cancel,
        subs: RefCell::new(HashMap::new()),
    };

    // Four concurrent arms, for the actor's lifetime:
    //   1. the transport pump (`matter.run`),
    //   2. the command loop (services requests one at a time),
    //   3. the subscription driver (polls all live subscriptions concurrently), and
    //   4. the mDNS backend, which services **operational resolves** — CASE
    //      re-addressing of a persisted device (subscribe / peek / decommission
    //      after its secure session lapses) needs a running resolver; without it
    //      every such resolve times out with `NotFound`. (It also services
    //      discovery browses, so those must NOT start a second backend — the
    //      `discover` handler uses the `_bg` variant.)
    // Because subscriptions run in their own arm — not inside a command — the
    // command loop stays free while any number of subscriptions stream, so a
    // `Subscribe` returns immediately and multiple devices can be watched at once.
    let mut pump = pin!(matter.run(&crypto, &socket, &socket, NoNetwork));
    let mut loop_ = pin!(command_loop(rx, &ctx, &sub_submit));
    let mut subs = pin!(run_subscriptions(sub_recv, &ctx));
    let mut mdns = pin!(ctx.controller.run_mdns_backend());

    match select4(&mut pump, &mut loop_, &mut subs, &mut mdns).await {
        Either4::First(r) => error!("actor: transport pump exited: {r:?}"),
        Either4::Second(()) => info!("actor: command channel closed, exiting"),
        Either4::Third(()) => info!("actor: subscription driver exited"),
        Either4::Fourth(r) => error!("actor: mDNS backend exited: {r:?}"),
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
        out,
        stop_rx,
        my_stop,
    } = job;

    let seq = Cell::new(0u64);
    let out_reports = out.clone();
    let out_state = out.clone();

    let subscription = ctx.controller.subscribe_health(
        handle,
        DiagSet::ALL,
        move |health| {
            seq.set(seq.get() + 1);
            let _ = out_reports.try_send(proto::ServerMsg::Event {
                event: proto::Event::HealthReport {
                    node_id,
                    seq: seq.get(),
                    health: convert::health(health),
                },
            });
        },
        move |state| {
            let _ = out_state.try_send(proto::ServerMsg::Event {
                event: proto::Event::SubscriptionState {
                    node_id,
                    state: convert::sub_state(state),
                },
            });
        },
    );

    // Stop when EITHER the explicit stop signal fires OR the connection's sink
    // closes (client disconnected — don't leak the device-side subscription).
    let stop = async {
        let _ = select(stop_rx.recv(), out.closed()).await;
    };
    let _ = select(subscription, stop).await;

    // Drop our registry entry, but only if it's still *ours* (a quick
    // unsubscribe+resubscribe of the same device could have replaced it).
    {
        let mut map = ctx.subs.borrow_mut();
        if map.get(&node_id).is_some_and(|s| s.same_channel(&my_stop)) {
            map.remove(&node_id);
        }
    }

    // Terminal state (delivered only if the connection is still up).
    let _ = out.try_send(proto::ServerMsg::Event {
        event: proto::Event::SubscriptionState {
            node_id,
            state: proto::SubscriptionState::Stopped,
        },
    });
}

/// Shared state every command handler operates on.
struct ActorCtx<'a, C: Crypto> {
    matter: &'a Matter<'a>,
    controller: Controller<'a, C>,
    ca: ControllerCa,
    fab_idx: NonZeroU8,
    store: ControllerStore<'a, DirKvBlobStore>,
    /// Cooperative cancel flag for the in-flight vertical (set by the server's
    /// dispatch, polled by [`vertical_flow`] at phase boundaries).
    cancel: CancelFlag,
    /// Live health subscriptions, keyed by device `node_id`. The value is the
    /// **stop** sender: send `()` (or drop it) to make the subscription task's
    /// `select` complete, dropping the `subscribe_health` future and tearing the
    /// subscription down. Present ⇒ that device is being watched.
    subs: RefCell<HashMap<u64, async_channel::Sender<()>>>,
}

/// A subscription request submitted to the concurrent driver. Carries only owned
/// data (no borrow of the shared stack), so the submission channel is `'static`;
/// the driver itself borrows the stack (`&ActorCtx`) to build+run the future.
struct SubJob {
    node_id: u64,
    handle: DeviceHandle,
    /// The connection's outbound sink — where this subscription streams frames.
    out: async_channel::Sender<proto::ServerMsg>,
    /// Fires (or its sender drops) to stop the subscription.
    stop_rx: async_channel::Receiver<()>,
    /// A clone of the registry stop-sender, so the driver can verify at cleanup
    /// that the registry entry is still this subscription before removing it.
    my_stop: async_channel::Sender<()>,
}

/// Pull commands and service them one at a time against the shared stack.
///
/// `ctx` is borrowed for the actor's whole lifetime (`'a`) because a `Subscribe`
/// submits a subscription future that borrows `ctx.controller` for that long.
async fn command_loop<C: Crypto>(
    rx: async_channel::Receiver<Command>,
    ctx: &ActorCtx<'_, C>,
    sub_submit: &async_channel::Sender<SubJob>,
) {
    while let Ok(cmd) = rx.recv().await {
        let response = service(&cmd, ctx, sub_submit).await;
        // Best-effort: if the client vanished, dropping the reply is fine.
        let _ = cmd.reply.send(response).await;
    }
}

/// Service one command against the shared stack, emitting step events as it goes.
async fn service<C: Crypto>(
    cmd: &Command,
    ctx: &ActorCtx<'_, C>,
    sub_submit: &async_channel::Sender<SubJob>,
) -> proto::Response {
    match &cmd.request {
        proto::Request::RunVertical {
            peer_addr,
            passcode,
            policy,
        } => run_vertical(cmd, ctx, peer_addr, *passcode, *policy).await,
        proto::Request::Discover { discriminator, max } => {
            run_discover(ctx, *discriminator, *max).await
        }
        proto::Request::ListDevices => list_devices(ctx),
        proto::Request::PeekDevice { node_id, with_logs } => {
            peek_device(ctx, *node_id, *with_logs).await
        }
        proto::Request::Identify { node_id, secs } => identify(ctx, *node_id, *secs).await,
        proto::Request::Subscribe { node_id } => subscribe(cmd, ctx, sub_submit, *node_id).await,
        proto::Request::Unsubscribe { node_id } => unsubscribe(ctx, *node_id),
        proto::Request::Decommission { node_id, force } => {
            decommission(ctx, *node_id, *force).await
        }
        proto::Request::SetTags { node_id, tags } => set_tags(ctx, *node_id, tags),
        // `Cancel` is normally intercepted by the server (it sets the shared flag
        // out-of-band rather than enqueuing). If one reaches here, no vertical is
        // running — clear any stale flag and ack.
        proto::Request::Cancel => {
            ctx.cancel.set(false);
            proto::Response::Ok
        }
    }
}

/// Browse for commissionable devices and return a pick-list. Uses the shared
/// stack's transport/mDNS rendezvous (the actor's pump is already running).
async fn run_discover<C: Crypto>(
    ctx: &ActorCtx<'_, C>,
    discriminator: Option<u16>,
    max: u8,
) -> proto::Response {
    let filter = CommissionableFilter {
        discriminator,
        ..Default::default()
    };

    // The actor already runs the mDNS backend (a permanent arm); use the `_bg`
    // variant so discovery doesn't start a competing second backend.
    let devices = ctx
        .controller
        .discover_all_bg(&filter, max.max(1), None)
        .await
        .into_iter()
        .map(|d| proto::DiscoveredDevice {
            addr: match d.address {
                rs_matter::transport::network::Address::Udp(a) => a.to_string(),
                other => format!("{other:?}"),
            },
            commissionable_id: d.commissionable_id,
        })
        .collect();

    proto::Response::Discovered { devices }
}

/// List the persistently-commissioned devices from the store.
fn list_devices<C: Crypto>(ctx: &ActorCtx<'_, C>) -> proto::Response {
    match ctx.store.load_devices() {
        Ok(devices) => proto::Response::Devices {
            devices: devices.into_iter().map(convert::stored_device).collect(),
        },
        Err(e) => proto::Response::Error {
            message: format!("load devices failed: {e:?}"),
        },
    }
}

/// Re-address a persisted device by NodeID over CASE (no re-commission) and read
/// its health (+ optional logs).
async fn peek_device<C: Crypto>(
    ctx: &ActorCtx<'_, C>,
    node_id: u64,
    with_logs: bool,
) -> proto::Response {
    let Some(dev) = lookup_stored(ctx, node_id) else {
        return proto::Response::Error {
            message: format!("no stored device with node_id 0x{node_id:016x}"),
        };
    };

    let health = ctx.controller.read_health(dev.handle, DiagSet::ALL).await;
    let crash_log_bytes = if with_logs {
        ctx.controller
            .retrieve_logs(dev.handle, LogIntent::CrashLogs)
            .await
            .ok()
            .map(|b| b.len())
    } else {
        None
    };

    proto::Response::Peek {
        handle: convert::handle(dev.handle),
        health: convert::health(health),
        crash_log_bytes,
    }
}

/// Blink a persisted device (Identify cluster) on its application endpoint.
async fn identify<C: Crypto>(ctx: &ActorCtx<'_, C>, node_id: u64, secs: u16) -> proto::Response {
    let Some(dev) = lookup_stored(ctx, node_id) else {
        return proto::Response::Error {
            message: format!("no stored device with node_id 0x{node_id:016x}"),
        };
    };
    match ctx.controller.identify(dev.handle, 1, secs).await {
        Ok(()) => proto::Response::Ok,
        Err(e) => proto::Response::Error {
            message: format!("identify failed: {e:?}"),
        },
    }
}

/// Start a live health subscription to a persisted device and reply
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
    let Some(dev) = lookup_stored(ctx, node_id) else {
        return proto::Response::Error {
            message: format!("no stored device with node_id 0x{node_id:016x}"),
        };
    };

    // Already watching this device? No-op success (the existing subscription
    // keeps streaming).
    if ctx.subs.borrow().contains_key(&node_id) {
        return proto::Response::Subscribed {
            node_id,
            already: true,
        };
    }

    // Events stream to the connection's outbound sink, which outlives this
    // command. Without one (non-WS caller) there's nowhere to stream, so decline.
    let Some(out) = cmd.out.clone() else {
        return proto::Response::Error {
            message: "subscriptions require a streaming connection".into(),
        };
    };

    // Stop signal: `Unsubscribe` (or connection teardown) fires it. Register the
    // sender under `node_id`; `my_stop` (a clone) lets the driver verify ownership
    // at cleanup.
    let (stop_tx, stop_rx) = async_channel::bounded::<()>(1);
    let my_stop = stop_tx.clone();
    ctx.subs.borrow_mut().insert(node_id, stop_tx);

    // Hand a job (owned data only) to the driver, which builds+runs the actual
    // subscription against the shared stack. Reply immediately.
    let job = SubJob {
        node_id,
        handle: dev.handle,
        out,
        stop_rx,
        my_stop,
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

/// Stop a live subscription: fire its stop signal (which ends the subscription
/// task) and forget it. Idempotent — unsubscribing an unknown device is `Ok`.
fn unsubscribe<C: Crypto>(ctx: &ActorCtx<'_, C>, node_id: u64) -> proto::Response {
    if let Some(stop) = ctx.subs.borrow_mut().remove(&node_id) {
        // Best-effort: send the stop; if the receiver is already gone the task is
        // ending anyway. Dropping `stop` here also closes the channel, which the
        // task's `recv()` sees as `Err` — either way it wakes and completes.
        let _ = stop.try_send(());
        info!("unsubscribed node 0x{node_id:016x}");
    }
    proto::Response::Ok
}

/// Decommission a persisted device: stop any live subscription, `RemoveFabric`
/// our fabric on the device (freeing its fabric slot), then drop the local
/// record. Returns the refreshed device list.
///
/// If the device is unreachable the `RemoveFabric` can't happen; with `force` we
/// still drop the local record (leaving a device-side orphan fabric — the honest
/// trade-off, surfaced to the user), otherwise we report the error and keep the
/// record so it can be retried.
async fn decommission<C: Crypto>(
    ctx: &ActorCtx<'_, C>,
    node_id: u64,
    force: bool,
) -> proto::Response {
    let Some(dev) = lookup_stored(ctx, node_id) else {
        return proto::Response::Error {
            message: format!("no stored device with node_id 0x{node_id:016x}"),
        };
    };

    // Stop any live subscription first (its CASE session is about to be torn down).
    if let Some(stop) = ctx.subs.borrow_mut().remove(&node_id) {
        let _ = stop.try_send(());
    }

    // Remove our fabric on the device (over CASE). On failure, only proceed to
    // drop the local record if the caller forced it.
    match ctx.controller.decommission(dev.handle).await {
        Ok(()) => info!("decommissioned node 0x{node_id:016x} (RemoveFabric ok)"),
        Err(e) if force => {
            warn!(
                "decommission node 0x{node_id:016x}: RemoveFabric failed ({e:?}); forcing local removal (device-side fabric leaked)"
            );
        }
        Err(e) => {
            return proto::Response::Error {
                message: format!(
                    "device unreachable — RemoveFabric failed: {e:?}. Retry, or force-remove to drop the local record anyway."
                ),
            };
        }
    }

    // Drop the local record (load-all / filter / save-all).
    match ctx.store.load_devices() {
        Ok(mut devices) => {
            devices.retain(|d| d.handle.node_id != node_id);
            if let Err(e) = ctx.store.save_devices(&devices) {
                return proto::Response::Error {
                    message: format!("failed to update store: {e:?}"),
                };
            }
        }
        Err(e) => {
            return proto::Response::Error {
                message: format!("failed to load store: {e:?}"),
            };
        }
    }

    list_devices(ctx)
}

/// Replace a stored device's local tags, then return the refreshed device list
/// (so the UI updates in one round-trip).
fn set_tags<C: Crypto>(ctx: &ActorCtx<'_, C>, node_id: u64, tags: &[String]) -> proto::Response {
    match ctx.store.set_tags(node_id, tags) {
        Ok(true) => list_devices(ctx),
        Ok(false) => proto::Response::Error {
            message: format!("no stored device with node_id 0x{node_id:016x}"),
        },
        Err(e) => proto::Response::Error {
            message: format!("set tags failed: {e:?}"),
        },
    }
}

/// Find a stored device by NodeID.
fn lookup_stored<C: Crypto>(
    ctx: &ActorCtx<'_, C>,
    node_id: u64,
) -> Option<matter_peek_core::StoredDevice> {
    ctx.store
        .load_devices()
        .ok()?
        .into_iter()
        .find(|d| d.handle.node_id == node_id)
}

/// Run the full vertical against a freshly-addressed peer, using the shared
/// persistent stack + CA. `Persistent` policy keeps the fabric and records the
/// device in the store; `Ephemeral` removes it afterward.
async fn run_vertical<C: Crypto>(
    cmd: &Command,
    ctx: &ActorCtx<'_, C>,
    peer_addr: &str,
    passcode: u32,
    policy: proto::FabricPolicy,
) -> proto::Response {
    let peer_addr: SocketAddr = match peer_addr.parse() {
        Ok(a) => a,
        Err(e) => {
            return proto::Response::Error {
                message: format!("invalid peer addr: {e}"),
            }
        }
    };

    vertical_flow(cmd, ctx, peer_addr, passcode, policy).await
}

/// The vertical proper: commission → health → logs → (persist | decommission),
/// reporting each step as a [`proto::Event`].
async fn vertical_flow<C: Crypto>(
    cmd: &Command,
    ctx: &ActorCtx<'_, C>,
    peer_addr: SocketAddr,
    passcode: u32,
    policy: proto::FabricPolicy,
) -> proto::Response {
    use proto::VerticalStep as Step;

    // Fresh cancel state for this run (any stale request from a prior vertical is
    // cleared). The flag is set out-of-band by the server's dispatch and polled
    // at the phase boundaries below.
    ctx.cancel.set(false);

    // Allocate the device's NodeID: one past the current store max (persistent),
    // so re-runs don't collide; ephemeral reuses the base id (removed anyway).
    let device_node_id = next_device_node_id(ctx);

    // SAFEPOINT 1 — before commission: the device has no fabric of ours yet, so
    // cancelling here is side-effect-free.
    if ctx.cancel.get() {
        return cancelled(false);
    }

    // --- Commission ---
    // Cancellable up to AddNOC: the closure lets `commission_cancellable` poll
    // the flag and abandon the (often-hung-on-a-closed-window) orphan sweep / PASE
    // — all pre-AddNOC, so abandoning is safe and needs no cleanup. A `Cancelled`
    // here means no device state was created.
    emit(cmd, Step::Commission, started()).await;
    let cancel = ctx.cancel.clone();
    let outcome = match ctx
        .controller
        .commission_cancellable(
            &ctx.ca,
            ctx.fab_idx,
            peer_addr,
            passcode,
            device_node_id,
            convert::policy(policy),
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

    // --- Read health ---
    emit(cmd, Step::ReadHealth, started()).await;
    let health = ctx
        .controller
        .read_health(outcome.handle, DiagSet::ALL)
        .await;
    emit(cmd, Step::ReadHealth, done()).await;

    // --- Retrieve CrashLogs (soft-fail: a device may simply have none) ---
    emit(cmd, Step::RetrieveLogs, started()).await;
    let crash_log_bytes = match ctx
        .controller
        .retrieve_logs(outcome.handle, LogIntent::CrashLogs)
        .await
    {
        Ok(bytes) => {
            emit(cmd, Step::RetrieveLogs, done()).await;
            Some(bytes.len())
        }
        Err(e) => {
            emit(cmd, Step::RetrieveLogs, failed(&e)).await;
            None
        }
    };

    // SAFEPOINT 3 — after the (read-only) health/logs phases, before we decide
    // the device's fate. A cancel that arrived during those reads is honored
    // here: the device is decommissioned regardless of policy (don't persist a
    // device the user asked to cancel), leaving nothing behind.
    if ctx.cancel.get() {
        let removed = ctx.controller.decommission(outcome.handle).await.is_ok();
        if !removed {
            error!("actor: cancel cleanup decommission failed");
        }
        return cancelled(removed);
    }

    // --- Persist (Persistent) | Decommission (Ephemeral) ---
    let mut rematch_candidate = None;
    let decommissioned = match policy {
        proto::FabricPolicy::Persistent => {
            // Keep the device: persist our fabric (extended by AddNOC) + a store
            // record (with BasicInformation identity) so it can be re-addressed
            // over CASE later without re-commissioning. Re-matching reuses the
            // record of a re-commissioned unit; a fingerprint-only hit comes back
            // as a candidate to surface for linking.
            match persist_commissioned(ctx, outcome.handle).await {
                Ok(candidate) => rematch_candidate = candidate,
                Err(e) => error!("actor: persisting commissioned device failed: {e:?}"),
            }
            false
        }
        proto::FabricPolicy::Ephemeral => {
            emit(cmd, Step::Decommission, started()).await;
            match ctx.controller.decommission(outcome.handle).await {
                Ok(()) => {
                    emit(cmd, Step::Decommission, done()).await;
                    true
                }
                Err(e) => {
                    emit(cmd, Step::Decommission, failed(&e)).await;
                    false
                }
            }
        }
    };

    proto::Response::Vertical(proto::VerticalResult {
        handle: convert::handle(outcome.handle),
        health: convert::health(health),
        crash_log_bytes,
        decommissioned,
        rematch_candidate,
    })
}

/// Persist a freshly-commissioned device, re-matching it against the store so a
/// re-commissioned unit reuses its existing record (and the user's tags / label)
/// rather than spawning a duplicate.
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

    // Save the updated controller fabric (it now lists the new device's NOC).
    ctx.matter
        .with_state(|state| ctx.store.save_fabric(state.fabrics.fabric(ctx.fab_idx)?))?;

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

/// Next device NodeID: one past the current store max, so persistent re-runs
/// don't collide. Ephemeral runs are fine reusing the base (fabric is removed).
fn next_device_node_id<C: Crypto>(ctx: &ActorCtx<'_, C>) -> u64 {
    ctx.store
        .load_devices()
        .ok()
        .and_then(|d| d.iter().map(|x| x.handle.node_id).max())
        .map(|m| m + 1)
        .unwrap_or(FIRST_DEVICE_NODE_ID)
}

async fn emit(cmd: &Command, step: proto::VerticalStep, status: proto::StepStatus) {
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

/// The reply for a vertical cancelled at a safepoint. `decommissioned` says
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
