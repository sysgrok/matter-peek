//! [`Controller`]: the Matter operations the service builds on (commission,
//! discover, read and follow health, retrieve logs, identify, decommission, OTA),
//! over a borrowed rs-matter stack.

use core::net::SocketAddr;
use core::num::NonZeroU8;
use core::pin::pin;

use embassy_futures::select::{select, Either};
use embassy_time::{Duration, Timer};

use log::{error, info};

use rs_matter::cert::gen::VALID_FOREVER;
use rs_matter::cert::{MAX_CERT_TLV_AND_ASN1_LEN, MAX_CERT_TLV_LEN};
use rs_matter::crypto::Crypto;
use rs_matter::error::{Error, ErrorCode};
use rs_matter::onboard::{CommissionOptions, Commissioner};
use rs_matter::transport::network::Address;
use rs_matter::transport::session::{Session, SessionMode};
use rs_matter::utils::sync::Notification;
use rs_matter::Matter;

use rs_matter::dm::clusters::decl::identify::IdentifyClient as _;
use rs_matter::dm::clusters::decl::operational_credentials::{
    OperationalCredentialsAttrReads as _, OperationalCredentialsAttrResponses as _,
    OperationalCredentialsClient as _,
};
use rs_matter::im::client::ImClient as _;
use rs_matter::transport::exchange::Exchange;
use rs_matter::transport::network::mdns::CommissionableFilter;

use crate::model::{CommissionOutcome, DeviceHandle, DeviceHealth, DiagSet, LogIntent};

extern crate alloc;
use alloc::vec::Vec;

/// Per-phase (PASE / CASE) timeout for a commission. Kept short so a commission
/// against an unreachable device fails — and thus becomes retryable / the UI
/// recovers — promptly, rather than holding the service for a minute. A
/// reachable device commissions in well under a second; even a slow LAN is
/// comfortably inside this. (Cancellation proper is cooperative at phase
/// boundaries; this bounds the one phase a cancel can't reach into.)
const COMMISSION_TIMEOUT_SECS: u64 = 20;

/// Per-fabric identity the controller installs on a device.
///
/// The conventional chip-tool admin values by default.
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
}

impl<'a, C: Crypto> Controller<'a, C> {
    pub const fn new(matter: &'a Matter<'a>, crypto: &'a C) -> Self {
        Self { matter, crypto }
    }

    /// Commission the device at `peer_addr` using `passcode`, assigning it
    /// `device_node_id` on the controller fabric `controller_fab_idx`, signing
    /// the device NOC with `ca`.
    ///
    /// The controller fabric + CA must already exist (via
    /// [`ControllerCa::bootstrap`](crate::ca::ControllerCa::bootstrap), once).
    /// This drives
    /// [`Commissioner::commission`] (phase 1, over PASE) and
    /// [`Commissioner::complete_via_case`] (phase 2, over CASE).
    ///
    /// `peer_addr` addresses phase 1 only. Phase 2 reaches the device through its
    /// *operational* identity, which rs-matter resolves from `(fabric, node-id)`
    /// over mDNS — so an mDNS backend must be running concurrently (see
    /// [`run_mdns_backend`](crate::discovery::run_mdns_backend)), even when
    /// `peer_addr` was supplied by hand and no discovery was needed.
    ///
    /// Cancellable up to the point of
    /// no return (AddNOC). `cancel()` is polled at the safe pre-AddNOC boundaries
    /// (before the orphan sweep, after it, before phase 1) and the orphan sweep —
    /// the step most prone to hang when re-commissioning a device whose window is
    /// closed — is *raced* against it. Once phase 1 begins, commissioning is
    /// atomic and runs to completion (or its timeout); interrupting mid-AddNOC
    /// could orphan a fabric, so we don't.
    ///
    /// Returns [`CommissionFlow::Cancelled`] when cancellation took effect
    /// before any device fabric was installed (nothing to clean up).
    #[allow(clippy::too_many_arguments)]
    pub async fn commission(
        &self,
        ca: &crate::ca::ControllerCa,
        controller_fab_idx: NonZeroU8,
        peer_addr: SocketAddr,
        passcode: u32,
        device_node_id: u64,
        live_node_ids: &[u64],
        cancel: impl Fn() -> bool,
    ) -> Result<CommissionFlow, Error> {
        let crypto = self.crypto;
        let matter = self.matter;

        if cancel() {
            return Ok(CommissionFlow::Cancelled);
        }

        // Start this attempt from a clean PASE.
        //
        // `Exchange::initiate_pase` reuses an existing PASE session to a peer
        // *address* unconditionally - it checks neither the session's age nor
        // whether the peer still has its half, and ignores the passcode entirely
        // on the reuse path. That is what lets the steps below share one PASE
        // session, but it means a second attempt silently encrypts to a session
        // the device forgot when it rebooted, and every packet simply times out.
        // Symptom: exactly one successful commission per process, then
        // `TxTimeout` forever.
        let peer = Address::Udp(peer_addr).canonical();
        let dropped = remove_sessions(matter, |sess| {
            matches!(sess.get_session_mode(), SessionMode::Pase { .. })
                && sess.get_peer_addr().canonical() == peer
        });
        if dropped > 0 {
            info!("dropped {dropped} stale PASE session(s) for {peer_addr}");
        }

        // Likewise any CASE session to the NodeID we are about to assign. NodeIDs
        // are reused - after a decommission empties the store, the next device
        // gets the same base ID - so a CASE session left over from the previous
        // occupant matches `(fabric, node)` exactly, and phase 2 resumes it
        // instead of running CASE. It then burns a full MRP timeout before
        // `complete_via_case` retries (and fails outright where it is given a
        // single attempt). Dropping it here is safe: `device_node_id` is freshly
        // allocated, so no live device can hold it, and no exchange is in flight.
        let dropped = remove_sessions(matter, |sess| {
            sess.is_encrypted()
                && sess.get_local_fabric_idx() == controller_fab_idx.get()
                && sess.get_peer_node_id() == Some(device_node_id)
        });
        if dropped > 0 {
            info!("dropped {dropped} stale CASE session(s) for node 0x{device_node_id:016x}");
        }

        // Reclaim any fabric slot we leaked on this device in a previous run
        // (commissioned, then crashed before RemoveFabric). Best-effort over the
        // PASE window, before AddNOC — so a device whose fabric table is full of
        // our orphans doesn't reject the new commission with `TableFull`.
        //
        // This is also the step most likely to HANG when re-commissioning a
        // device whose commissioning window has since closed (its PASE never
        // completes). So it is both time-bounded *and* raced against `cancel` —
        // and it is entirely pre-AddNOC, so abandoning it is always safe.
        let sweep =
            self.sweep_orphaned_fabrics(ca, controller_fab_idx, peer_addr, passcode, live_node_ids);
        match race_cancel(with_timeout(sweep, "orphan-sweep"), &cancel).await {
            Raced::Cancelled => return Ok(CommissionFlow::Cancelled),
            Raced::Done(Ok(reclaimed)) if reclaimed > 0 => {
                info!("orphaned-fabric sweep: reclaimed {reclaimed} stale fabric slot(s)");
            }
            Raced::Done(Ok(_)) => {}
            Raced::Done(Err(e)) => info!("orphaned-fabric sweep skipped (non-fatal): {e:?}"),
        }

        if cancel() {
            return Ok(CommissionFlow::Cancelled);
        }

        info!("=== Commissioner::commission (phase 1 — PASE) ===");

        // NocGenerator backed by the controller CA — signs the *device* NOC
        // during commissioning. Serial is derived from the NodeID internally.
        let mut noc_buf = [0u8; MAX_CERT_TLV_AND_ASN1_LEN];
        let mut noc_generator = ca.noc_generator(&mut noc_buf)?;

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
            commissioner.complete_via_case(&phase1),
            "complete_via_case()",
        )
        .await?;
        info!("phase 2 ok: CASE established, CommissioningComplete acknowledged");

        Ok(CommissionFlow::Done(CommissionOutcome {
            handle: DeviceHandle {
                fabric_index: controller_fab_idx,
                node_id: phase1.device_node_id,
            },
        }))
    }

    /// Remove any fabric the device still holds that belongs to **us** — i.e.
    /// orphans from a previous run that commissioned this device but crashed
    /// before `RemoveFabric`. Runs over the **PASE** window (no fabric of ours is
    /// needed yet), the canonical Matter re-commissioning cleanup.
    ///
    /// "Ours" = the device's `FabricDescriptor.root_public_key` equals our RCAC
    /// public key **and** its `fabric_id` equals ours (belt-and-suspenders). A
    /// non-fabric-filtered read returns every fabric on the device, so we can see
    /// them all over PASE.
    ///
    /// **`live_node_ids` are not orphans.** A fabric of ours whose
    /// `FabricDescriptor.node_id` (the NodeID *this device* uses on that fabric)
    /// still has a record in our store is the device's *current* commissioning,
    /// not a leak. Sweeping it would decommission the very device the caller is
    /// re-commissioning — and since `RemoveFabric` of a device's last fabric
    /// makes it drop its Matter state (typically closing the commissioning
    /// window), the commission that follows would then fail, leaving the device
    /// removed instead of re-commissioned. So those indices are skipped.
    ///
    /// Best-effort and non-destructive to *other* admins: only our own fabrics
    /// match. Returns the count removed. Errors are surfaced to the caller, which
    /// treats the whole sweep as optional (a device with no orphans, or one that
    /// refuses the read, just proceeds to normal commissioning).
    async fn sweep_orphaned_fabrics(
        &self,
        ca: &crate::ca::ControllerCa,
        controller_fab_idx: NonZeroU8,
        peer_addr: SocketAddr,
        passcode: u32,
        live_node_ids: &[u64],
    ) -> Result<usize, Error> {
        const ROOT_ENDPOINT: rs_matter::dm::EndptId = 0;

        let our_rcac_pubkey = ca.rcac_public_key()?;
        let our_fabric_id = self.matter.with_state(|state| {
            Ok::<_, Error>(state.fabrics.fabric(controller_fab_idx)?.fabric_id())
        })?;

        let peer = Address::Udp(peer_addr);

        // Read the full Fabrics list (non-fabric-filtered) over PASE and collect
        // the fabric indices whose root pubkey + fabric_id match ours. We copy
        // the small set of indices out of the borrowed response before it drops.
        let exchange = Exchange::initiate_pase(self.matter, self.crypto, peer, passcode).await?;
        let mut chunk = exchange
            .read_with(|msg| {
                msg.attr_requests()?
                    .operational_credentials_read()
                    .fabrics(ROOT_ENDPOINT)?
                    .end()?
                    .fabric_filtered(false)?
                    .end()
            })
            .await?;

        let mut ours: Vec<u8> = Vec::new();
        {
            let resp = chunk.response()?;
            for (_endpoint, result) in resp.operational_credentials_read_resp().fabrics() {
                let list = result?;
                for fabric in &list {
                    let fabric = fabric?;
                    // `root_public_key` is a TLV octet string (`Octets`, derefs to
                    // `[u8]`); `node_id` is the NodeID the *device* holds on that
                    // fabric; `fabric_index` is the optional ctx-254 field.
                    let device_node_id = fabric.node_id()?;
                    let verdict = classify_fabric(
                        &fabric.root_public_key()?,
                        fabric.fabric_id()?,
                        device_node_id,
                        our_rcac_pubkey.as_slice(),
                        our_fabric_id,
                        live_node_ids,
                    );
                    match verdict {
                        FabricVerdict::NotOurs => {}
                        FabricVerdict::Live => info!(
                            "orphaned-fabric sweep: keeping our live fabric for node 0x{device_node_id:016x} (not an orphan)"
                        ),
                        FabricVerdict::Orphan => {
                            if let Some(idx) = fabric.fabric_index()? {
                                ours.push(idx);
                            }
                        }
                    }
                }
            }
        }
        while let Some(next) = chunk.complete().await? {
            chunk = next;
        }

        if ours.is_empty() {
            return Ok(0);
        }
        info!(
            "orphaned-fabric sweep: {} fabric(s) on device belong to us {:?}",
            ours.len(),
            ours
        );

        // Remove each — one PASE exchange per RemoveFabric (mirrors how the
        // Commissioner opens a fresh PASE exchange per step on the same session).
        let mut removed = 0usize;
        for idx in ours {
            let exchange =
                Exchange::initiate_pase(self.matter, self.crypto, peer, passcode).await?;
            match exchange
                .operational_credentials()
                .remove_fabric(ROOT_ENDPOINT, |req| req.fabric_index(idx)?.end())
                .await
            {
                Ok(handle) => {
                    handle.complete().await?;
                    info!("orphaned-fabric sweep: removed our stale fabric index {idx}");
                    removed += 1;
                }
                Err(e) => {
                    // Don't abort the whole sweep on one failure — keep reclaiming.
                    error!("orphaned-fabric sweep: RemoveFabric({idx}) failed: {e:?}");
                }
            }
        }

        Ok(removed)
    }

    /// Discover up to `max` commissionable devices matching `filter`, for a
    /// pick-list. Assumes the caller runs the mDNS backend (via
    /// [`Self::run_mdns_backend`]), so it only places browse requests.
    pub async fn discover_all(
        &self,
        filter: &CommissionableFilter,
        max: u8,
        window_ms: Option<u32>,
    ) -> Vec<crate::discovery::DiscoveredDevice> {
        crate::discovery::discover_all_with_running_backend(self.matter, filter, max, window_ms)
            .await
    }

    /// The devices advertising on our fabric `fab_idx`: those holding it that
    /// are reachable, by node ID. Assumes the caller runs the mDNS backend (see
    /// [`Self::discover_all`]).
    pub async fn browse_fabric(&self, fab_idx: NonZeroU8) -> Result<Vec<u64>, Error> {
        crate::discovery::browse_fabric_with_running_backend(self.matter, fab_idx, None).await
    }

    /// Run the mDNS backend **forever** (returns only on error). A long-lived host
    /// must run this as a permanent concurrent task so that **operational
    /// resolves** — needed by `Exchange::initiate` to re-address a persisted
    /// device over CASE (subscribe / peek / decommission after the secure session
    /// has lapsed) — are serviced. Without it, those resolves time out with
    /// `NotFound`. See [`run_mdns_backend`](crate::discovery::run_mdns_backend).
    pub async fn run_mdns_backend(&self) -> Result<(), Error> {
        crate::discovery::run_mdns_backend(self.matter, self.crypto).await
    }

    /// Read a diagnostic health snapshot from a previously-commissioned device
    /// over a fresh CASE session. `dev` carries the `(fabric_index, node_id)`
    /// returned by [`Self::commission`] — that's all that's needed to re-address
    /// the device on our fabric.
    pub async fn read_health(
        &self,
        dev: DeviceHandle,
        which: DiagSet,
    ) -> Result<DeviceHealth, Error> {
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

    /// Subscribe to a device's diagnostic attributes and stream live
    /// [`DeviceHealth`](crate::model::DeviceHealth) updates to `on_update` — the
    /// health-board primitive (vs. the one-shot [`Self::read_health`]).
    ///
    /// - `on_update` receives the priming snapshot first, then deltas.
    /// - `on_state` receives [`SubState`](crate::subscribe::SubState) lifecycle
    ///   transitions (`Live { max_int }` / `Offline`), so the caller can show
    ///   connection health.
    /// - `retry` cuts the wait before the next re-establishing attempt short.
    ///
    /// Runs **indefinitely**, self-healing across session drops (a stale
    /// subscription is re-established with a fresh device-side id), until the
    /// returned future is dropped (the cancel). The transport pump and the
    /// controller node ([`crate::node::run`], with the same `reports`) must run in
    /// sibling tasks.
    pub async fn subscribe_health(
        &self,
        reports: &crate::subscribe::Reports,
        dev: DeviceHandle,
        which: DiagSet,
        retry: &Notification,
        on_update: impl FnMut(DeviceHealth),
        on_state: impl FnMut(crate::subscribe::SubState),
    ) -> Result<(), Error> {
        crate::subscribe::subscribe_health(
            self.matter,
            self.crypto,
            reports,
            dev.fabric_index,
            dev.node_id,
            which,
            retry,
            on_update,
            on_state,
        )
        .await
    }

    /// Serve what devices reach on us: the OTA Provider of `ota` and the reports
    /// of the subscriptions using `reports`. See [`crate::node::run`].
    pub async fn run_node<P>(
        &self,
        reports: &crate::subscribe::Reports,
        ota: &P,
    ) -> Result<(), Error>
    where
        P: rs_matter::dm::clusters::ota_prov::OtaImagesRegistry
            + rs_matter::dm::clusters::ota_prov::OtaImages,
    {
        crate::node::run(self.matter, self.crypto, reports, ota).await
    }

    /// Announce us to the device as its OTA Provider, with an update for it: it
    /// then asks us for the update it is allowed (see
    /// [`OtaProvider::allow`](crate::ota::OtaProvider::allow)).
    pub async fn announce_ota_provider(&self, dev: DeviceHandle) -> Result<(), Error> {
        crate::ota::announce(self.matter, self.crypto, dev.fabric_index, dev.node_id).await
    }

    /// Read a device's stable identity (`UniqueID` / `SerialNumber` /
    /// `NodeLabel`) from BasicInformation over a fresh CASE session. Used to list
    /// a device once commissioned or found on our fabric.
    pub async fn read_basic_info(&self, dev: DeviceHandle) -> crate::model::DeviceIdentity {
        crate::basic_info::read_basic_info(self.matter, self.crypto, dev.fabric_index, dev.node_id)
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
    /// controller's NOC. Decommissioning, and also the orphaned-fabric cleanup
    /// primitive.
    ///
    /// `RemoveFabric` takes the device's own index of our fabric, which is
    /// unrelated to ours, so it is read first (`CurrentFabricIndex`).
    pub async fn decommission(&self, dev: DeviceHandle) -> Result<(), Error> {
        let device_fab_idx = self.device_fabric_index(dev).await?;

        let exchange =
            Exchange::initiate(self.matter, self.crypto, dev.fabric_index, dev.node_id).await?;

        exchange
            .operational_credentials()
            .remove_fabric(0, |req| req.fabric_index(device_fab_idx)?.end())
            .await?
            .complete()
            .await?;

        info!("decommissioned: removed device fabric index {device_fab_idx}");

        // NOTE: the stale session is dropped by the *caller*, not here. This
        // exchange queues a trailing close on that session for the transport
        // pump to send; removing the session first makes the pump log a TX
        // failure for it. See `forget_sessions`.

        Ok(())
    }

    /// The device's index of our fabric.
    async fn device_fabric_index(&self, dev: DeviceHandle) -> Result<u8, Error> {
        let exchange =
            Exchange::initiate(self.matter, self.crypto, dev.fabric_index, dev.node_id).await?;

        let mut chunk = exchange
            .read_with(|msg| {
                msg.attr_requests()?
                    .operational_credentials_read()
                    .current_fabric_index(0)?
                    .end()?
                    .fabric_filtered(true)?
                    .end()
            })
            .await?;

        let mut fab_idx = None;
        for (_endpoint, result) in chunk
            .response()?
            .operational_credentials_read_resp()
            .current_fabric_index()
        {
            fab_idx = Some(result?);
        }
        while let Some(next) = chunk.complete().await? {
            chunk = next;
        }

        fab_idx.ok_or_else(|| ErrorCode::NotFound.into())
    }

    /// Drop any secure session we still hold with `dev`, returning how many were
    /// removed.
    ///
    /// Call this whenever we stop considering `dev` ours - after `RemoveFabric`,
    /// and also when we give up on an unreachable device and drop it locally.
    /// A session the peer no longer honours is not inert: the next thing that
    /// addresses that node resumes it, the device answers with an unsecured
    /// `SessionNotFound`, and the two nodes can end up trading Status Reports.
    /// Dropping it forces a clean CASE handshake if the device ever returns.
    pub fn forget_sessions(&self, dev: DeviceHandle) -> usize {
        let dropped = remove_sessions(self.matter, |sess| {
            sess.is_encrypted()
                && sess.get_local_fabric_idx() == dev.fabric_index.get()
                && sess.get_peer_node_id() == Some(dev.node_id)
        });

        if dropped > 0 {
            info!(
                "dropped {dropped} stale session(s) for node 0x{:016x}",
                dev.node_id
            );
        }

        dropped
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

/// Outcome of [`Controller::commission`].
#[derive(Debug)]
pub enum CommissionFlow {
    /// Commissioning completed.
    Done(CommissionOutcome),
    /// Cancelled at a safe pre-AddNOC point — no device fabric was installed, so
    /// there is nothing to clean up.
    Cancelled,
}

/// Remove every (non-reserved) session matching `predicate`, returning how many
/// were dropped.
///
/// Wakes the transport's session-removed waiters when anything was dropped, as
/// rs-matter does itself when it removes sessions.
pub(crate) fn remove_sessions<F>(matter: &Matter<'_>, predicate: F) -> usize
where
    F: FnMut(&Session) -> bool,
{
    let removed = matter.with_state(|state| state.sessions.remove_where(predicate));

    if removed > 0 {
        matter.transport().notify_session_removed();
    }

    removed
}

/// Internal result of racing a future against a cancel predicate.
enum Raced<T> {
    Done(T),
    Cancelled,
}

/// Drive `fut`, but bail with [`Raced::Cancelled`] as soon as `cancel()` returns
/// true. Cancellation is polled (sampled on a short tick alongside the future),
/// not edge-triggered — cheap, and enough to make a hung PASE abandonable within
/// a tick of the user pressing Cancel.
async fn race_cancel<F, T>(fut: F, cancel: &impl Fn() -> bool) -> Raced<T>
where
    F: core::future::Future<Output = T>,
{
    let mut fut = pin!(fut);
    loop {
        let mut tick = pin!(Timer::after(Duration::from_millis(100)));
        match select(&mut fut, &mut tick).await {
            Either::First(out) => return Raced::Done(out),
            Either::Second(_) => {
                if cancel() {
                    return Raced::Cancelled;
                }
            }
        }
    }
}

/// What a fabric found on a device means to the orphaned-fabric sweep.
///
/// See [`classify_fabric`]. The distinction that matters is `Live` vs `Orphan`:
/// both are *our* fabrics, but only one of them is safe to reclaim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FabricVerdict {
    /// Belongs to another administrator — never touch it.
    NotOurs,
    /// Ours, and a store record still points at it: this device's *current*
    /// commissioning. Removing it would decommission the device.
    Live,
    /// Ours, with no record pointing at it: a slot we leaked on a previous run.
    Orphan,
}

/// Classify one `FabricDescriptor` read off a device during the sweep.
///
/// "Ours" is `root_public_key` == our RCAC public key **and** `fabric_id` ==
/// ours (belt-and-suspenders). Among ours, a `device_node_id` that appears in
/// `live_node_ids` is the device's live commissioning, not a leak.
///
/// Pure (no I/O) so it is unit-testable in isolation.
pub fn classify_fabric(
    root_public_key: &[u8],
    fabric_id: u64,
    device_node_id: u64,
    our_rcac_pubkey: &[u8],
    our_fabric_id: u64,
    live_node_ids: &[u64],
) -> FabricVerdict {
    if root_public_key != our_rcac_pubkey || fabric_id != our_fabric_id {
        return FabricVerdict::NotOurs;
    }
    if live_node_ids.contains(&device_node_id) {
        return FabricVerdict::Live;
    }
    FabricVerdict::Orphan
}

#[cfg(test)]
mod tests {
    use super::{classify_fabric, FabricVerdict};

    const OURS: &[u8] = b"our-rcac-public-key";
    const THEIRS: &[u8] = b"someone-elses-rcac-key";
    const FABRIC: u64 = 1;
    const NODE: u64 = 0x1122_3344_5566_7788;

    #[test]
    fn another_admins_fabric_is_never_touched() {
        assert_eq!(
            classify_fabric(THEIRS, FABRIC, NODE, OURS, FABRIC, &[]),
            FabricVerdict::NotOurs
        );
    }

    #[test]
    fn our_key_on_a_different_fabric_id_is_not_ours() {
        assert_eq!(
            classify_fabric(OURS, FABRIC + 1, NODE, OURS, FABRIC, &[]),
            FabricVerdict::NotOurs
        );
    }

    #[test]
    fn our_fabric_with_no_record_is_an_orphan() {
        assert_eq!(
            classify_fabric(OURS, FABRIC, NODE, OURS, FABRIC, &[]),
            FabricVerdict::Orphan
        );
        // A record for a *different* device does not protect this one.
        assert_eq!(
            classify_fabric(OURS, FABRIC, NODE, OURS, FABRIC, &[NODE + 1]),
            FabricVerdict::Orphan
        );
    }

    /// The regression test: re-commissioning a device we already hold must not
    /// sweep away the fabric it is currently commissioned into. Doing so
    /// decommissioned the device instead of re-commissioning it.
    #[test]
    fn our_fabric_with_a_live_record_is_kept() {
        assert_eq!(
            classify_fabric(OURS, FABRIC, NODE, OURS, FABRIC, &[NODE]),
            FabricVerdict::Live
        );
        assert_eq!(
            classify_fabric(OURS, FABRIC, NODE, OURS, FABRIC, &[7, NODE, 9]),
            FabricVerdict::Live
        );
    }
}
