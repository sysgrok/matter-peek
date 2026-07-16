//! Live health via **subscriptions** — the Phase 2 health-board primitive.
//!
//! Where [`diagnostics::read_health`](crate::diagnostics::read_health) takes a
//! one-shot snapshot, this establishes an IM *subscription* to the diagnostic
//! attributes and then keeps delivering updated [`DeviceHealth`] snapshots as the
//! device pushes server-initiated `ReportData` messages — for a live dashboard
//! instead of manual re-polling.
//!
//! ## How the report side works
//!
//! rs-matter's IM client covers subscribe *establishment* only (it ends at
//! `SubscribeOutcome::Established`). Post-establishment reports arrive as fresh
//! *inbound* IM exchanges over the same CASE session. To catch those, the
//! controller runs its own [`InteractionModel`] wired with a
//! [`ReportDataHandler`] — the generic controller-side facility we added to
//! rs-matter (`InteractionModel::new_with_reports`). The `InteractionModel`
//! accepts the inbound exchange, parses each `ReportData` chunk, ACKs it, and
//! hands it to our [`HealthReportHandler`], which demuxes it into a
//! [`DeviceHealth`] delta and invokes the caller's `on_update` closure.
//!
//! ## Liveness: watchdog + auto re-establish
//!
//! A subscription can silently die (the CASE session drops, the device reboots,
//! a NAT/UDP mapping expires) — and a *dead* subscription is indistinguishable
//! from a *healthy-but-quiet* one from the outside. The device commits to
//! reporting at least every `max_int` seconds (the negotiated maximum interval),
//! so [`subscribe_health`] runs a **watchdog**: if no report arrives within
//! [`STALE_MULTIPLIER`]× `max_int`, the subscription is deemed stale, torn down,
//! and **re-established** — a fresh `SubscribeRequest` yields a new subscription
//! id from the device, which implicitly invalidates the old one. The caller is
//! told about the lifecycle via [`SubState`] through its `on_state` callback.
//!
//! ## Concurrency
//!
//! [`subscribe_health`] runs the controller [`Responder`] concurrently with the
//! establishment/watch loop, for the lifetime of the returned future (drop it to
//! cancel — this tears the subscription down cleanly). The caller's transport
//! pump (`matter.run(...)`) must already be running in a sibling task.

use core::cell::{Cell, RefCell};
use core::num::NonZeroU8;

use embassy_futures::select::{select3, Either3};
use embassy_time::{Duration, Instant, Timer};

use log::{debug, info, warn};

use rs_matter::crypto::Crypto;
use rs_matter::dm::clusters::net_comm::{DummyNetworks, NetworkType};
use rs_matter::dm::networks::wireless::NoopWirelessNetCtl;
use rs_matter::dm::{EmptyHandler, Node, ReportContext, ReportDataHandler};
use rs_matter::error::Error;
use rs_matter::im::client::{ImClient as _, SubscribeOutcome, TxOutcome};
use rs_matter::im::encoding::ReportDataResp;
use rs_matter::im::{AttrPath, GenericPath, IMStatusCode, InteractionModel, InteractionModelState};
use rs_matter::persist::DummyKvBlobStore;
use rs_matter::respond::Responder;
use rs_matter::transport::exchange::{Exchange, MatterBuffers};
use rs_matter::Matter;

use crate::diagnostics::{health_cluster_ids, parse_health, ROOT_ENDPOINT};
use crate::model::{DeviceHealth, DiagSet};

extern crate alloc;
use alloc::vec::Vec;

/// Subscription reporting-interval bounds (seconds). `min = 0` lets the device
/// report as soon as an attribute changes; `max = 60` bounds the silent period
/// (the device must report at least this often, which also serves as the liveness
/// heartbeat the watchdog measures against).
const MIN_INTERVAL_FLOOR_SECS: u16 = 0;
const MAX_INTERVAL_CEIL_SECS: u16 = 60;

/// How many missed max-intervals mark a subscription stale. The device promises a
/// report every `max_int` seconds; we allow a generous 3× (network jitter, a
/// missed report or two) before concluding the subscription is dead and
/// re-establishing it.
const STALE_MULTIPLIER: u32 = 3;

/// Capped-exponential backoff bounds for re-establishment after a failed
/// (re-)subscribe. Starts small (a genuinely transient blip recovers fast) and
/// caps so a persistently-unreachable / session-exhausted device is retried at
/// most once a minute rather than hammered. A healthy pass resets to the min.
const REESTABLISH_BACKOFF_MIN: Duration = Duration::from_secs(2);
const REESTABLISH_BACKOFF_MAX: Duration = Duration::from_secs(60);

/// Lifecycle of a live subscription, reported to the caller via `on_state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubState {
    /// The subscription is established and reporting. Carries the negotiated
    /// maximum reporting interval (seconds) — the device will report at least
    /// this often.
    Live { max_int_secs: u16 },
    /// No report arrived within the staleness window (or establishment failed);
    /// the subscription is being torn down and re-established. The next
    /// [`SubState::Live`] marks recovery (with a *new* device-side sub id).
    Reconnecting,
}

/// A [`ReportDataHandler`] that demuxes each incoming `ReportData` into a
/// [`DeviceHealth`] delta, forwards it to the caller's `on_update` closure, and
/// stamps the arrival time so the watchdog can measure quiet periods.
///
/// The closures are behind `RefCell`/`Cell` because `handle_report` takes `&self`
/// (the handler is shared, borrowed by the IM) but the callback accumulates state.
struct HealthReportHandler<F> {
    which: DiagSet,
    on_update: RefCell<F>,
    /// When the most recent report was accepted — the watchdog's heartbeat.
    last_report: Cell<Instant>,
}

impl<F> HealthReportHandler<F>
where
    F: FnMut(DeviceHealth),
{
    fn new(which: DiagSet, on_update: F, now: Instant) -> Self {
        Self {
            which,
            on_update: RefCell::new(on_update),
            last_report: Cell::new(now),
        }
    }
}

impl<F> ReportDataHandler for HealthReportHandler<F>
where
    F: FnMut(DeviceHealth),
{
    async fn handle_report(
        &self,
        ctx: impl ReportContext,
        report: &ReportDataResp<'_>,
    ) -> Result<(), IMStatusCode> {
        let sub = ctx.subscription();
        let health = parse_health(report, self.which);

        debug!(
            "subscription report: sub_id={:?} peer=0x{:016x} -> {health:?}",
            sub.subscription_id, sub.peer_node_id
        );

        // Any accepted report is a liveness signal — stamp it even if the delta
        // carries no attributes we track (a keep-alive). Accepting (Ok) is what
        // keeps the subscription alive; rejecting would tear it down.
        self.last_report.set(Instant::now());
        (self.on_update.borrow_mut())(health);
        Ok(())
    }
}

/// Subscribe to the device's diagnostic attributes (`which`) and stream updated
/// [`DeviceHealth`] snapshots to `on_update` — **indefinitely**, self-healing
/// across session drops — until the returned future is dropped (the cancel).
///
/// - `on_update` receives the priming snapshot first, then deltas (merge them).
/// - `on_state` receives [`SubState`] transitions: `Live { max_int }` on each
///   (re-)establishment, `Reconnecting` when a stale/failed subscription is being
///   rebuilt. The UI uses these to show "live" vs. "reconnecting".
///
/// Never returns on its own (short of the caller dropping it): a lost
/// subscription is re-established rather than surfaced as an error. The transport
/// pump (`matter.run(...)`) must already be running in a sibling task.
pub async fn subscribe_health<C, FU, FS>(
    matter: &Matter<'_>,
    crypto: C,
    fab_idx: NonZeroU8,
    node_id: u64,
    which: DiagSet,
    mut on_update: FU,
    mut on_state: FS,
) -> Result<(), Error>
where
    C: Crypto + Copy,
    FU: FnMut(DeviceHealth),
    FS: FnMut(SubState),
{
    let paths = attr_paths(which);
    if paths.is_empty() {
        warn!("subscribe_health: no attributes selected; nothing to subscribe to");
        return Ok(());
    }

    // Establish → watch → (on stale/error) tear down + re-establish, forever. Each
    // pass builds a *fresh* controller IM + responder, so a re-subscribe gets a
    // new device-side subscription id (invalidating the old one) and a clean
    // report path. `&mut` callbacks are re-borrowed per pass.
    //
    // On repeated establishment failure we back off with a **capped exponential**
    // delay. This matters: each attempt opens a CASE session on the device, and a
    // tight retry loop against a cold / flaky device exhausts its (small) session
    // table, turning a transient failure into a permanent one. A successful pass
    // resets the backoff.
    let mut backoff = REESTABLISH_BACKOFF_MIN;
    loop {
        // Track whether this pass ever went `Live` (so we can reset backoff). We
        // wrap the caller's `on_state` to observe it.
        let established = Cell::new(false);
        let outcome = {
            let mut on_state_wrapped = |s: SubState| {
                if matches!(s, SubState::Live { .. }) {
                    established.set(true);
                }
                on_state(s);
            };
            run_once(
                matter,
                crypto,
                fab_idx,
                node_id,
                which,
                &paths,
                &mut on_update,
                &mut on_state_wrapped,
            )
            .await
        };

        // A pass that ever reached `Live` is a healthy one — reset the backoff.
        if established.get() {
            backoff = REESTABLISH_BACKOFF_MIN;
        }

        match outcome {
            Ok(()) => {
                // Stale re-establish: `on_state(Reconnecting)` already emitted. If
                // the pass had been healthy, retry promptly; otherwise back off.
                if !established.get() {
                    Timer::after(backoff).await;
                    backoff = (backoff * 2).min(REESTABLISH_BACKOFF_MAX);
                }
            }
            Err(e) => {
                // Establishment / transport error: report Reconnecting and back off
                // (capped-exponential) so a cold / unreachable device isn't hammered
                // (which would exhaust its CASE session table).
                warn!(
                    "subscribe_health(0x{node_id:016x}): {e:?}; re-establishing in {}s",
                    backoff.as_secs()
                );
                on_state(SubState::Reconnecting);
                Timer::after(backoff).await;
                backoff = (backoff * 2).min(REESTABLISH_BACKOFF_MAX);
            }
        }
    }
}

/// One establish-and-watch pass. Returns `Ok(())` when the subscription went
/// stale (caller should re-establish immediately), or `Err` on an
/// establishment/transport failure (caller should back off then retry).
///
/// Runs three things concurrently: the report [`Responder`], the establishment
/// flow (which primes the first snapshot and reports `max_int` via `on_state`),
/// and — once established — the staleness watchdog.
#[allow(clippy::too_many_arguments)]
async fn run_once<C, FU, FS>(
    matter: &Matter<'_>,
    crypto: C,
    fab_idx: NonZeroU8,
    node_id: u64,
    which: DiagSet,
    paths: &[AttrPath],
    on_update: &mut FU,
    on_state: &mut FS,
) -> Result<(), Error>
where
    C: Crypto + Copy,
    FU: FnMut(DeviceHealth),
    FS: FnMut(SubState),
{
    let node = Node::new(&[]);
    let handler = (node, EmptyHandler);
    let buffers: MatterBuffers = MatterBuffers::new();
    let state: InteractionModelState<DummyNetworks, 3, 1024> =
        InteractionModelState::new(DummyNetworks);
    let kv = matter.kv(DummyKvBlobStore);

    // Negotiated max interval, filled in by `establish` once the device commits.
    let max_int = Cell::new(0u16);

    let reports = HealthReportHandler::new(which, &mut *on_update, Instant::now());

    let im = InteractionModel::new_with_reports(
        matter,
        crypto,
        &buffers,
        handler,
        &kv,
        NoopWirelessNetCtl::new(NetworkType::Ethernet),
        &reports,
        &state,
    );

    // An **IM-only** responder (not `new_default`, which also bundles a
    // `SecureChannel` handler): a controller *initiates* CASE via the IM client
    // and must not run a second SC responder on the shared stack — that would
    // compete for inbound SC exchanges and corrupt the session on teardown. We
    // only need to accept inbound `ReportData` (IM).
    let responder = Responder::new("matter-peek-reports", &im, matter, 0);

    // Establishment reports `max_int` (via `max_int`) and, on success, keeps the
    // subscription's report path alive by pending. The watchdog then races it.
    let establish_fut = establish(
        matter, crypto, fab_idx, node_id, which, paths, &reports, &max_int,
    );

    // The watchdog only becomes meaningful after establishment sets `max_int`;
    // until then its window is 0 and it simply waits.
    let watchdog = async {
        // Wait for establishment to negotiate the interval.
        while max_int.get() == 0 {
            Timer::after(Duration::from_millis(100)).await;
        }
        let max = max_int.get();
        on_state(SubState::Live { max_int_secs: max });

        let window = Duration::from_secs((max as u32 * STALE_MULTIPLIER) as u64);
        loop {
            // Sleep until the current deadline (last report + window); if a report
            // arrived meanwhile, `last_report` advanced and we sleep again.
            let deadline = reports.last_report.get() + window;
            let now = Instant::now();
            if now >= deadline {
                warn!(
                    "subscription 0x{node_id:016x} stale: no report in {}s (>{STALE_MULTIPLIER}x max_int={max}s); re-establishing",
                    (now - reports.last_report.get()).as_secs()
                );
                on_state(SubState::Reconnecting);
                return; // stale → caller re-establishes
            }
            Timer::after(deadline - now).await;
        }
    };

    // Race the three. `responder` and `establish` don't complete on their own in
    // the happy path; the watchdog is what ends a pass (by returning → stale).
    match select3(responder.run::<4>(), establish_fut, watchdog).await {
        // Responder exited — unexpected while we still want reports. Surface it as
        // an error so the outer loop re-establishes after a backoff.
        Either3::First(r) => {
            r?;
            Err(Error::from(rs_matter::error::ErrorCode::NoExchange))
        }
        // Establishment finished/failed. On Ok it pended forever, so this arm is
        // an establishment error → propagate for backoff+retry.
        Either3::Second(r) => r.map(|()| {
            // `establish` only returns Ok on the (unreachable) pend completion.
        }),
        // Watchdog fired → stale. Ok(()) tells the outer loop to re-establish now.
        Either3::Third(()) => Ok(()),
    }
}

/// Establish the subscription (send `SubscribeRequest`, walk the priming
/// `ReportData` chunks into the first [`DeviceHealth`], await the
/// `SubscribeResponse`), record `max_int`, then pend so the responder keeps
/// receiving reports.
#[allow(clippy::too_many_arguments)]
async fn establish<C, F>(
    matter: &Matter<'_>,
    crypto: C,
    fab_idx: NonZeroU8,
    node_id: u64,
    which: DiagSet,
    paths: &[AttrPath],
    reports: &HealthReportHandler<F>,
    max_int: &Cell<u16>,
) -> Result<(), Error>
where
    C: Crypto,
    F: FnMut(DeviceHealth),
{
    let exchange = Exchange::initiate(matter, crypto, fab_idx, node_id).await?;
    let mut sender = exchange.subscribe_sender().await?;

    let mut chunk = loop {
        match sender.tx().await? {
            TxOutcome::BuildRequest(builder) => {
                sender = builder
                    .keep_subs(true)?
                    .min_int_floor(MIN_INTERVAL_FLOOR_SECS)?
                    .max_int_ceil(MAX_INTERVAL_CEIL_SECS)?
                    .attr_requests_from(paths)?
                    .fabric_filtered(false)?
                    .end()?;
            }
            TxOutcome::GotResponse(c) => break c,
        }
    };

    // Walk the priming chunks: each carries a slice of the current attribute
    // values. Feed each through the report handler so the caller's first
    // update(s) are the priming snapshot (this also stamps `last_report`).
    let established = loop {
        {
            let resp = chunk.response()?;
            let health = parse_health(&resp, which);
            reports.last_report.set(Instant::now());
            (reports.on_update.borrow_mut())(health);
        }
        match chunk.complete().await? {
            SubscribeOutcome::NextChunk(next) => chunk = next,
            SubscribeOutcome::Established(est) => break est,
        }
    };

    info!(
        "health subscription established: sub_id={} max_int={}s (peer 0x{node_id:016x})",
        established.subscription_id, established.max_int
    );

    // Publish the negotiated interval so the watchdog can arm (clamp to ≥1s so a
    // pathological max_int=0 doesn't make the window zero).
    max_int.set(established.max_int.max(1));

    // The subscription now lives as the (fabric, peer, sub_id) triple; reports
    // arrive on the responder. Pend so the enclosing race stays open until the
    // watchdog fires or the caller drops us.
    core::future::pending::<()>().await;
    Ok(())
}

/// Build the subscription's `AttrPath` list: one **cluster-wildcard** path per
/// selected diagnostic cluster (endpoint 0, `attr = None`). Cluster granularity
/// (rather than per-attribute) is what lets the subscription survive a device
/// that implements only a subset of the attributes — see
/// [`health_cluster_ids`](crate::diagnostics::health_cluster_ids).
fn attr_paths(which: DiagSet) -> Vec<AttrPath> {
    health_cluster_ids(which)
        .into_iter()
        .map(|cluster| {
            AttrPath::from_gp(&GenericPath::new(Some(ROOT_ENDPOINT), Some(cluster), None))
        })
        .collect()
}
