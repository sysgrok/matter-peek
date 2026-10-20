//! matter-peek CLI — the Phase 0 vertical spike.
//!
//! Drives [`matter_peek_core::Controller::commission`] against an
//! externally-running Matter accessory. Defaults match rs-matter's test fixture
//! (passcode `20202021`, peer `[::1]:5540`), so it works out-of-the-box against
//! `chip-all-clusters-app` or an rs-matter example device.
//!
//! ```text
//! matter-peek [SETUP_CODE] [PEER_ADDR]
//! ```
//!
//! `SETUP_CODE` is a passcode, a manual pairing code or an `MT:` QR code payload.
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

use embassy_futures::select::{select3, Either3};

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
use matter_peek_core::ota::OtaProvider;
use matter_peek_core::setup_code::SetupCode;
use matter_peek_core::subscribe::Reports;
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
            eprintln!("usage: matter-peek [SETUP_CODE] [PEER_ADDR] [--subscribe]");
            eprintln!(
                "       matter-peek --ble [--discriminator N] [SETUP_CODE]   (with $THREAD_DATASET or $THREAD_DATASET_FILE)"
            );
            eprintln!(
                "       ... --qr-image <PNG|JPEG>                            (supplies the setup code)"
            );
            eprintln!(
                "       SETUP_CODE: a passcode, a manual pairing code or an MT: QR code payload"
            );
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
    /// `--ble`: commission the device over Bluetooth (BTP) instead of IP,
    /// provisioning it with the Thread dataset from `$THREAD_DATASET`. The device
    /// is matched by `filter` (or the first commissionable one seen).
    #[cfg_attr(not(feature = "ble"), allow(dead_code))]
    ble: bool,
    /// What finds the device, over mDNS or BLE: `--discriminator <N>` (12-bit),
    /// else the discriminator of the setup code, if it carries one.
    filter: CommissionableFilter,
    /// `--qr-image <path>`: read the device's onboarding payload out of a QR-code
    /// image (PNG/JPEG), instead of passing the setup code by hand.
    #[cfg_attr(not(feature = "qr-image"), allow(dead_code))]
    qr_image: Option<String>,
}

/// How long the `--subscribe` mode streams live health updates before tearing
/// the subscription down and decommissioning.
const SUBSCRIBE_SECS: u64 = 30;

fn parse_args() -> Result<Args, String> {
    // Positional args (passcode, peer) plus flags that may appear anywhere:
    // `--subscribe`, `--ble`, and `--discriminator <N>`. Consume the flags first,
    // then parse positionals from what's left.
    let mut subscribe = false;
    let mut ble = false;
    let mut discriminator = None;
    let mut qr_image = None;

    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut positionals: Vec<String> = Vec::new();
    let mut i = 0;
    while i < raw.len() {
        match raw[i].as_str() {
            "--subscribe" => subscribe = true,
            "--ble" => ble = true,
            "--discriminator" => {
                i += 1;
                let val = raw
                    .get(i)
                    .ok_or_else(|| "--discriminator needs a value".to_string())?;
                discriminator = Some(
                    parse_int_u16(val)
                        .map_err(|e| format!("--discriminator must be a 12-bit integer: {e}"))?,
                );
            }
            "--qr-image" => {
                i += 1;
                qr_image = Some(
                    raw.get(i)
                        .ok_or_else(|| "--qr-image needs a path".to_string())?
                        .clone(),
                );
            }
            other => positionals.push(other.to_string()),
        }
        i += 1;
    }

    let mut iter = positionals.into_iter();

    let code = match iter.next() {
        Some(s) => SetupCode::parse(&s).map_err(|e| {
            format!(
                "not a passcode, manual pairing code or MT: QR code payload: {:?}",
                e.code()
            )
        })?,
        None => SetupCode {
            passcode: DEFAULT_PASSCODE,
            filter: CommissionableFilter::default(),
        },
    };
    let filter = match discriminator {
        Some(discriminator) => CommissionableFilter {
            discriminator: Some(discriminator),
            ..Default::default()
        },
        None => code.filter,
    };
    // Second arg: an explicit `[::1]:5540` address, or the literal `discover`
    // (also the default when omitted) to find the device via mDNS. Ignored with
    // `--ble` (BLE finds the device by discriminator over the air).
    let peer_addr = match iter.next().as_deref() {
        None | Some("discover") => None,
        Some(s) => Some(
            s.parse::<SocketAddr>()
                .map_err(|e| format!("peer addr must be SocketAddr or 'discover': {e}"))?,
        ),
    };
    Ok(Args {
        passcode: code.passcode,
        peer_addr,
        subscribe,
        ble,
        filter,
        qr_image,
    })
}

/// Read a device's onboarding payload out of a QR-code image, returning its
/// setup code.
///
/// The `image` crate turns the file into pixels; `rs-matter` does the actual QR
/// recognition (from 8-bit greyscale) and the payload parsing.
#[cfg(feature = "qr-image")]
fn read_qr_image(path: &str) -> Result<SetupCode, Error> {
    use rs_matter::pairing::qr::{scan, QrPayload};

    let luma = image::open(path)
        .map_err(|e| {
            error!("could not read QR image {path}: {e}");
            Error::from(ErrorCode::InvalidData)
        })?
        .to_luma8();

    let text = scan::scan_luma(luma.width() as _, luma.height() as _, luma.as_raw())?;
    info!("qr-image: recognized {text}");

    // The payload borrows from `buf`, so both must outlive the reads below.
    let mut buf = [0; 128];
    let payload = QrPayload::parse(&text, &mut buf)?;

    info!(
        "qr-image: discriminator={} passcode={} vid=0x{:04x} pid=0x{:04x} caps={:?}",
        payload.discriminator(),
        payload.passcode(),
        payload.vid(),
        payload.pid(),
        payload.discovery_capabilities(),
    );

    Ok(SetupCode {
        passcode: payload.passcode(),
        filter: payload.commissionable_filter(),
    })
}

/// Parse a `u16` in decimal or `0x`-prefixed hex.
fn parse_int_u16(s: &str) -> Result<u16, core::num::ParseIntError> {
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => u16::from_str_radix(hex, 16),
        None => s.parse::<u16>(),
    }
}

// `args` is only mutated to fold in a `--qr-image` payload, which is feature-gated.
#[cfg_attr(not(feature = "qr-image"), allow(unused_mut))]
async fn run(mut args: Args) -> Result<matter_peek_core::CommissionOutcome, Error> {
    // A QR-code image, if given, supplies the setup code one would otherwise pass
    // by hand. Do this first, so both the BLE and the IP path below just see it as
    // a normal argument.
    #[cfg(feature = "qr-image")]
    if let Some(path) = args.qr_image.take() {
        let code = read_qr_image(&path)?;

        args.filter = code.filter;
        args.passcode = code.passcode;
    }

    let crypto = default_crypto(rand::rng(), DAC_PRIVKEY);
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

    // BLE commissioning path — commissions over Bluetooth and provisions Thread,
    // managing its own two-phase (BLE then IP) transport. Returns as soon as the
    // device is fully commissioned (health reads / decommission are the IP path's
    // job and can be layered on later).
    #[cfg(feature = "ble")]
    if args.ble {
        return commission_over_ble(matter, &crypto, &ca, fab_idx, &args).await;
    }

    // Resolve the target address *before* binding the transport socket: discovery
    // uses its own mDNS path (separate socket / DBus) and doesn't need the Matter
    // transport pump. Knowing the peer first lets us bind a socket in the peer's
    // address family (see `bind_controller_socket_for`).
    let peer_addr = match args.peer_addr {
        Some(a) => a,
        None => {
            info!("=== Discovering a commissionable device via mDNS ===");
            let found = controller.discover(&args.filter, None).await?;
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
    //
    // The mDNS backend runs alongside it because every step after PASE addresses
    // the device by `(fabric, node-id)` rather than by IP, and rs-matter answers
    // that from an **operational resolve** — phase 2 of the commission included.
    // Without a backend servicing the resolve rendezvous each of those times out.
    // (The discovery browse above has already finished by now; it runs its own
    // short-lived backend, and two backends must not poll the rendezvous at once.)
    let socket = bind_controller_socket()?;
    let transport_fut = matter.run(&crypto, &socket, &socket, NoNetwork);
    let mdns_fut = controller.run_mdns_backend();
    let flow_fut = flow(&controller, &ca, fab_idx, &args, peer_addr);

    let mut transport_fut = pin!(transport_fut);
    let mut mdns_fut = pin!(mdns_fut);
    let mut flow_fut = pin!(flow_fut);

    match select3(&mut transport_fut, &mut mdns_fut, &mut flow_fut).await {
        Either3::First(r) => {
            error!("transport exited prematurely: {r:?}");
            Err(ErrorCode::NoExchange.into())
        }
        Either3::Second(r) => {
            error!("mDNS backend exited prematurely: {r:?}");
            Err(ErrorCode::NoNetworkInterface.into())
        }
        Either3::Third(result) => result,
    }
}

/// Commission a device over BLE, provisioning it with the Thread dataset from
/// `$THREAD_DATASET` (hex) or the file at `$THREAD_DATASET_FILE`.
#[cfg(feature = "ble")]
async fn commission_over_ble<C: rs_matter::crypto::Crypto>(
    matter: &Matter<'_>,
    crypto: &C,
    ca: &matter_peek_core::ca::ControllerCa,
    fab_idx: core::num::NonZeroU8,
    args: &Args,
) -> Result<matter_peek_core::CommissionOutcome, Error> {
    use matter_peek_core::ble::{commission_ble, decode_hex, WirelessCreds};

    // Credentials for whatever network the device turns out to want. We don't know
    // which kind that is yet - the device itself is asked, after PASE - so just
    // supply whatever the environment offers and let the core match them up (or
    // report a clear mismatch).
    //
    // Thread dataset: prefer the file (keeps the secret out of the environment and
    // the process listing), else the hex env var.
    let dataset = match std::env::var("THREAD_DATASET_FILE") {
        Ok(path) => {
            let hex = std::fs::read_to_string(&path).map_err(|e| {
                error!("could not read THREAD_DATASET_FILE ({path}): {e}");
                Error::from(ErrorCode::InvalidData)
            })?;

            Some(decode_hex(hex.trim())?)
        }
        Err(_) => match std::env::var("THREAD_DATASET") {
            Ok(hex) => Some(decode_hex(hex.trim())?),
            Err(_) => None,
        },
    };

    let wifi = match (std::env::var("WIFI_SSID"), std::env::var("WIFI_PASS")) {
        (Ok(ssid), Ok(pass)) => Some((ssid, pass)),
        _ => None,
    };

    // An Ethernet device needs none of this, so no credentials at all is valid; the
    // core errors out if the device does turn out to want some.
    let creds = match (&dataset, &wifi) {
        (Some(dataset_tlv), _) => Some(WirelessCreds::Thread { dataset_tlv }),
        (None, Some((ssid, pass))) => Some(WirelessCreds::Wifi {
            ssid: ssid.as_bytes(),
            pass: pass.as_bytes(),
        }),
        (None, None) => {
            info!("no $THREAD_DATASET / $WIFI_SSID+$WIFI_PASS given - assuming an Ethernet device");
            None
        }
    };

    info!(
        "=== BLE commissioning (filter={:?}, creds={}) ===",
        args.filter,
        creds
            .as_ref()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "none (Ethernet)".into()),
    );

    // A UDP socket for phase B (the operational / CASE transport).
    let socket = bind_controller_socket()?;

    commission_ble(
        matter,
        crypto,
        ca,
        fab_idx,
        &args.filter,
        args.passcode,
        DEVICE_NODE_ID,
        creds.as_ref(),
        &socket,
        FabricPolicy::Ephemeral,
    )
    .await
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
            // The CLI keeps no store, so it holds no live fabric of its own:
            // every fabric of ours on the device really is a leak to reclaim.
            &[],
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

        // Reports reach the subscription through our node's responder.
        let reports = Reports::new();
        let ota = OtaProvider::new((), |_| {});
        let node = controller.run_node(&reports, &ota);

        let sub = controller.subscribe_health(
            &reports,
            outcome.handle,
            DiagSet::ALL,
            move |health| {
                counter.set(counter.get() + 1);
                println!("--- health update #{} ---", counter.get());
                print_health(&health);
            },
            |state| info!("subscription state: {state:?}"),
        );
        let timer = embassy_time::Timer::after(embassy_time::Duration::from_secs(SUBSCRIBE_SECS));
        match select3(pin!(sub), pin!(timer), pin!(node)).await {
            Either3::First(r) => {
                if let Err(e) = r {
                    error!("subscription ended early: {e:?}");
                }
            }
            Either3::Third(r) => error!("controller node ended: {r:?}"),
            Either3::Second(()) => info!(
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
