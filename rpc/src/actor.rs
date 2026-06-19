//! The Matter actor.
//!
//! One **persistent** task owns a single Matter stack for the whole process
//! lifetime: on startup it loads any persisted controller fabric
//! (`Matter::load_persist`) and ensures a CA ([`ControllerCa::ensure`]), then
//! runs the transport pump concurrently with a command loop. Each command is
//! serviced against the *shared* stack / CA / fabric, so persistently-
//! commissioned devices can be re-addressed over CASE without re-commissioning.
//!
//! The stack is `&'static` (via a `StaticCell`, fed `'static` device details) —
//! this sidesteps the self-referential `Matter<'a>` lifetime entirely: a
//! `&'static Matter` is freely shared between the pump and the command loop.

use core::net::SocketAddr;
use core::num::NonZeroU8;
use core::pin::pin;

use std::net::UdpSocket;

use embassy_futures::select::{select, Either};

use log::{error, info};

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
use matter_peek_core::persist::ControllerStore;
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

/// A unit of work handed to the actor, with the channels to report back on.
pub struct Command {
    pub req_id: proto::ReqId,
    pub request: proto::Request,
    /// Progress events for this command.
    pub events: async_channel::Sender<proto::Event>,
    /// The single final reply.
    pub reply: async_channel::Sender<proto::Response>,
}

/// Run the persistent actor: stand up one Matter stack for the process, restore
/// persisted state, ensure the controller CA + fabric, then service commands
/// against that shared stack until the channel closes.
pub async fn run(rx: async_channel::Receiver<Command>) {
    let crypto = default_crypto(rand::thread_rng(), DAC_PRIVKEY);

    // One `&'static` stack for the whole process (see module docs). Kept `&mut`
    // just long enough to `load_persist`, then reborrowed immutably for sharing.
    let matter: &'static mut Matter<'static> = PERSIST_MATTER.uninit().init_with(Matter::init(
        &TEST_DEV_DET,
        TEST_DEV_COMM,
        &TEST_DEV_ATT,
        0,
    ));

    // Directory-backed KV (desktop). `load_persist` consumes a `KvBlobStore` by
    // value, so hand it a clone (cheap — just a path) and keep one for the store.
    let dir_kv = DirKvBlobStore::new_default();
    if let Err(e) = matter.load_persist(dir_kv.clone(), &mut [0u8; 4096]).await {
        error!("actor: load_persist failed: {e:?} (continuing with fresh state)");
    }
    let matter: &'static Matter<'static> = matter;

    let store = ControllerStore::new(dir_kv);
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
    let ctx = ActorCtx {
        matter,
        controller,
        ca,
        fab_idx,
        store,
    };

    // Transport pump + command loop, concurrently, for the actor's lifetime.
    // Named `pin!` bindings + `&mut` in `select` (rather than `select(pin!(a),
    // pin!(b))`) so the borrows of `crypto`/`socket`/`ctx` outlive the pinned
    // futures' drops.
    let mut pump = pin!(matter.run(&crypto, &socket, &socket, NoNetwork));
    let mut loop_ = pin!(command_loop(rx, &ctx));

    match select(&mut pump, &mut loop_).await {
        Either::First(r) => error!("actor: transport pump exited: {r:?}"),
        Either::Second(()) => info!("actor: command channel closed, exiting"),
    }
}

/// Shared state every command handler operates on.
struct ActorCtx<'a, C: Crypto> {
    matter: &'a Matter<'a>,
    controller: Controller<'a, C>,
    ca: ControllerCa,
    fab_idx: NonZeroU8,
    store: ControllerStore<DirKvBlobStore>,
}

/// Pull commands and service them one at a time against the shared stack.
async fn command_loop<C: Crypto>(rx: async_channel::Receiver<Command>, ctx: &ActorCtx<'_, C>) {
    while let Ok(cmd) = rx.recv().await {
        let response = service(&cmd, ctx).await;
        // Best-effort: if the client vanished, dropping the reply is fine.
        let _ = cmd.reply.send(response).await;
    }
}

/// Service one command against the shared stack, emitting step events as it goes.
async fn service<C: Crypto>(cmd: &Command, ctx: &ActorCtx<'_, C>) -> proto::Response {
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

    let devices = ctx
        .controller
        .discover_all(&filter, max.max(1), None)
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

    // Allocate the device's NodeID: one past the current store max (persistent),
    // so re-runs don't collide; ephemeral reuses the base id (removed anyway).
    let device_node_id = next_device_node_id(ctx);

    // --- Commission ---
    emit(cmd, Step::Commission, started()).await;
    let outcome = match ctx
        .controller
        .commission(
            &ctx.ca,
            ctx.fab_idx,
            peer_addr,
            passcode,
            device_node_id,
            convert::policy(policy),
        )
        .await
    {
        Ok(o) => {
            emit(cmd, Step::Commission, done()).await;
            o
        }
        Err(e) => {
            emit(cmd, Step::Commission, failed(&e)).await;
            return proto::Response::Error {
                message: format!("commission failed: {e:?}"),
            };
        }
    };

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

    // --- Persist (Persistent) | Decommission (Ephemeral) ---
    let decommissioned = match policy {
        proto::FabricPolicy::Persistent => {
            // Keep the device: persist our fabric (extended by AddNOC) + a store
            // record (with BasicInformation identity) so it can be re-addressed
            // over CASE later without re-commissioning.
            if let Err(e) = persist_commissioned(ctx, outcome.handle).await {
                error!("actor: persisting commissioned device failed: {e:?}");
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
    })
}

/// Persist a just-commissioned device under the `Persistent` policy: save our
/// (now device-bearing) fabric and a store record with the device's stable
/// identity read from BasicInformation.
async fn persist_commissioned<C: Crypto>(
    ctx: &ActorCtx<'_, C>,
    handle: DeviceHandle,
) -> Result<(), Error> {
    // Save the updated controller fabric (it now lists the new device's NOC).
    ctx.matter
        .with_state(|state| ctx.store.save_fabric(state.fabrics.fabric(ctx.fab_idx)?))?;

    // Read the device's stable identity (best-effort) and upsert the record.
    let ident = ctx.controller.read_basic_info(handle).await;
    let mut devices = ctx.store.load_devices().unwrap_or_default();
    devices.retain(|d| d.handle.node_id != handle.node_id);
    devices.push(matter_peek_core::StoredDevice {
        handle,
        unique_id: ident.unique_id,
        serial_number: ident.serial_number,
        node_label: ident.node_label,
        tags: alloc_vec_new(),
    });
    ctx.store.save_devices(&devices)
}

extern crate alloc;
fn alloc_vec_new() -> alloc::vec::Vec<alloc::string::String> {
    alloc::vec::Vec::new()
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
