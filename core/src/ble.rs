//! BLE (BTP) commissioning — the Commissioner over a GATT **Central**.
//!
//! This is the counterpart to the IP commissioning path in [`crate::controller`]
//! for devices that are only reachable over Bluetooth Low Energy until they have
//! joined their operational (Thread / Wi-Fi) network. It drives rs-matter's
//! GATT-Central BTP transport (`rs_matter::transport::network::btp::gatt::bluez`)
//! and adds the network-commissioning step the IP path doesn't need.
//!
//! The flow has two phases over two different transports on the **same** `Matter`
//! stack (see [`commission_ble`]):
//!
//! - **Phase A — over BLE/BTP.** Scan for the device's commissionable
//!   advertisement, connect to it as a GATT Central, and run PASE → ArmFailSafe →
//!   … → AddNOC, then provision the operational network (`AddOrUpdate{Thread,
//!   WiFi}Network` + `ConnectNetwork`). At the end of phase A the device has our
//!   fabric installed and has been told to join its network; its fail-safe is
//!   still armed.
//!
//! - **Phase B — over IP.** Once the device joins, it announces itself
//!   operationally over `_matter._tcp` mDNS. We tear down the BLE transport,
//!   bring up the UDP transport + mDNS backend, resolve the device's operational
//!   address, establish CASE, and invoke `CommissioningComplete`.
//!
//! Which network a device wants is *not* in its onboarding payload - a QR code's
//! discovery capabilities say how to *reach* it for commissioning, not what it will
//! join. So the device is asked directly: see [`read_network_type`].

extern crate alloc;

use core::num::NonZeroU8;
use core::pin::pin;

use alloc::vec::Vec;

use embassy_futures::select::{select, Either};
use embassy_time::{Duration, Timer};

use log::{error, info, warn};

use rs_matter::cert::gen::VALID_FOREVER;
use rs_matter::cert::{MAX_CERT_TLV_AND_ASN1_LEN, MAX_CERT_TLV_LEN};
use rs_matter::crypto::Crypto;
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
use rs_matter::transport::network::btp::bluez;
use rs_matter::transport::network::btp::Btp;
use rs_matter::transport::network::mdns::CommissionableFilter;
use rs_matter::transport::network::{Address, NoNetwork};
use rs_matter::utils::select::Coalesce;
use rs_matter::utils::zbus::Connection;
use rs_matter::Matter;

use crate::ca::ControllerCa;
use crate::model::{CommissionOutcome, DeviceHandle};

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

/// How long [`commission_ble`] scans for the commissionable advertisement.
const SCAN_TIMEOUT_SECS: u16 = 60;

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

/// Commission a device that is currently only reachable over BLE, provisioning it
/// with the given operational-network credentials so it joins its network and
/// finishes commissioning over IP.
///
/// See the module docs for the two-phase flow. Returns the installed device
/// handle on success.
///
/// # Arguments
/// - `matter` / `crypto`: the borrowed rs-matter stack and crypto backend (as held
///   by [`crate::Controller`]).
/// - `ca` / `controller_fab_idx`: the controller CA and its local fabric index
///   (from [`ControllerCa::bootstrap`](crate::ca::ControllerCa::bootstrap)).
/// - `filter`: matches the device's BLE advertisement (typically a discriminator
///   from the scanned QR / manual pairing code).
/// - `passcode`: the device's PASE passcode (from the same QR / pairing code).
/// - `device_node_id`: the operational Node ID to assign the device.
/// - `creds`: the credentials for the network the device should join. Which kind is
///   needed is *not* something the onboarding payload can tell us - it is read off
///   the device itself (see [`read_network_type`]) - so this must match the
///   device's actual network type. Pass `None` for an Ethernet device, which is
///   already on its network and needs no network commissioning at all.
/// - `udp`: a bound UDP socket for phase B (the IP transport). `[::]:0` works.
#[allow(clippy::too_many_arguments)]
pub async fn commission_ble<C, S>(
    matter: &Matter<'_>,
    crypto: &C,
    ca: &ControllerCa,
    controller_fab_idx: NonZeroU8,
    filter: &CommissionableFilter,
    passcode: u32,
    device_node_id: u64,
    creds: Option<&WirelessCreds<'_>>,
    udp: &S,
) -> Result<CommissionOutcome, Error>
where
    C: Crypto,
    for<'x> &'x S:
        rs_matter::transport::network::NetworkSend + rs_matter::transport::network::NetworkReceive,
{
    let connection = Connection::system()
        .await
        .map_err(|_| Error::from(ErrorCode::NoNetworkInterface))?;

    // ---- Discovery: find the device's commissionable BLE advertisement. --------
    info!("BLE commission: scanning for a commissionable device (filter: {filter:?})");

    let addr = bluez::scan(
        &connection,
        None,
        filter,
        Some(SCAN_TIMEOUT_SECS),
        |addr, adv| {
            info!("BLE commission: found candidate {addr} (adv: {adv:?})");
            Some(addr)
        },
    )
    .await?;

    info!("BLE commission: using device {addr}");

    // ---- Phase A — over BLE/BTP. -----------------------------------------------
    let phase1 = {
        let btp = Btp::new();
        // BlueZ reports an under-stated GATT MTU; the relaxed negotiation lets us
        // use the larger MTU the peer actually supports (mirrors the peripheral).
        btp.set_relaxed_mtu_nego(true);

        // Establish the initiator role and a clean session *before* any of the
        // three futures below run. The commissioner (`flow`) starts sending Matter
        // traffic (the PASE PBKDFParamRequest) as soon as the transport is up, so
        // the session must already be reset and in the initiator role - otherwise
        // a later reset inside the transport setup would race with, and wipe, that
        // first queued SDU.
        btp.reset();
        btp.set_initiator(true);

        // The BTP transport pump (matter side) and the GATT Central (BlueZ side).
        let mut transport = pin!(matter.run(crypto, &btp, &btp, NoNetwork));
        let mut central = pin!(bluez::run_central(&connection, None, addr, &btp));

        // The commissioning steps that run *over* this transport.
        let mut flow = pin!(phase_a_flow(
            matter,
            crypto,
            ca,
            controller_fab_idx,
            addr,
            passcode,
            device_node_id,
            creds,
        ));

        // Race the two transport drivers against the flow. The flow completing (or
        // erroring) is the only non-error way out; either transport ending first is
        // unexpected.
        let result = select(select(&mut transport, &mut central).coalesce(), &mut flow).await;

        let phase1 = match result {
            Either::First(transport_result) => {
                // A transport future returned before the flow — always an error path.
                transport_result?;
                return Err(ErrorCode::NoNetworkInterface.into());
            }
            Either::Second(flow_result) => flow_result?,
        };

        // Drop the BLE transport before bringing up the UDP one.
        matter.reset_transport()?;

        phase1
    };

    info!(
        "BLE commission: phase A ok (device fabric {}, node 0x{:016x}); device is joining Thread",
        phase1.fabric_index, phase1.device_node_id
    );

    // ---- Phase B — over IP. ----------------------------------------------------
    // Bring up the UDP transport + mDNS backend, and complete CASE once the device
    // has joined Thread and announced itself operationally.
    let mut transport = pin!(matter.run(crypto, udp, udp, NoNetwork));
    let mut mdns = pin!(crate::discovery::run_mdns_backend(matter, crypto));
    let mut complete = pin!(phase_b_complete(
        matter,
        crypto,
        controller_fab_idx,
        ca,
        &phase1,
    ));

    let result = select(select(&mut transport, &mut mdns).coalesce(), &mut complete).await;

    match result {
        Either::First(transport_result) => {
            transport_result?;
            matter.reset_transport()?;
            return Err(ErrorCode::NoNetworkInterface.into());
        }
        Either::Second(complete_result) => complete_result?,
    }

    matter.reset_transport()?;

    info!("BLE commission: phase B ok (CASE established, CommissioningComplete acknowledged)");

    Ok(CommissionOutcome {
        handle: DeviceHandle {
            fabric_index: controller_fab_idx,
            node_id: phase1.device_node_id,
        },
    })
}

/// Phase A steps that run over the live BLE/BTP transport: PASE → AddNOC, then
/// Thread network provisioning.
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
    provision_network(matter, crypto, peer, passcode, creds).await?;

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
) -> Result<(), Error> {
    let net_type = read_network_type(matter, crypto, peer, passcode).await?;

    info!("BLE commission: device network type is {net_type:?}");

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
    let handle = exchange
        .network_commissioning()
        .connect_network(ROOT_ENDPOINT_ID, |req| {
            req.network_id(OctetStr::new(network_id))?
                .breadcrumb(None)?
                .end()
        })
        .await?;
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

/// Decode a hex string (with optional whitespace) into bytes. Convenience for
/// callers that carry the Thread dataset as hex (e.g. from a CLI flag / env var).
pub fn decode_hex(s: &str) -> Result<Vec<u8>, Error> {
    let cleaned: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();

    if !cleaned.len().is_multiple_of(2) {
        return Err(ErrorCode::InvalidData.into());
    }

    let mut out = Vec::with_capacity(cleaned.len() / 2);
    for pair in cleaned.chunks_exact(2) {
        let hi = hex_val(pair[0]).ok_or(ErrorCode::InvalidData)?;
        let lo = hex_val(pair[1]).ok_or(ErrorCode::InvalidData)?;
        out.push((hi << 4) | lo);
    }

    Ok(out)
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod test {
    use alloc::vec;

    use super::decode_hex;

    #[test]
    fn decode_hex_round_trips() {
        assert_eq!(decode_hex("00ff10").unwrap(), vec![0x00, 0xff, 0x10]);
        assert_eq!(
            decode_hex("DE AD BE EF").unwrap(),
            vec![0xde, 0xad, 0xbe, 0xef]
        );
        assert!(decode_hex("abc").is_err()); // odd length
        assert!(decode_hex("zz").is_err()); // non-hex
    }
}
