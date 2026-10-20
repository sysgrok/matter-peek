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
//! *inbound* IM exchanges over the same CASE session. The controller node's one
//! responder ([`crate::node::run`]) takes them, and [`Reports`] routes each to its
//! subscription by publisher and subscription ID, as a [`DeviceHealth`] delta.
//! A report for no subscription of ours is refused, so the device drops it.
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
//! [`subscribe_health`] runs until the returned future is dropped (the cancel).
//! The transport pump (`matter.run(...)`) and the controller node
//! ([`crate::node::run`], with the same [`Reports`]) must run in sibling tasks.

use core::cell::{Cell, RefCell};
use core::num::NonZeroU8;

use embassy_futures::select::select;
use embassy_time::{Duration, Instant, Timer};

use log::{debug, info, warn};

use rs_matter::crypto::Crypto;
use rs_matter::dm::{ReportContext, ReportDataHandler};
use rs_matter::error::Error;
use rs_matter::im::client::{ImClient as _, SubscribeOutcome, TxOutcome};
use rs_matter::im::encoding::ReportDataResp;
use rs_matter::im::{AttrPath, GenericPath, IMStatusCode};
use rs_matter::transport::exchange::Exchange;
use rs_matter::utils::sync::Notification;
use rs_matter::Matter;

use crate::diagnostics::{health_cluster_ids, parse_health, ROOT_ENDPOINT};
use crate::model::{DeviceHealth, DiagSet};

extern crate alloc;
use alloc::rc::Rc;
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

/// Routes the reports of our health subscriptions to them, by publisher and
/// subscription ID. Shared by [`subscribe_health`] and the controller node's
/// responder ([`crate::node::run`]).
#[derive(Default)]
pub struct Reports {
    slots: RefCell<Vec<Rc<Slot>>>,
}

impl Reports {
    pub const fn new() -> Self {
        Self {
            slots: RefCell::new(Vec::new()),
        }
    }

    /// Take the reports of a subscription to `peer_node_id`, until the returned
    /// registration is dropped.
    fn register(&self, peer_node_id: u64, which: DiagSet) -> Registration<'_> {
        let slot = Rc::new(Slot {
            peer_node_id,
            subscription_id: Cell::new(None),
            which,
            updates: RefCell::new(Vec::new()),
            last_report: Cell::new(Instant::now()),
            received: Notification::new(),
        });

        self.slots.borrow_mut().push(slot.clone());

        Registration {
            reports: self,
            slot,
        }
    }

    /// The slot of subscription `subscription_id` to `peer_node_id`; or one to
    /// it still being established, which this report is the first of.
    fn slot(&self, peer_node_id: u64, subscription_id: u32) -> Option<Rc<Slot>> {
        let slots = self.slots.borrow();
        let mut to_peer = slots
            .iter()
            .filter(|slot| slot.peer_node_id == peer_node_id);

        to_peer
            .clone()
            .find(|slot| slot.subscription_id.get() == Some(subscription_id))
            .or_else(|| to_peer.find(|slot| slot.subscription_id.get().is_none()))
            .cloned()
    }
}

impl ReportDataHandler for Reports {
    async fn handle_report(
        &self,
        ctx: impl ReportContext,
        report: &ReportDataResp<'_>,
    ) -> Result<(), IMStatusCode> {
        let sub = ctx.subscription();

        let Some(slot) = sub
            .subscription_id
            .and_then(|id| self.slot(sub.peer_node_id, id))
        else {
            debug!(
                "report for no subscription of ours: sub_id={:?} peer=0x{:016x}",
                sub.subscription_id, sub.peer_node_id
            );
            return Err(IMStatusCode::InvalidSubscription);
        };

        let health = parse_health(report, slot.which);

        debug!(
            "subscription report: sub_id={:?} peer=0x{:016x} -> {health:?}",
            sub.subscription_id, sub.peer_node_id
        );

        // Any report is a liveness signal, even one with nothing we track.
        slot.last_report.set(Instant::now());
        slot.updates.borrow_mut().push(health);
        slot.received.notify();

        Ok(())
    }
}

/// The reports of one subscription, as they arrive.
struct Slot {
    peer_node_id: u64,
    /// `None` while the subscription is being established.
    subscription_id: Cell<Option<u32>>,
    which: DiagSet,
    updates: RefCell<Vec<DeviceHealth>>,
    /// When the most recent report arrived: the watchdog's heartbeat.
    last_report: Cell<Instant>,
    received: Notification,
}

/// A subscription's place in [`Reports`]; dropping it removes it.
struct Registration<'a> {
    reports: &'a Reports,
    slot: Rc<Slot>,
}

impl Drop for Registration<'_> {
    fn drop(&mut self) {
        self.reports
            .slots
            .borrow_mut()
            .retain(|slot| !Rc::ptr_eq(slot, &self.slot));
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
/// pump (`matter.run(...)`) and the controller node, with `reports`, must already
/// run in sibling tasks.
#[allow(clippy::too_many_arguments)]
pub async fn subscribe_health<C, FU, FS>(
    matter: &Matter<'_>,
    crypto: C,
    reports: &Reports,
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

    // Establish → watch → (on stale/error) tear down + re-establish, forever. A
    // re-subscribe gets a new device-side subscription id, invalidating the old
    // one. `&mut` callbacks are re-borrowed per pass.
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
                reports,
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
#[allow(clippy::too_many_arguments)]
async fn run_once<C, FU, FS>(
    matter: &Matter<'_>,
    crypto: C,
    reports: &Reports,
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
    // Registered before establishing, so that no early report is refused.
    let registration = reports.register(node_id, which);
    let slot = &registration.slot;

    let (subscription_id, max_int) =
        establish(matter, crypto, fab_idx, node_id, which, paths, on_update).await?;

    slot.subscription_id.set(Some(subscription_id));
    slot.last_report.set(Instant::now());

    // Clamped to ≥1s, so that a pathological max_int=0 does not make the window 0.
    let max_int = max_int.max(1);
    on_state(SubState::Live {
        max_int_secs: max_int,
    });

    let window = Duration::from_secs((max_int as u32 * STALE_MULTIPLIER) as u64);

    loop {
        for health in slot.updates.take() {
            on_update(health);
        }

        // Stale if no report came in the window after the last one.
        let deadline = slot.last_report.get() + window;
        let now = Instant::now();
        if now >= deadline {
            warn!(
                "subscription 0x{node_id:016x} stale: no report in {}s (>{STALE_MULTIPLIER}x max_int={max_int}s); re-establishing",
                (now - slot.last_report.get()).as_secs()
            );
            on_state(SubState::Reconnecting);
            return Ok(());
        }

        select(slot.received.wait(), Timer::at(deadline)).await;
    }
}

/// Establish the subscription: send `SubscribeRequest`, feed the priming
/// `ReportData` chunks to `on_update` as the first [`DeviceHealth`], and await
/// the `SubscribeResponse`. Returns the subscription ID and `max_int`.
async fn establish<C, F>(
    matter: &Matter<'_>,
    crypto: C,
    fab_idx: NonZeroU8,
    node_id: u64,
    which: DiagSet,
    paths: &[AttrPath],
    on_update: &mut F,
) -> Result<(u32, u16), Error>
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

    // Each priming chunk carries a slice of the current attribute values.
    let established = loop {
        {
            let resp = chunk.response()?;
            on_update(parse_health(&resp, which));
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

    Ok((established.subscription_id, established.max_int))
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
