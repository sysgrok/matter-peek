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

use std::net::{SocketAddr, UdpSocket};
use std::process::ExitCode;

use embassy_futures::select::{select, Either};

use log::{error, info};

use rs_matter::crypto::default_crypto;
use rs_matter::dm::devices::test::{DAC_PRIVKEY, TEST_DEV_ATT, TEST_DEV_COMM, TEST_DEV_DET};
use rs_matter::error::{Error, ErrorCode};
use rs_matter::transport::network::mdns::CommissionableFilter;
use rs_matter::transport::network::NoNetwork;
use rs_matter::utils::init::InitMaybeUninit;
use rs_matter::Matter;

use static_cell::StaticCell;

use matter_peek_core::controller::FabricIdentity;
use matter_peek_core::{Controller, DiagSet, FabricPolicy, LogIntent};

const DEFAULT_PASSCODE: u32 = 20202021;
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
    match &args.peer_addr {
        Some(a) => info!("matter-peek: passcode={} peer={a}", args.passcode),
        None => info!(
            "matter-peek: passcode={} peer=<discover via mDNS>",
            args.passcode
        ),
    }

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
    /// `None` → discover the device via mDNS; `Some` → use this explicit address.
    peer_addr: Option<SocketAddr>,
    /// `--subscribe`: after the one-shot read, establish a live health
    /// subscription and stream updates for [`SUBSCRIBE_SECS`] instead of
    /// decommissioning immediately.
    subscribe: bool,
}

/// How long the `--subscribe` mode streams live health updates before tearing
/// the subscription down and decommissioning.
const SUBSCRIBE_SECS: u64 = 30;

fn parse_args() -> Result<Args, String> {
    // Positional args (passcode, peer) with a `--subscribe` flag that may appear
    // anywhere. Filter the flag out first, then parse positionals.
    let mut subscribe = false;
    let positionals: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| {
            if a == "--subscribe" {
                subscribe = true;
                false
            } else {
                true
            }
        })
        .collect();
    let mut iter = positionals.into_iter();

    let passcode = match iter.next() {
        Some(s) => s
            .parse::<u32>()
            .map_err(|e| format!("passcode must be u32: {e}"))?,
        None => DEFAULT_PASSCODE,
    };
    // Second arg: an explicit `[::1]:5540` address, or the literal `discover`
    // (also the default when omitted) to find the device via mDNS.
    let peer_addr = match iter.next().as_deref() {
        None | Some("discover") => None,
        Some(s) => Some(
            s.parse::<SocketAddr>()
                .map_err(|e| format!("peer addr must be SocketAddr or 'discover': {e}"))?,
        ),
    };
    Ok(Args {
        passcode,
        peer_addr,
        subscribe,
    })
}

async fn run(args: Args) -> Result<matter_peek_core::CommissionOutcome, Error> {
    let crypto = default_crypto(rand::thread_rng(), DAC_PRIVKEY);
    let matter = CTRL_MATTER.uninit().init_with(Matter::init(
        &TEST_DEV_DET,
        TEST_DEV_COMM,
        &TEST_DEV_ATT,
        // local port = 0 → kernel-picked, matches the ephemeral `socket`.
        0,
    ));

    let controller = Controller::new(matter, &crypto);

    // Bootstrap the controller CA + ephemeral fabric (the CLI spike uses the
    // ephemeral policy: throwaway fabric, removed after the run).
    let (ca, fab_idx) = matter_peek_core::ca::ControllerCa::bootstrap(
        &crypto,
        matter,
        FabricIdentity::default(),
        matter_peek_core::ca::CaTier::RcacDirect,
    )?;

    // Resolve the target address *before* binding the transport socket: discovery
    // uses its own mDNS path (separate socket / DBus) and doesn't need the Matter
    // transport pump. Knowing the peer first lets us bind a socket in the peer's
    // address family (see `bind_controller_socket_for`).
    let peer_addr = match args.peer_addr {
        Some(a) => a,
        None => {
            info!("=== Discovering a commissionable device via mDNS ===");
            let found = controller
                .discover(&CommissionableFilter::default(), None)
                .await?;
            match found.address {
                rs_matter::transport::network::Address::Udp(a) => a,
                other => {
                    error!("discovered non-UDP address: {other:?}");
                    return Err(ErrorCode::NoNetworkInterface.into());
                }
            }
        }
    };
    info!("commissioning device at {peer_addr}");

    // Bind the dual-stack transport socket, then run the pump for the rest of
    // the flow. (rs-matter canonicalizes peer addresses, so one `[::]:0` socket
    // serves IPv4 and IPv6 peers alike.)
    let socket = bind_controller_socket()?;
    let transport_fut = matter.run(&crypto, &socket, &socket, NoNetwork);
    let flow_fut = flow(&controller, &ca, fab_idx, &args, peer_addr);

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
    ca: &matter_peek_core::ca::ControllerCa,
    fab_idx: core::num::NonZeroU8,
    args: &Args,
    peer_addr: SocketAddr,
) -> Result<matter_peek_core::CommissionOutcome, Error> {
    let outcome = controller
        .commission(
            ca,
            fab_idx,
            peer_addr,
            args.passcode,
            DEVICE_NODE_ID,
            FabricPolicy::Ephemeral,
        )
        .await?;

    info!("=== Reading diagnostic health snapshot ===");
    let health = controller.read_health(outcome.handle, DiagSet::ALL).await;
    print_health(&health);

    if args.subscribe {
        info!("=== Subscribing to live health for {SUBSCRIBE_SECS}s ===");
        // `Rc<Cell>` so the closure owns a clone (its borrow isn't tied to a
        // local that the pinned subscription future would outlive on drop).
        let update_count = std::rc::Rc::new(core::cell::Cell::new(0usize));
        let counter = update_count.clone();
        let sub = controller.subscribe_health(outcome.handle, DiagSet::ALL, move |health| {
            counter.set(counter.get() + 1);
            println!("--- health update #{} ---", counter.get());
            print_health(&health);
        });
        let timer = embassy_time::Timer::after(embassy_time::Duration::from_secs(SUBSCRIBE_SECS));
        match select(pin!(sub), pin!(timer)).await {
            Either::First(r) => {
                if let Err(e) = r {
                    error!("subscription ended early: {e:?}");
                }
            }
            Either::Second(()) => info!(
                "subscription window elapsed ({} update(s))",
                update_count.get()
            ),
        }

        // In subscribe mode, go straight to teardown — the subscription owns the
        // session for its lifetime; we don't reuse it for further one-shot ops.
        info!("=== Decommissioning (RemoveFabric) ===");
        if let Err(e) = controller.decommission(outcome.handle).await {
            error!("decommission failed: {e:?}");
        }
        return Ok(outcome);
    }

    info!("=== Retrieving CrashLogs over BDX ===");
    match controller
        .retrieve_logs(outcome.handle, LogIntent::CrashLogs)
        .await
    {
        Ok(bytes) => println!("crash_logs: {} bytes", bytes.len()),
        Err(e) => error!("crash_logs: retrieve failed: {e:?}"),
    }

    // Exercise the Identify primitive (blink the device for 3s). Soft-fail: not
    // every device hosts Identify on the same endpoint.
    info!("=== Identify (blink) ===");
    match controller.identify(outcome.handle, 1, 3).await {
        Ok(()) => println!("identify: ok (ep1, 3s)"),
        Err(e) => error!("identify: failed: {e:?}"),
    }

    // Ephemeral policy: self-destruct our fabric on the device.
    info!("=== Decommissioning (RemoveFabric) ===");
    if let Err(e) = controller.decommission(outcome.handle).await {
        error!("decommission failed: {e:?}");
    }

    Ok(outcome)
}

/// Bind an ephemeral dual-stack IPv6 UDP socket for the controller transport.
///
/// Socket creation is the binary's job (so `matter-peek-core` stays `no_std`):
/// a single `[::]:0` socket serves both IPv4 and IPv6 peers — rs-matter
/// canonicalizes IPv4-mapped-IPv6 peer addresses on send-storage and receive, so
/// session lookup matches regardless of the socket's family. (On Windows/BSD an
/// IPv6 socket can be v6-only by default — a future tweak may need to enable
/// dual-stack explicitly there.)
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
