//! BLE (BTP) commissioning — the Commissioner over a GATT **Central**.
//!
//! This is the counterpart to the IP commissioning path in [`crate::matter`]
//! for devices that are only reachable over Bluetooth Low Energy until they have
//! joined their operational (Thread / Wi-Fi) network. It drives a GATT Central
//! over the host's Bluetooth stack (`bluez` on Linux, `winrt` on Windows), which
//! carries rs-matter's BTP transport, and adds the network-commissioning step the
//! IP path doesn't need.
//!
//! The flow has two phases over the two sides of the **same** `Matter` stack's
//! transport, which the caller runs over both UDP and BTP (see [`commission_ble`]):
//!
//! - **Phase A — over BLE/BTP.** Scan for the device's commissionable
//!   advertisement, connect to it as a GATT Central, and run PASE → ArmFailSafe →
//!   … → AddNOC, then provision the operational network (`AddOrUpdate{Thread,
//!   WiFi}Network` + `ConnectNetwork`). At the end of phase A the device has our
//!   fabric installed and has been told to join its network; its fail-safe is
//!   still armed.
//!
//! - **Phase B — over IP.** Once the device joins, it announces itself
//!   operationally over `_matter._tcp` mDNS. We drop the BLE link, resolve the
//!   device's operational address, establish CASE, and invoke
//!   `CommissioningComplete`.
//!
//! Which network a device wants is *not* in its onboarding payload - a QR code's
//! discovery capabilities say how to *reach* it for commissioning, not what it will
//! join. So the device is asked directly: see [`read_network_type`].

extern crate alloc;

use core::cell::Cell;
use core::num::NonZeroU8;
use core::pin::pin;

use alloc::vec::Vec;

use embassy_futures::select::{select, Either};
use embassy_time::{with_timeout, Duration, Timer};

use log::{error, info, warn};

use rs_matter::cert::gen::VALID_FOREVER;
use rs_matter::cert::{MAX_CERT_TLV_AND_ASN1_LEN, MAX_CERT_TLV_LEN};
use rs_matter::crypto::Crypto;
use rs_matter::dm::clusters::decl::general_commissioning::{
    GeneralCommissioningAttrReads as _, GeneralCommissioningAttrResponses as _,
};
use rs_matter::dm::clusters::decl::network_commissioning::{
    Feature, NetworkCommissioningAttrReads as _, NetworkCommissioningAttrResponses as _,
    NetworkCommissioningClient as _, NetworkCommissioningStatusEnum,
};
use rs_matter::dm::clusters::net_comm::NetworkType;
use rs_matter::dm::endpoints::ROOT_ENDPOINT_ID;
use rs_matter::error::{Error, ErrorCode};
use rs_matter::im::client::ImClient as _;
use rs_matter::onboard::{CommissionOptions, CommissionResult, Commissioner};
use rs_matter::tlv::OctetStr;
use rs_matter::transport::exchange::Exchange;
use rs_matter::transport::network::btp::Btp;
use rs_matter::transport::network::mdns::CommissionableFilter;
use rs_matter::transport::network::{Address, BtAddr};
use rs_matter::transport::session::SessionMode;
use rs_matter::Matter;

use crate::ca::ControllerCa;
use crate::matter::remove_sessions;
use crate::model::{CommissionOutcome, DeviceHandle};

#[cfg(all(feature = "ble-bluez", target_os = "linux"))]
use bluez as stack;
#[cfg(all(feature = "ble-winrt", windows))]
use winrt as stack;

#[cfg(all(feature = "ble-bluez", target_os = "linux"))]
mod bluez;
#[cfg(all(feature = "ble-winrt", windows))]
mod winrt;

#[cfg(not(any(
    all(feature = "ble-bluez", target_os = "linux"),
    all(feature = "ble-winrt", windows)
)))]
compile_error!(
    "`ble` needs the host's Bluetooth stack: `ble-bluez` on Linux, `ble-winrt` on Windows"
);

/// Fail-safe duration armed for the whole BLE→Thread flow.
///
/// The single fail-safe armed during phase 1 must cover *everything* up to
/// `CommissioningComplete`: AddNOC, the network-commissioning step, the device's
/// Thread attach, its operational mDNS announce (which, via a Thread Border
/// Router's SRP advertising proxy, can take a while to propagate to the LAN), the
/// mDNS resolve, and CASE. If it expires the device rolls back the partial
/// commission, so this must comfortably exceed
/// [`OPERATIONAL_JOIN_TIMEOUT_SECS`]. (The spec caps ArmFailSafe at 900 s.)
const BLE_FAIL_SAFE_SECS: u16 = 240;

/// How long a device without concurrent connection gets to answer
/// `ConnectNetwork` over BLE, which it leaves to join its network.
const NON_CONCURRENT_CONNECT_WAIT_SECS: u64 = 5;

/// How long [`commission_ble`] scans for the commissionable advertisement.
const SCAN_TIMEOUT_SECS: u16 = 30;

/// How long to keep retrying the operational CASE resolve/handshake in phase B,
/// while the device is joining Thread and starting to announce itself.
///
/// A Thread device's operational `_matter._tcp` advertisement reaches the LAN
/// only after it attaches to the mesh *and* the Border Router's SRP advertising
/// proxy republishes it, which can take well over a minute end to end - so this
/// is generous. Must stay below [`BLE_FAIL_SAFE_SECS`] (the device rolls back if
/// the fail-safe expires first).
const OPERATIONAL_JOIN_TIMEOUT_SECS: u64 = 180;

/// Re-exported so callers can build the credentials [`commission_ble`] wants
/// without depending on `rs-matter`'s cluster module directly.
pub use rs_matter::dm::clusters::net_comm::WirelessCreds;

/// A device advertising as commissionable over Bluetooth LE.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BleDevice {
    pub addr: BtAddr,
    pub discriminator: u16,
    pub vendor_id: u16,
    pub product_id: u16,
}

/// The devices advertising as commissionable over Bluetooth LE that match
/// `filter`, scanning for `secs` seconds.
pub async fn scan_ble(filter: &CommissionableFilter, secs: u16) -> Result<Vec<BleDevice>, Error> {
    let mut found = Vec::new();

    // Every match is collected, so the scan only ends with its time limit.
    let _ = stack::scan(filter, Some(secs), |addr, adv| {
        found.push(BleDevice {
            addr,
            discriminator: adv.discriminator(),
            vendor_id: adv.vid(),
            product_id: adv.pid(),
        });

        None::<()>
    })
    .await;

    Ok(found)
}

/// Why commissioning over BLE cannot run now, if it cannot: the host's Bluetooth
/// stack does not run, it has no adapter, or the adapter is off.
pub(crate) async fn unavailable() -> Option<alloc::string::String> {
    stack::unavailable().await
}

/// Commission a device that is currently only reachable over BLE, provisioning it
/// with the given operational-network credentials so it joins its network and
/// finishes commissioning over IP.
///
/// The caller runs `matter`'s transport over both UDP and `btp` (e.g. a
/// `ChainedNetwork` routing BTP addresses to `btp`), plus the mDNS backend:
/// phase A goes over `btp`, phase B over UDP. See the module docs for the flow.
/// Returns the installed device handle on success.
///
/// # Arguments
/// - `matter` / `crypto`: the borrowed rs-matter stack and crypto backend (as held
///   by [`crate::matter::Controller`]).
/// - `ca` / `controller_fab_idx`: the controller CA and its local fabric index
///   (from [`ControllerCa::bootstrap`](crate::ca::ControllerCa::bootstrap)).
/// - `btp`: the BTP side of `matter`'s transport.
/// - `filter`: matches the device's BLE advertisement (typically a discriminator
///   from the scanned QR / manual pairing code).
/// - `addr`: the device's Bluetooth address, if already known (e.g. from
///   [`scan_ble`]); else the first device matching `filter` is used.
/// - `passcode`: the device's PASE passcode (from the same QR / pairing code).
/// - `device_node_id`: the operational Node ID to assign the device.
/// - `creds`: the credentials for the network the device should join. Which kind is
///   needed is *not* something the onboarding payload can tell us - it is read off
///   the device itself (see [`read_network_type`]) - so this must match the
///   device's actual network type. Pass `None` for an Ethernet device, which is
///   already on its network and needs no network commissioning at all.
#[allow(clippy::too_many_arguments)]
pub async fn commission_ble<C: Crypto>(
    matter: &Matter<'_>,
    crypto: &C,
    ca: &ControllerCa,
    controller_fab_idx: NonZeroU8,
    btp: &Btp,
    filter: &CommissionableFilter,
    addr: Option<BtAddr>,
    passcode: u32,
    device_node_id: u64,
    creds: Option<&WirelessCreds<'_>>,
) -> Result<CommissionOutcome, Error> {
    // ---- Discovery: find the device's commissionable BLE advertisement. --------
    // Also when its address is known: the GATT connect needs the Bluetooth stack
    // to have seen it.
    info!("BLE commission: scanning for a commissionable device (filter: {filter:?})");

    let addr = stack::scan(filter, Some(SCAN_TIMEOUT_SECS), |found, adv| {
        info!("BLE commission: found candidate {found} (adv: {adv:?})");
        addr.is_none_or(|addr| addr == found).then_some(found)
    })
    .await?;

    info!("BLE commission: using device {addr}");

    // ---- Phase A — over BLE/BTP. -----------------------------------------------
    // BlueZ reports an under-stated GATT MTU; the relaxed negotiation lets us use
    // the larger MTU the peer actually supports (mirrors the peripheral).
    btp.set_relaxed_mtu_nego(true);

    // Establish the initiator role and a clean session *before* the futures below
    // run. The commissioner (`flow`) starts sending Matter traffic (the PASE
    // PBKDFParamRequest) as soon as the link is up, so the session must already
    // be reset and in the initiator role - otherwise a later reset would race
    // with, and wipe, that first queued SDU.
    btp.reset();
    btp.set_initiator(true);

    let phase1 = {
        // The GATT Central; the caller's transport pumps `btp`.
        let mut central = pin!(stack::run_central(addr, btp));

        // Set once `ConnectNetwork` is sent: the device may leave BLE from then on.
        let joining = Cell::new(false);

        // The commissioning steps that run *over* this link.
        let mut flow = pin!(phase_a_flow(
            matter,
            crypto,
            ca,
            controller_fab_idx,
            addr,
            passcode,
            device_node_id,
            creds,
            &joining,
        ));

        let result = match select(&mut central, &mut flow).await {
            // Leaving BLE to join its network, as a device without concurrent
            // connection does: the flow ends on its own, answered or not.
            Either::First(_) if joining.get() => {
                info!("BLE commission: the device left BLE to join its network");
                flow.await
            }
            Either::First(central_result) => Err(central_result
                .err()
                .unwrap_or_else(|| ErrorCode::NoNetworkInterface.into())),
            Either::Second(flow_result) => flow_result,
        };

        drop_ble_link(matter, btp, addr);
        result?
    };

    info!(
        "BLE commission: phase A ok (device fabric {}, node 0x{:016X}); device is joining its network",
        phase1.fabric_index, phase1.device_node_id
    );

    // ---- Phase B — over IP. ----------------------------------------------------
    // Complete CASE once the device has joined its network and announced itself
    // operationally.
    phase_b_complete(matter, crypto, controller_fab_idx, ca, &phase1).await?;

    info!("BLE commission: phase B ok (CASE established, CommissioningComplete acknowledged)");

    // A new device: no window was opened for it.
    Ok(CommissionOutcome {
        handle: DeviceHandle {
            fabric_index: controller_fab_idx,
            node_id: phase1.device_node_id,
        },
        window_opener: None,
    })
}

/// Done with the BLE link to `addr`: drop the PASE session that ran over it and
/// reset the BTP session (the GATT connection went with the central's future).
fn drop_ble_link(matter: &Matter<'_>, btp: &Btp, addr: BtAddr) {
    let peer = Address::Btp(addr);
    remove_sessions(matter, |sess| {
        matches!(sess.get_session_mode(), SessionMode::Pase { .. }) && sess.get_peer_addr() == peer
    });

    btp.reset();
}

/// Phase A steps that run over the live BLE/BTP transport: PASE → AddNOC, then
/// network provisioning, which sets `joining` once the device is told to join.
#[allow(clippy::too_many_arguments)]
async fn phase_a_flow<C: Crypto>(
    matter: &Matter<'_>,
    crypto: &C,
    ca: &ControllerCa,
    controller_fab_idx: NonZeroU8,
    addr: rs_matter::transport::network::BtAddr,
    passcode: u32,
    device_node_id: u64,
    creds: Option<&WirelessCreds<'_>>,
    joining: &Cell<bool>,
) -> Result<CommissionResult, Error> {
    let peer = Address::Btp(addr);

    // NocGenerator + commissioner scratch, exactly as the IP path.
    let mut noc_buf = [0u8; MAX_CERT_TLV_AND_ASN1_LEN];
    let mut noc_generator = ca.noc_generator(&mut noc_buf)?;

    let mut commissioner_buf = [0u8; MAX_CERT_TLV_LEN];
    let mut commissioner = Commissioner::new(
        matter,
        crypto,
        controller_fab_idx,
        &mut noc_generator,
        &mut commissioner_buf,
    );

    let opts = CommissionOptions {
        allow_test_attestation: true,
        // One fail-safe covers phase A *and* phase B (the network join happens
        // inside this window — see `BLE_FAIL_SAFE_SECS`).
        fail_safe_secs: BLE_FAIL_SAFE_SECS,
    };

    info!("BLE commission: phase 1 (PASE → AddNOC) over BTP");
    let phase1 = commissioner
        .commission(peer, passcode, &opts, device_node_id, VALID_FOREVER)
        .await?;

    // Network commissioning: hand the device its credentials and tell it to connect.
    // Runs over PASE on the still-live BTP session.
    if let Err(e) = provision_network(matter, crypto, peer, passcode, creds, joining).await {
        // Roll the device back at once, so it can be commissioned again right
        // away. Best-effort, as the device may be gone.
        let _ = commissioner.disarm_fail_safe(peer).await;
        return Err(e);
    }

    Ok(phase1)
}

/// Provision the device's operational network, if it needs one.
///
/// Neither the QR code nor the manual pairing code says whether a device is Thread,
/// Wi-Fi or Ethernet - the onboarding payload's discovery capabilities describe how
/// to *reach* the device for commissioning, not the network it will live on. The
/// device itself is the authority, so we ask it: the `NetworkCommissioning`
/// cluster's `FeatureMap` names its interface type, and we provision accordingly.
async fn provision_network<C: Crypto>(
    matter: &Matter<'_>,
    crypto: &C,
    peer: Address,
    passcode: u32,
    creds: Option<&WirelessCreds<'_>>,
    joining: &Cell<bool>,
) -> Result<(), Error> {
    let net_type = read_network_type(matter, crypto, peer, passcode).await?;
    let concurrent = read_concurrent_connection(matter, crypto, peer, passcode).await;

    info!(
        "BLE commission: device network type is {net_type:?} (concurrent connection: {concurrent})"
    );

    if matches!(net_type, NetworkType::Ethernet) {
        // An Ethernet device is already on its operational network; there is nothing
        // to provision, and the cluster exposes no network commands to do it with.
        info!("BLE commission: Ethernet device - skipping network commissioning");
        return Ok(());
    }

    let creds = creds.ok_or_else(|| {
        error!("BLE commission: device needs {net_type:?} credentials, but none were given");
        Error::from(ErrorCode::InvalidAction)
    })?;

    // Refuse to hand a Thread device Wi-Fi credentials, or vice versa.
    creds.check_match(net_type).inspect_err(|_| {
        error!(
            "BLE commission: {creds} credentials do not match the device's {net_type:?} network"
        );
    })?;

    // For Thread this is the Extended PAN ID, for Wi-Fi the SSID - `WirelessCreds`
    // knows which, so `ConnectNetwork` below just uses it.
    let network_id = creds.id()?;

    match creds {
        WirelessCreds::Thread { dataset_tlv } => {
            info!(
                "BLE commission: AddOrUpdateThreadNetwork ({} dataset bytes)",
                dataset_tlv.len()
            );

            let exchange = Exchange::initiate_pase(matter, crypto, peer, passcode).await?;
            let handle = exchange
                .network_commissioning()
                .add_or_update_thread_network(ROOT_ENDPOINT_ID, |req| {
                    req.operational_dataset(OctetStr::new(dataset_tlv))?
                        .breadcrumb(None)?
                        .end()
                })
                .await?;
            check_net_status(
                handle.response()?.networking_status()?,
                "AddOrUpdateThreadNetwork",
            )?;
            handle.complete().await?;
        }
        WirelessCreds::Wifi { ssid, pass } => {
            info!("BLE commission: AddOrUpdateWiFiNetwork ({creds})");

            let exchange = Exchange::initiate_pase(matter, crypto, peer, passcode).await?;
            let handle = exchange
                .network_commissioning()
                .add_or_update_wi_fi_network(ROOT_ENDPOINT_ID, |req| {
                    // No per-device-credentials fields here: `NetworkIdentity`,
                    // `ClientIdentifier` and `PossessionNonce` left this command
                    // in the Matter 1.6.0 data model, moving to the provisional
                    // `NetworkIdentityManagement` cluster. We only ever passed
                    // `None` for them, so dropping them changes nothing on the
                    // wire. (The 1.6.1 IDL has them back; rs-matter keeps that
                    // restoration commented out until it adopts 1.6.1.)
                    req.ssid(OctetStr::new(ssid))?
                        .credentials(OctetStr::new(pass))?
                        .breadcrumb(None)?
                        .end()
                })
                .await?;
            check_net_status(
                handle.response()?.networking_status()?,
                "AddOrUpdateWiFiNetwork",
            )?;
            handle.complete().await?;
        }
    }

    info!("BLE commission: ConnectNetwork (network id {network_id:02x?})");

    let exchange = Exchange::initiate_pase(matter, crypto, peer, passcode).await?;
    joining.set(true);

    let connect = exchange
        .network_commissioning()
        .connect_network(ROOT_ENDPOINT_ID, |req| {
            req.network_id(OctetStr::new(network_id))?
                .breadcrumb(None)?
                .end()
        });

    // A device with concurrent connection answers over BLE once it has tried to
    // join; one without leaves BLE to join, perhaps without answering.
    let connect = if concurrent {
        connect.await
    } else {
        let wait = Duration::from_secs(NON_CONCURRENT_CONNECT_WAIT_SECS);
        with_timeout(wait, connect)
            .await
            .unwrap_or_else(|_| Err(ErrorCode::RxTimeout.into()))
    };

    // With no answer, phase B finds out whether the device joined: some claim
    // concurrent connection and go silent all the same.
    let handle = match connect {
        Ok(handle) => handle,
        Err(e) if matches!(e.code(), ErrorCode::RxTimeout | ErrorCode::TxTimeout) => {
            if concurrent {
                warn!("BLE commission: no ConnectNetwork response, although the device claims concurrent connection ({e:?}); trying it on its network");
            } else {
                info!("BLE commission: no ConnectNetwork response, as the device has no concurrent connection; trying it on its network");
            }
            return Ok(());
        }
        Err(e) => return Err(e),
    };
    check_net_status(handle.response()?.networking_status()?, "ConnectNetwork")?;
    handle.complete().await?;

    Ok(())
}

/// Ask the device what kind of operational network it has, by reading the
/// `NetworkCommissioning` cluster's `FeatureMap` over PASE.
///
/// Exactly one of the three interface bits is expected; a device that reports none
/// of them has no network interface we know how to commission.
async fn read_network_type<C: Crypto>(
    matter: &Matter<'_>,
    crypto: &C,
    peer: Address,
    passcode: u32,
) -> Result<NetworkType, Error> {
    let exchange = Exchange::initiate_pase(matter, crypto, peer, passcode).await?;

    let mut chunk = exchange
        .read_with(|msg| {
            msg.attr_requests()?
                .network_commissioning_read()
                .feature_map(ROOT_ENDPOINT_ID)?
                .end()?
                .fabric_filtered(false)?
                .end()
        })
        .await?;

    // Copy the bits out before the borrowed response drops.
    let mut features = None;
    {
        let resp = chunk.response()?;
        for (_endpoint, result) in resp.network_commissioning_read_resp().feature_map() {
            features = Some(Feature::from_bits_truncate(result?));
        }
    }
    while let Some(next) = chunk.complete().await? {
        chunk = next;
    }

    let features = features.ok_or_else(|| {
        error!("BLE commission: device did not report a NetworkCommissioning FeatureMap");
        Error::from(ErrorCode::InvalidData)
    })?;

    if features.contains(Feature::THREAD_NETWORK_INTERFACE) {
        Ok(NetworkType::Thread)
    } else if features.contains(Feature::WI_FI_NETWORK_INTERFACE) {
        Ok(NetworkType::Wifi)
    } else if features.contains(Feature::ETHERNET_NETWORK_INTERFACE) {
        Ok(NetworkType::Ethernet)
    } else {
        error!("BLE commission: device reports no known network interface ({features:?})");
        Err(ErrorCode::InvalidData.into())
    }
}

/// Whether the device stays on BLE while it joins its network
/// (`SupportsConcurrentConnection`), read over PASE. Assumed when it cannot be
/// read, as CHIP does.
async fn read_concurrent_connection<C: Crypto>(
    matter: &Matter<'_>,
    crypto: &C,
    peer: Address,
    passcode: u32,
) -> bool {
    let read = async {
        let exchange = Exchange::initiate_pase(matter, crypto, peer, passcode).await?;

        let mut chunk = exchange
            .read_with(|msg| {
                msg.attr_requests()?
                    .general_commissioning_read()
                    .supports_concurrent_connection(ROOT_ENDPOINT_ID)?
                    .end()?
                    .fabric_filtered(false)?
                    .end()
            })
            .await?;

        let mut concurrent = None;
        {
            let resp = chunk.response()?;
            for (_endpoint, result) in resp
                .general_commissioning_read_resp()
                .supports_concurrent_connection()
            {
                concurrent = Some(result?);
            }
        }
        while let Some(next) = chunk.complete().await? {
            chunk = next;
        }

        concurrent.ok_or_else(|| Error::from(ErrorCode::InvalidData))
    };

    read.await.unwrap_or_else(|e: Error| {
        warn!("BLE commission: cannot read SupportsConcurrentConnection ({e:?}); assuming it");
        true
    })
}

/// Phase B: retry the operational CASE handshake (which resolves the device's
/// operational address over mDNS) until it succeeds or we give up.
///
/// The device may not have finished joining its network / started announcing itself
/// when we first try, so `complete_via_case` is retried on failure (a failed
/// resolve returns promptly) within a bounded window.
async fn phase_b_complete<C: Crypto>(
    matter: &Matter<'_>,
    crypto: &C,
    controller_fab_idx: NonZeroU8,
    ca: &ControllerCa,
    phase1: &CommissionResult,
) -> Result<(), Error> {
    let mut noc_buf = [0u8; MAX_CERT_TLV_AND_ASN1_LEN];
    let mut noc_generator = ca.noc_generator(&mut noc_buf)?;

    let mut commissioner_buf = [0u8; MAX_CERT_TLV_LEN];
    // One CASE attempt per call: the wall-clock loop below is what bounds the
    // wait for the device to finish joining its network, and it needs the
    // per-call failure to come back promptly to keep its own cadence. (The
    // built-in retry count exists for the IP path, where there is no such outer
    // loop — see `controller::commission`.)
    let mut commissioner = Commissioner::new(
        matter,
        crypto,
        controller_fab_idx,
        &mut noc_generator,
        &mut commissioner_buf,
    )
    .with_case_attempts(1);

    let deadline = OPERATIONAL_JOIN_TIMEOUT_SECS;
    let mut waited = 0u64;

    loop {
        match commissioner.complete_via_case(phase1).await {
            Ok(()) => return Ok(()),
            Err(e) if waited >= deadline => return Err(e),
            Err(e) => {
                info!(
                    "BLE commission: device not operational yet ({e:?}); retrying (waited {waited}s / {deadline}s)"
                );

                Timer::after(Duration::from_secs(3)).await;
                waited += 3;
            }
        }
    }
}

/// Map a non-`Success` network-commissioning status to an error.
fn check_net_status(status: NetworkCommissioningStatusEnum, step: &str) -> Result<(), Error> {
    if status == NetworkCommissioningStatusEnum::Success {
        Ok(())
    } else {
        warn!("BLE commission: {step} returned {status:?}");
        Err(ErrorCode::Failure.into())
    }
}
