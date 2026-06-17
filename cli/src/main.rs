//! matter-peek CLI — the Phase 0 vertical spike.
//!
//! Drives [`matter_peek_core::Controller::commission`] against an
//! externally-running Matter accessory. Defaults match rs-matter's test fixture
//! (passcode `20202021`, peer `[::1]:5540`), so it works out-of-the-box against
//! `chip-all-clusters-app` or an rs-matter example device.
//!
//! ```text
//! matter-peek [PASSCODE] [PEER_ADDR]
//! ```
//!
//! On success prints one machine-readable line:
//! ```text
//! matter-peek: ok fabric_index=<u8> device_node_id=0x<hex>
//! ```
//!
//! The full Phase 0 vertical: commission (ephemeral) → read `sw_diag` +
//! `gen_diag` → retrieve `CrashLogs` over BDX → `RemoveFabric` (self-destruct).

use core::pin::pin;

use std::net::SocketAddr;
use std::process::ExitCode;

use embassy_futures::select::{select, Either};

use log::{error, info};

use rs_matter::crypto::default_crypto;
use rs_matter::dm::devices::test::{DAC_PRIVKEY, TEST_DEV_ATT, TEST_DEV_COMM, TEST_DEV_DET};
use rs_matter::error::{Error, ErrorCode};
use rs_matter::transport::network::NoNetwork;
use rs_matter::utils::init::InitMaybeUninit;
use rs_matter::Matter;

use static_cell::StaticCell;

use matter_peek_core::controller::{bind_controller_socket, FabricIdentity};
use matter_peek_core::{Controller, DiagSet, FabricPolicy, LogIntent};

const DEFAULT_PASSCODE: u32 = 20202021;
const DEFAULT_PEER_ADDR: &str = "[::1]:5540";
const DEVICE_NODE_ID: u64 = 112234;

static CTRL_MATTER: StaticCell<Matter> = StaticCell::new();

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args = match parse_args() {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("matter-peek: {msg}");
            eprintln!("usage: matter-peek [PASSCODE] [PEER_ADDR]");
            return ExitCode::FAILURE;
        }
    };
    info!(
        "matter-peek: passcode={} peer={}",
        args.passcode, args.peer_addr
    );

    match futures_lite::future::block_on(run(args)) {
        Ok(outcome) => {
            println!(
                "matter-peek: ok fabric_index={} device_node_id=0x{:016x}",
                outcome.handle.fabric_index, outcome.handle.node_id
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            error!("matter-peek: FAILED — {e:?}");
            ExitCode::FAILURE
        }
    }
}

struct Args {
    passcode: u32,
    peer_addr: SocketAddr,
}

fn parse_args() -> Result<Args, String> {
    let mut iter = std::env::args().skip(1);
    let passcode = match iter.next() {
        Some(s) => s
            .parse::<u32>()
            .map_err(|e| format!("passcode must be u32: {e}"))?,
        None => DEFAULT_PASSCODE,
    };
    let peer_addr_str = iter.next().unwrap_or_else(|| DEFAULT_PEER_ADDR.to_string());
    let peer_addr = peer_addr_str
        .parse::<SocketAddr>()
        .map_err(|e| format!("peer addr must be SocketAddr (e.g. [::1]:5540): {e}"))?;
    Ok(Args {
        passcode,
        peer_addr,
    })
}

async fn run(args: Args) -> Result<matter_peek_core::CommissionOutcome, Error> {
    let socket = bind_controller_socket()?;

    let crypto = default_crypto(rand::thread_rng(), DAC_PRIVKEY);
    let matter = CTRL_MATTER.uninit().init_with(Matter::init(
        &TEST_DEV_DET,
        TEST_DEV_COMM,
        &TEST_DEV_ATT,
        // local port = 0 → kernel-picked, matches the ephemeral `socket`.
        0,
    ));

    let controller = Controller::new(matter, &crypto, FabricIdentity::default());

    // The transport pump must stay alive throughout the flow.
    let transport_fut = matter.run(&crypto, &socket, &socket, NoNetwork);
    let flow_fut = flow(&controller, &args);

    let mut transport_fut = pin!(transport_fut);
    let mut flow_fut = pin!(flow_fut);

    match select(&mut transport_fut, &mut flow_fut).await {
        Either::First(r) => {
            error!("transport exited prematurely: {r:?}");
            Err(ErrorCode::NoExchange.into())
        }
        Either::Second(result) => result,
    }
}

/// The spike flow: commission (ephemeral), then read a diagnostic health
/// snapshot over the freshly-established CASE session.
async fn flow<'a, C: rs_matter::crypto::Crypto>(
    controller: &Controller<'a, C>,
    args: &Args,
) -> Result<matter_peek_core::CommissionOutcome, Error> {
    let outcome = controller
        .commission(
            args.peer_addr,
            args.passcode,
            DEVICE_NODE_ID,
            FabricPolicy::Ephemeral,
        )
        .await?;

    info!("=== Reading diagnostic health snapshot ===");
    let health = controller.read_health(outcome.handle, DiagSet::ALL).await;
    print_health(&health);

    info!("=== Retrieving CrashLogs over BDX ===");
    match controller
        .retrieve_logs(outcome.handle, LogIntent::CrashLogs)
        .await
    {
        Ok(bytes) => println!("crash_logs: {} bytes", bytes.len()),
        Err(e) => error!("crash_logs: retrieve failed: {e:?}"),
    }

    // Ephemeral policy: self-destruct our fabric on the device.
    info!("=== Decommissioning (RemoveFabric) ===");
    if let Err(e) = controller.decommission(outcome.handle).await {
        error!("decommission failed: {e:?}");
    }

    Ok(outcome)
}

fn print_health(health: &matter_peek_core::DeviceHealth) {
    if let Some(g) = &health.general {
        println!(
            "general: up_time_secs={:?} reboot_count={:?} total_operational_hours={:?}",
            g.up_time_secs, g.reboot_count, g.total_operational_hours
        );
    } else {
        println!("general: <not read>");
    }
    if let Some(s) = &health.software {
        println!(
            "software: heap_free={:?} heap_used={:?} heap_high_watermark={:?}",
            s.current_heap_free, s.current_heap_used, s.current_heap_high_watermark
        );
    } else {
        println!("software: <not read>");
    }
}
