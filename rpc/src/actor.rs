//! The Matter actor.
//!
//! A long-lived task owns the channel of incoming [`Command`]s and, for each,
//! stands up a Matter stack + [`Controller`], runs the transport pump
//! concurrently with the requested flow, and reports progress via the command's
//! event sender plus a final reply on its oneshot.
//!
//! Why a fresh stack per command (rather than one shared stack): the controller
//! installs a *local* fabric on every `commission`, and a single shared stack
//! would accumulate controller-side fabric slots across runs. Scoping the stack
//! to one vertical run mirrors the proven CLI flow exactly and keeps the
//! self-referential `Matter<'a>` lifetimes contained in one async scope. (A
//! persistent-fabric mode later will keep one stack alive deliberately.)

use core::pin::pin;

use embassy_futures::select::{select, Either};

use log::{error, info};

use rs_matter::crypto::default_crypto;
use rs_matter::dm::devices::test::{DAC_PRIVKEY, TEST_DEV_ATT, TEST_DEV_COMM, TEST_DEV_DET};
use rs_matter::error::ErrorCode;
use rs_matter::transport::network::mdns::CommissionableFilter;
use rs_matter::transport::network::NoNetwork;
use rs_matter::Matter;

use matter_peek_core::controller::{bind_controller_socket, FabricIdentity};
use matter_peek_core::{Controller, DiagSet, LogIntent};

use matter_peek_proto as proto;

use crate::convert;

/// The device NodeID the controller assigns on its fabric (Phase 1 fixed value).
const DEVICE_NODE_ID: u64 = 112234;

/// A unit of work handed to the actor, with the channels to report back on.
pub struct Command {
    pub req_id: proto::ReqId,
    pub request: proto::Request,
    /// Progress events for this command.
    pub events: async_channel::Sender<proto::Event>,
    /// The single final reply.
    pub reply: async_channel::Sender<proto::Response>,
}

/// Run the actor loop: pull commands off `rx` and service them one at a time.
///
/// Serial execution is intentional for Phase 1 — one Matter stack / radio at a
/// time. Concurrency across devices is a later concern.
pub async fn run(rx: async_channel::Receiver<Command>) {
    while let Ok(cmd) = rx.recv().await {
        let response = service(&cmd).await;
        // Best-effort: if the client vanished, dropping the reply is fine.
        let _ = cmd.reply.send(response).await;
    }
    info!("actor: command channel closed, exiting");
}

/// Service one command, emitting step events along the way.
async fn service(cmd: &Command) -> proto::Response {
    match &cmd.request {
        proto::Request::RunVertical {
            peer_addr,
            passcode,
            policy,
        } => run_vertical(cmd, peer_addr, *passcode, *policy).await,
        proto::Request::Discover { discriminator, max } => run_discover(*discriminator, *max).await,
    }
}

/// Browse for commissionable devices and return a pick-list. Discovery uses the
/// mDNS backend's own socket/DBus path (run inside `discover_all`), so it needs
/// no Matter transport pump.
async fn run_discover(discriminator: Option<u16>, max: u8) -> proto::Response {
    let crypto = default_crypto(rand::thread_rng(), DAC_PRIVKEY);
    let matter = Matter::new(&TEST_DEV_DET, TEST_DEV_COMM, &TEST_DEV_ATT, 0);
    let controller = Controller::new(&matter, &crypto, FabricIdentity::default());

    let filter = CommissionableFilter {
        discriminator,
        ..Default::default()
    };

    let devices = controller
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

async fn run_vertical(
    cmd: &Command,
    peer_addr: &str,
    passcode: u32,
    policy: proto::FabricPolicy,
) -> proto::Response {
    let peer_addr = match peer_addr.parse() {
        Ok(a) => a,
        Err(e) => {
            return proto::Response::Error {
                message: format!("invalid peer addr: {e}"),
            }
        }
    };

    let socket = match bind_controller_socket() {
        Ok(s) => s,
        Err(e) => {
            return proto::Response::Error {
                message: format!("bind failed: {e:?}"),
            }
        }
    };

    let crypto = default_crypto(rand::thread_rng(), DAC_PRIVKEY);

    // Fresh per-command stack (see module docs), as a plain stack local so the
    // self-referential `Matter<'a>` borrows resolve to this scope — the same
    // construction rs-matter's own e2e tests use (`Matter::new`, not boxed/
    // `StaticCell`, which would pin the lifetime to `'static`).
    let matter = Matter::new(&TEST_DEV_DET, TEST_DEV_COMM, &TEST_DEV_ATT, 0);
    let controller = Controller::new(&matter, &crypto, FabricIdentity::default());

    // Run the transport pump concurrently with the flow; whichever finishes
    // first ends the scope (the flow normally finishes; the pump only ends on
    // error).
    let mut transport = pin!(matter.run(&crypto, &socket, &socket, NoNetwork));
    let mut flow = pin!(vertical_flow(cmd, &controller, peer_addr, passcode, policy));

    match select(&mut transport, &mut flow).await {
        Either::First(r) => {
            error!("actor: transport exited prematurely: {r:?}");
            proto::Response::Error {
                message: "transport exited prematurely".into(),
            }
        }
        Either::Second(resp) => resp,
    }
}

/// The actual vertical, identical in spirit to the CLI flow but reporting each
/// step as a [`proto::Event`].
async fn vertical_flow<C: rs_matter::crypto::Crypto>(
    cmd: &Command,
    controller: &Controller<'_, C>,
    peer_addr: std::net::SocketAddr,
    passcode: u32,
    policy: proto::FabricPolicy,
) -> proto::Response {
    use proto::VerticalStep as Step;

    // --- Commission ---
    emit(cmd, Step::Commission, started()).await;
    let outcome = match controller
        .commission(peer_addr, passcode, DEVICE_NODE_ID, convert::policy(policy))
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
    let health = controller.read_health(outcome.handle, DiagSet::ALL).await;
    emit(cmd, Step::ReadHealth, done()).await;

    // --- Retrieve CrashLogs (soft-fail: a device may simply have none) ---
    emit(cmd, Step::RetrieveLogs, started()).await;
    let crash_log_bytes = match controller
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

    // --- Decommission (ephemeral self-destruct) ---
    let decommissioned = if matches!(policy, proto::FabricPolicy::Ephemeral) {
        emit(cmd, Step::Decommission, started()).await;
        match controller.decommission(outcome.handle).await {
            Ok(()) => {
                emit(cmd, Step::Decommission, done()).await;
                true
            }
            Err(e) => {
                emit(cmd, Step::Decommission, failed(&e)).await;
                false
            }
        }
    } else {
        false
    };

    proto::Response::Vertical(proto::VerticalResult {
        handle: convert::handle(outcome.handle),
        health: convert::health(health),
        crash_log_bytes,
        decommissioned,
    })
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

// Keep `ErrorCode` import meaningful even if the transport branch's message
// changes — referenced in the prematurely-exited path's intent.
#[allow(unused_imports)]
use ErrorCode as _;
