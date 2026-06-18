//! The controller facade — the single API every transport (RPC, UniFFI, CLI)
//! drives.
//!
//! Phase 0 implements [`Controller::commission`] end-to-end against an
//! externally-running Matter accessory, lifted from rs-matter's
//! `commissioner_tests` example and generalized. `decommission`, `read_health`,
//! and `retrieve_logs` are stubbed against the real rs-matter APIs and filled
//! in next.

use core::num::NonZeroU8;
use core::pin::pin;

use std::net::{SocketAddr, UdpSocket};

use embassy_futures::select::{select, Either};
use embassy_time::{Duration, Timer};

use log::{error, info};

use rs_matter::cert::gen::VALID_FOREVER;
use rs_matter::cert::{MAX_CERT_TLV_AND_ASN1_LEN, MAX_CERT_TLV_LEN};
use rs_matter::crypto::{Crypto, RngCore as _, SecretKey as _, SigningSecretKey as _};
use rs_matter::error::{Error, ErrorCode};
use rs_matter::onboard::cac::{IcacGenerator, RcacGenerator};
use rs_matter::onboard::noc::NocGenerator;
use rs_matter::onboard::{CommissionOptions, Commissioner};
use rs_matter::transport::network::Address;
use rs_matter::Matter;

use rs_matter::dm::clusters::decl::identify::IdentifyClient as _;
use rs_matter::dm::clusters::decl::operational_credentials::OperationalCredentialsClient as _;
use rs_matter::transport::exchange::Exchange;
use rs_matter::transport::network::mdns::CommissionableFilter;

use crate::model::{
    CommissionOutcome, DeviceHandle, DeviceHealth, DiagSet, FabricPolicy, LogIntent,
};

extern crate alloc;
use alloc::vec::Vec;

const COMMISSION_TIMEOUT_SECS: u64 = 60;

/// Per-fabric identity the controller installs on a device.
///
/// In Phase 0 these are fixed to the conventional chip-tool admin values so the
/// spike commissions against `chip-all-clusters-app`. The persistent-fabric
/// path (later) draws these from the local store / a stable controller CA.
#[derive(Debug, Clone, Copy)]
pub struct FabricIdentity {
    pub fabric_id: u64,
    pub controller_node_id: u64,
    pub admin_vendor_id: u16,
}

impl Default for FabricIdentity {
    fn default() -> Self {
        // chip-tool's conventional admin NodeID + test vendor — matches what
        // `chip-all-clusters-app` expects on the device-side ACL.
        Self {
            fabric_id: 1,
            controller_node_id: 112233,
            admin_vendor_id: 0xFFF1,
        }
    }
}

/// The controller facade. Holds the borrowed `rs-matter` stack + crypto backend;
/// the caller owns lifetimes and the transport pump (see the `cli` crate for the
/// wiring).
pub struct Controller<'a, C: Crypto> {
    matter: &'a Matter<'a>,
    crypto: &'a C,
    identity: FabricIdentity,
}

impl<'a, C: Crypto> Controller<'a, C> {
    pub const fn new(matter: &'a Matter<'a>, crypto: &'a C, identity: FabricIdentity) -> Self {
        Self {
            matter,
            crypto,
            identity,
        }
    }

    /// Commission the device at `peer_addr` using `passcode`, assigning it
    /// `device_node_id` on our fabric.
    ///
    /// Installs a fresh offline RCAC→ICAC→NOC chain, adds the fabric to the
    /// stack, then drives [`Commissioner::commission`] (phase 1, over PASE) and
    /// [`Commissioner::complete_via_case`] (phase 2, over CASE).
    ///
    /// `policy` is recorded on the outcome; `Ephemeral` self-destruct
    /// (`RemoveFabric`) is performed by the caller after the work is done (or by
    /// a future `decommission`).
    pub async fn commission(
        &self,
        peer_addr: SocketAddr,
        passcode: u32,
        device_node_id: u64,
        policy: FabricPolicy,
    ) -> Result<CommissionOutcome, Error> {
        let crypto = self.crypto;
        let matter = self.matter;
        let id = self.identity;

        info!("=== Commissioner::commission (phase 1 — PASE) ===");

        // Offline CA chain: RCAC then ICAC; RCAC priv key discarded immediately.
        let mut rcac_buf = [0u8; MAX_CERT_TLV_AND_ASN1_LEN];
        let mut rcac_gen = RcacGenerator::new(&mut rcac_buf);
        let (rcac_priv, rcac) = rcac_gen.generate(crypto, id.fabric_id, VALID_FOREVER)?;

        let mut icac_buf = [0u8; MAX_CERT_TLV_AND_ASN1_LEN];
        let mut icac_gen = IcacGenerator::new(&mut icac_buf);
        let (icac_priv, icac) =
            icac_gen.generate(crypto, rcac_priv.reference(), rcac, VALID_FOREVER)?;
        drop(rcac_priv);

        // Controller operational keypair + CSR + canonicalized secret.
        let controller_secret_key = crypto.generate_secret_key()?;
        let mut controller_csr_buf = [0u8; 256];
        let controller_csr = controller_secret_key.csr(&mut controller_csr_buf)?;
        let mut controller_secret_key_canon = rs_matter::crypto::CanonPkcSecretKey::new();
        controller_secret_key.write_canon(&mut controller_secret_key_canon)?;

        // NocGenerator signs the controller NOC now, the device NOC during
        // commissioning. Serial is derived from the NodeID internally.
        let mut noc_buf = [0u8; MAX_CERT_TLV_AND_ASN1_LEN];
        let mut noc_generator =
            NocGenerator::create(icac_priv.reference(), rcac, icac, &mut noc_buf)?;

        let controller_noc = noc_generator.generate(
            crypto,
            controller_csr,
            id.controller_node_id,
            &[],
            VALID_FOREVER,
        )?;

        // Fabric IPK: 16 random bytes, shared across the fabric.
        let mut ipk = rs_matter::crypto::CanonAeadKey::new();
        crypto.rand()?.fill_bytes(ipk.access_mut());

        // Install the fabric in `matter.state.fabrics`.
        let controller_fab_idx = matter.with_state(|state| {
            state
                .fabrics
                .add(
                    crypto,
                    controller_secret_key_canon.reference(),
                    rcac,
                    controller_noc,
                    icac,
                    Some(ipk.reference()),
                    id.admin_vendor_id,
                    id.controller_node_id,
                )
                .map(|f| f.fab_idx())
        })?;

        // Scratch buffer for the Commissioner (stages RCAC/ICAC across the
        // async on-wire calls). See `Commissioner::new` for the size rule.
        let mut commissioner_buf = [0u8; MAX_CERT_TLV_LEN];
        let mut commissioner = Commissioner::new(
            matter,
            crypto,
            controller_fab_idx,
            &mut noc_generator,
            &mut commissioner_buf,
        );

        let opts = CommissionOptions {
            // chip-all-clusters-app ships the canonical test DAC; skip DCL
            // verification until real DAC verification lands.
            allow_test_attestation: true,
            ..CommissionOptions::default()
        };

        let peer = Address::Udp(peer_addr);

        // Phase 1 — ArmFailSafe → ... → AddNOC, over PASE.
        let phase1 = with_timeout(
            commissioner.commission(peer, passcode, &opts, device_node_id, VALID_FOREVER),
            "commission()",
        )
        .await?;
        info!(
            "phase 1 ok: device_fabric_index={}, device_node_id=0x{:016x}",
            phase1.fabric_index, phase1.device_node_id,
        );

        // Phase 2 — CASE + CommissioningComplete.
        info!("=== complete_via_case (phase 2 — CASE) ===");
        with_timeout(
            commissioner.complete_via_case(peer, &phase1),
            "complete_via_case()",
        )
        .await?;
        info!("phase 2 ok: CASE established, CommissioningComplete acknowledged");

        Ok(CommissionOutcome {
            handle: DeviceHandle {
                fabric_index: phase1.fabric_index,
                node_id: phase1.device_node_id,
            },
            policy,
        })
    }

    /// Discover the first commissionable device on the local network matching
    /// `filter` (default = any), via the compiled-in mDNS backend. Returns its
    /// address, which can be fed straight into [`Self::commission`].
    ///
    /// Runs the mDNS backend concurrently with the browse; the transport pump
    /// need not be running for this (mDNS is a separate socket / DBus path).
    pub async fn discover(
        &self,
        filter: &CommissionableFilter,
        timeout_ms: Option<u32>,
    ) -> Result<crate::discovery::DiscoveredDevice, Error> {
        crate::discovery::discover_commissionable(self.matter, self.crypto, filter, timeout_ms)
            .await
    }

    /// Discover up to `max` commissionable devices matching `filter`, for a UI
    /// pick-list. Returns whatever was found (possibly empty).
    pub async fn discover_all(
        &self,
        filter: &CommissionableFilter,
        max: u8,
        timeout_ms: Option<u32>,
    ) -> Vec<crate::discovery::DiscoveredDevice> {
        crate::discovery::discover_all(self.matter, self.crypto, filter, max, timeout_ms).await
    }

    /// Read a diagnostic health snapshot from a previously-commissioned device
    /// over a fresh CASE session. `dev` carries the `(fabric_index, node_id)`
    /// returned by [`Self::commission`] — that's all that's needed to re-address
    /// the device on our fabric.
    pub async fn read_health(&self, dev: DeviceHandle, which: DiagSet) -> DeviceHealth {
        // `self.crypto: &C` is `Copy` and impls `Crypto` (blanket `impl Crypto
        // for &T`), so each per-attribute exchange gets its own crypto handle.
        crate::diagnostics::read_health(
            self.matter,
            self.crypto,
            dev.fabric_index,
            dev.node_id,
            which,
        )
        .await
    }

    /// Retrieve a diagnostic log (`intent`) from a commissioned device, returning
    /// the raw bytes. Handles inline (small) and BDX-pushed (large) delivery; the
    /// transport pump must be running in a sibling task.
    pub async fn retrieve_logs(
        &self,
        dev: DeviceHandle,
        intent: LogIntent,
    ) -> Result<Vec<u8>, Error> {
        crate::logs::retrieve_logs(
            self.matter,
            self.crypto,
            dev.fabric_index,
            dev.node_id,
            intent,
        )
        .await
    }

    /// Remove our fabric from the device (`RemoveFabric`), self-destructing the
    /// controller's NOC. This is the `Ephemeral` policy's teardown and also the
    /// orphaned-fabric cleanup primitive.
    ///
    /// `dev.fabric_index` is the device-local fabric slot the device assigned to
    /// us during commissioning (carried on the [`DeviceHandle`]); a controller is
    /// allowed to pass its own `FabricIndex` to `RemoveFabric`.
    pub async fn decommission(&self, dev: DeviceHandle) -> Result<(), Error> {
        let exchange =
            Exchange::initiate(self.matter, self.crypto, dev.fabric_index, dev.node_id).await?;

        exchange
            .operational_credentials()
            .remove_fabric(0, |req| req.fabric_index(dev.fabric_index.get())?.end())
            .await?
            .complete()
            .await?;

        info!("decommissioned: removed fabric index {}", dev.fabric_index);
        Ok(())
    }

    /// Ask the device to physically identify itself (blink LED / beep) for
    /// `secs` seconds, via the Identify cluster (`0x0003`) on `endpoint`.
    ///
    /// Lets a technician locate which physical unit a UI entry corresponds to —
    /// the standard Matter "which one is this?" affordance. `endpoint` is the
    /// application endpoint hosting Identify (commonly `1`); pass `0` to target
    /// the root node if the device puts Identify there.
    pub async fn identify(&self, dev: DeviceHandle, endpoint: u16, secs: u16) -> Result<(), Error> {
        let exchange =
            Exchange::initiate(self.matter, self.crypto, dev.fabric_index, dev.node_id).await?;

        // `identify` is a DefaultSuccess command — it returns `()` and drains the
        // response internally (no handle to `.complete()`).
        exchange
            .identify()
            .identify(endpoint, |req| req.identify_time(secs)?.end())
            .await?;

        info!(
            "identify: device 0x{:016x} ep{endpoint} for {secs}s",
            dev.node_id
        );
        Ok(())
    }
}

/// Drive `fut` with the standard commissioning timeout, logging on expiry.
async fn with_timeout<F, T>(fut: F, what: &str) -> Result<T, Error>
where
    F: core::future::Future<Output = Result<T, Error>>,
{
    let mut fut = pin!(fut);
    let mut timeout = pin!(Timer::after(Duration::from_secs(COMMISSION_TIMEOUT_SECS)));
    match select(&mut fut, &mut timeout).await {
        Either::First(r) => r,
        Either::Second(_) => {
            error!("{what} timed out after {COMMISSION_TIMEOUT_SECS}s");
            Err(ErrorCode::RxTimeout.into())
        }
    }
}

/// Bind an ephemeral dual-stack IPv6 UDP socket for the controller transport.
///
/// A single `[::]:0` socket serves both IPv4 and IPv6 peers: rs-matter
/// canonicalizes peer addresses (IPv4-mapped IPv6 → IPv4) on both send-storage
/// and receive, so the session lookup matches regardless of the socket's family
/// — no per-family socket or `::ffff` mapping needed. (On Windows/BSD, where
/// IPv6 sockets can be v6-only by default, a future tweak may need to enable
/// dual-stack explicitly or pick the family per peer.)
pub fn bind_controller_socket() -> Result<async_io::Async<UdpSocket>, Error> {
    let bind_addr: SocketAddr = "[::]:0".parse().unwrap();
    let socket =
        async_io::Async::<UdpSocket>::bind(bind_addr).map_err(|_| ErrorCode::NoNetworkInterface)?;
    info!(
        "controller bound on {}",
        socket.get_ref().local_addr().unwrap()
    );
    Ok(socket)
}

// Re-export so callers don't reach into rs-matter for the obvious scalar type.
pub use rs_matter::onboard::CommissionResult;
#[allow(unused_imports)]
use NonZeroU8 as _; // documented in model::DeviceHandle::fabric_index
