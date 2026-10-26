//! Live health, and data models, via **subscriptions**.
//!
//! Where [`diagnostics::read_health`](crate::diagnostics::read_health) takes a
//! one-shot snapshot, this establishes an IM *subscription* to the diagnostic
//! attributes and then keeps delivering updated [`DeviceHealth`] snapshots as the
//! device pushes server-initiated `ReportData` messages — for a live dashboard
//! instead of manual re-polling. While someone looks at a device's data model,
//! the same subscription covers that too (see [`Scope`]).
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
//! so [`subscribe`] runs a **watchdog**: if no report arrives within
//! `max_int` plus [`ROUND_TRIPS`] times the time a report may spend being
//! retransmitted and answered, the device is deemed offline, and the
//! subscription is torn down and **re-established** — a fresh `SubscribeRequest`
//! yields a new subscription id from the device, which implicitly invalidates the
//! old one. Failed attempts are retried with a backoff, which a `retry`
//! notification cuts short. The caller is told about the lifecycle via
//! [`SubState`] through its `on_state` callback.
//!
//! ## Concurrency
//!
//! [`subscribe`] runs until the returned future is dropped (the cancel).
//! The transport pump (`matter.run(...)`) and the controller node
//! ([`crate::node::run`], with the same [`Reports`]) must run in sibling tasks.

use core::cell::{Cell, RefCell};
use core::num::NonZeroU8;

use embassy_futures::poll_once;
use embassy_futures::select::{select, select4, Either, Either4};
use embassy_time::{Duration, Instant, Timer};

use log::{debug, info, warn};

use rs_matter::crypto::Crypto;
use rs_matter::dm::{ReportContext, ReportDataHandler};
use rs_matter::error::Error;
use rs_matter::im::client::{ImClient as _, SubscribeOutcome, TxOutcome};
use rs_matter::im::encoding::{EventFilter, EventPath, ReportDataResp};
use rs_matter::im::{AttrPath, GenericPath, IMStatusCode};
use rs_matter::transport::exchange::Exchange;
use rs_matter::transport::mrp::RetransEntry;
use rs_matter::transport::session::SessionMode;
use rs_matter::utils::sync::Notification;
use rs_matter::Matter;

use crate::datamodel::{self, Decoded, EventReport};
use crate::diagnostics::{fabric_paths, health_cluster_ids, parse_health, ROOT_ENDPOINT};
use crate::model::{DeviceHandle, DeviceHealth, DiagSet};

extern crate alloc;
use alloc::rc::Rc;
use alloc::vec::Vec;

/// Subscription reporting-interval bounds (seconds). `min = 0` lets the device
/// report as soon as an attribute changes; `max = 20` bounds the silent period
/// (the device must report at least this often, which also serves as the liveness
/// heartbeat the watchdog measures against). Sleepy devices (ICDs) pick their own
/// longer interval anyway, so this does not wake them more often.
const MIN_INTERVAL_FLOOR_SECS: u16 = 0;
const MAX_INTERVAL_CEIL_SECS: u16 = 20;

/// The reporting-interval floor of a subscription covering a data model: the
/// changes of its busier attributes come batched.
const MODEL_MIN_INTERVAL_FLOOR_SECS: u16 = 2;

/// How long a subscription keeps a wider coverage than its listeners want: a
/// view opened again soon needs no new priming report.
const NARROW_GRACE: Duration = Duration::from_secs(3 * 60);

/// The Descriptor cluster's list of a node's endpoints, on the root endpoint.
const DESCRIPTOR: u32 = 0x001D;
const PARTS_LIST: u32 = 0x0003;

/// Operational Credentials' fabrics.
const OPERATIONAL_CREDENTIALS: u32 = 0x003E;
const FABRICS: u32 = 0x0001;

/// Our MRP intervals (ms), which pace the device's retransmissions of a report
/// to us: the spec defaults, unless configured.
const DEFAULT_ACTIVE_INTERVAL_MS: u32 = 300;
const DEFAULT_IDLE_INTERVAL_MS: u32 = 500;
const ACTIVE_THRESHOLD_MS: u16 = 4_000;

/// Allowance for processing a report before answering it.
const REPORT_PROCESSING_MS: u64 = 2_000;

/// How many report round trips past `max_int` the watchdog waits for a report.
const ROUND_TRIPS: u32 = 2;

/// Capped-exponential backoff bounds for re-establishment after a failed
/// (re-)subscribe. Starts small (a genuinely transient blip recovers fast) and
/// caps so a persistently-unreachable / session-exhausted device is retried at
/// most once a minute rather than hammered. A healthy pass resets to the min.
const REESTABLISH_BACKOFF_MIN: Duration = Duration::from_secs(2);
const REESTABLISH_BACKOFF_MAX: Duration = Duration::from_secs(60);

/// What a subscription covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Coverage {
    /// The health.
    Health,
    /// The health and the application endpoints' attributes.
    Application,
    /// Every attribute.
    All,
}

/// What reports say beyond the health: attributes and events.
#[derive(Debug, Default)]
pub struct ModelUpdate {
    pub attributes: Vec<Decoded>,
    pub events: Vec<EventReport>,
}

impl ModelUpdate {
    fn is_empty(&self) -> bool {
        self.attributes.is_empty() && self.events.is_empty()
    }

    fn extend(&mut self, report: &ReportDataResp<'_>) {
        self.attributes.extend(decoded(report));
        self.events.extend(datamodel::decode_events(report));
    }
}

/// The coverage of a subscription: what its listeners want, and what it has,
/// wider at once and narrower only after [`NARROW_GRACE`].
pub struct Scope {
    wanted: Cell<Coverage>,
    current: Cell<Coverage>,
    narrow_at: Cell<Option<Instant>>,
    changed: Notification,
}

impl Scope {
    pub const fn new() -> Self {
        Self {
            wanted: Cell::new(Coverage::Health),
            current: Cell::new(Coverage::Health),
            narrow_at: Cell::new(None),
            changed: Notification::new(),
        }
    }

    /// Want `coverage` from now on.
    pub fn want(&self, coverage: Coverage) {
        self.wanted.set(coverage);

        if coverage > self.current.get() {
            self.current.set(coverage);
            self.narrow_at.set(None);
            self.changed.notify();
        } else if coverage < self.current.get() {
            if self.narrow_at.get().is_none() {
                self.narrow_at.set(Some(Instant::now() + NARROW_GRACE));
                self.changed.notify();
            }
        } else {
            self.narrow_at.set(None);
        }
    }

    /// What the subscription covers.
    pub fn current(&self) -> Coverage {
        self.current.get()
    }

    /// Narrow, if that is due; whether the coverage changed.
    fn settle(&self) -> bool {
        match self.narrow_at.get() {
            Some(at) if Instant::now() >= at => {
                self.narrow_at.set(None);
                let changed = self.current.get() != self.wanted.get();
                self.current.set(self.wanted.get());
                changed
            }
            _ => false,
        }
    }
}

impl Default for Scope {
    fn default() -> Self {
        Self::new()
    }
}

/// Lets a few subscriptions over Thread at a time take their priming report
/// of a data model: those share the mesh.
pub struct Gate {
    tickets: async_channel::Sender<()>,
    free: async_channel::Receiver<()>,
}

impl Gate {
    pub fn new(at_once: usize) -> Self {
        let (tickets, free) = async_channel::bounded(at_once.max(1));
        for _ in 0..at_once.max(1) {
            let _ = tickets.try_send(());
        }

        Self { tickets, free }
    }

    async fn enter(&self) -> Ticket<'_> {
        let _ = self.free.recv().await;
        Ticket { gate: self }
    }
}

/// A place in a [`Gate`]; dropping it frees the place.
struct Ticket<'a> {
    gate: &'a Gate,
}

impl Drop for Ticket<'_> {
    fn drop(&mut self) {
        let _ = self.gate.tickets.try_send(());
    }
}

/// Lifecycle of a live subscription, reported to the caller via `on_state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubState {
    /// The subscription is established and reporting. Carries the negotiated
    /// maximum reporting interval (seconds) — the device will report at least
    /// this often.
    Live { max_int_secs: u16 },
    /// No report arrived within the staleness window, or establishment failed:
    /// the device is offline. Re-establishing is retried in the background; the
    /// next [`SubState::Live`] marks recovery (with a *new* device-side sub id).
    Offline,
}

/// Routes the reports of our health subscriptions to them, by publisher and
/// subscription ID. Shared by [`subscribe`] and the controller node's
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
    fn register(&self, peer_node_id: u64, which: DiagSet, coverage: Coverage) -> Registration<'_> {
        let slot = Rc::new(Slot {
            peer_node_id,
            subscription_id: Cell::new(None),
            which,
            coverage,
            updates: RefCell::new(Vec::new()),
            model_updates: RefCell::new(ModelUpdate::default()),
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
                "report for no subscription of ours: sub_id={:?} peer=0x{:016X}",
                sub.subscription_id, sub.peer_node_id
            );
            return Err(IMStatusCode::InvalidSubscription);
        };

        let health = parse_health(report, slot.which);

        debug!(
            "subscription report: sub_id={:?} peer=0x{:016X} -> {health:?}",
            sub.subscription_id, sub.peer_node_id
        );

        // Any report is a liveness signal, even one with nothing we track.
        slot.last_report.set(Instant::now());
        slot.updates.borrow_mut().push(health);
        if slot.coverage != Coverage::Health {
            slot.model_updates.borrow_mut().extend(report);
        }
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
    coverage: Coverage,
    updates: RefCell<Vec<DeviceHealth>>,
    /// What was reported, for a coverage beyond the health.
    model_updates: RefCell<ModelUpdate>,
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

/// Subscribe to a device's health (`which`), and to as much of its data model
/// as `scope` says — **indefinitely**, self-healing across session drops —
/// until the returned future is dropped (the cancel).
///
/// - `on_update` receives the priming [`DeviceHealth`] first, then deltas
///   (merge them).
/// - `on_model` receives the attributes and events reported beyond the
///   health: the whole priming at once, with `true` (its attributes replace
///   those known), then deltas. Each event comes once: a re-established
///   subscription asks for those after the last.
/// - `on_state` receives [`SubState`] transitions: `Live { max_int }` on each
///   (re-)establishment, `Offline` when the subscription went stale or could not
///   be established. The UI uses these to show "live" vs. "offline".
/// - `retry` cuts the backoff before the next attempt short, e.g. once the
///   device answered something else; it re-establishes a live subscription
///   too, e.g. once the device checked in, which it does when it lost it.
///
/// A change of `scope` re-establishes the subscription with the paths it
/// needs. Over Thread (`thread`), `gate` lets a few at a time take the priming
/// report of a data model.
///
/// Never returns on its own (short of the caller dropping it): a lost
/// subscription is re-established rather than surfaced as an error. The transport
/// pump (`matter.run(...)`) and the controller node, with `reports`, must already
/// run in sibling tasks.
#[allow(clippy::too_many_arguments)]
pub async fn subscribe<C, FU, FM, FS>(
    matter: &Matter<'_>,
    crypto: C,
    reports: &Reports,
    gate: &Gate,
    dev: DeviceHandle,
    thread: bool,
    which: DiagSet,
    scope: &Scope,
    retry: &Notification,
    mut on_update: FU,
    mut on_model: FM,
    mut on_state: FS,
) -> Result<(), Error>
where
    C: Crypto + Copy,
    FU: FnMut(DeviceHealth),
    FM: FnMut(ModelUpdate, bool),
    FS: FnMut(SubState),
{
    if attr_paths(which).is_empty() {
        warn!("subscribe: no attributes selected; nothing to subscribe to");
        return Ok(());
    }

    let node_id = dev.node_id;
    // The application endpoints, once a coverage needs them.
    let mut endpoints: Option<Vec<u16>> = None;
    // The number of the next event to report.
    let next_event = Cell::new(0);
    let mut on_model = |update: ModelUpdate, fresh: bool| {
        if let Some(last) = update.events.iter().map(|e| e.number).max() {
            next_event.set(next_event.get().max(last + 1));
        }
        on_model(update, fresh);
    };

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
        let coverage = scope.current();
        if coverage == Coverage::Application && endpoints.is_none() {
            match app_endpoints(matter, crypto, dev).await {
                Ok(found) => endpoints = Some(found),
                Err(e) => {
                    warn!("subscribe(0x{node_id:016X}): no endpoints: {e:?}");
                    on_state(SubState::Offline);
                    backoff = back_off(backoff, retry).await;
                    continue;
                }
            }
        }
        let paths = paths_of(which, coverage, endpoints.as_deref().unwrap_or_default());
        let events = event_paths_of(coverage, endpoints.as_deref().unwrap_or_default());

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
                gate,
                dev,
                thread,
                which,
                scope,
                retry,
                coverage,
                &paths,
                &events,
                next_event.get(),
                &mut on_update,
                &mut on_model,
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
                // Stale or rescoped: re-establish. If the pass had been
                // healthy, promptly; otherwise back off.
                if !established.get() {
                    backoff = back_off(backoff, retry).await;
                }
            }
            Err(e) => {
                // Establishment / transport error: report Offline and back off
                // (capped-exponential) so a cold / unreachable device isn't hammered
                // (which would exhaust its CASE session table).
                warn!(
                    "subscribe(0x{node_id:016X}): {e:?}; re-establishing in {}s",
                    backoff.as_secs()
                );
                on_state(SubState::Offline);
                backoff = back_off(backoff, retry).await;
            }
        }
    }
}

/// The MRP round trip of a report of `node_id`: its retransmissions to us
/// (paced by our MRP intervals), our processing, and the retransmissions of our
/// answer (paced by the device's active interval).
fn report_round_trip(matter: &Matter<'_>, fab_idx: NonZeroU8, node_id: u64) -> Duration {
    let dev_det = matter.dev_det();
    let inbound = RetransEntry::retransmission_timeout_ms(
        dev_det.sai.unwrap_or(DEFAULT_ACTIVE_INTERVAL_MS),
        dev_det.sii.unwrap_or(DEFAULT_IDLE_INTERVAL_MS),
        ACTIVE_THRESHOLD_MS,
        false,
    );

    let peer_active_ms = matter.with_state(|state| {
        state
            .sessions
            .iter()
            .find(|sess| {
                matches!(sess.get_session_mode(), SessionMode::Case { fab_idx: f, .. } if *f == fab_idx)
                    && sess.get_peer_node_id() == Some(node_id)
            })
            .map(|sess| sess.get_peer_active_interval_ms())
    });
    let outbound = RetransEntry::retransmission_timeout_ms(
        peer_active_ms.unwrap_or(DEFAULT_ACTIVE_INTERVAL_MS),
        peer_active_ms.unwrap_or(DEFAULT_ACTIVE_INTERVAL_MS),
        0,
        true,
    );

    Duration::from_millis(inbound + REPORT_PROCESSING_MS + outbound)
}

/// Wait `backoff`, or less if `retry` is notified; returns the next backoff.
async fn back_off(backoff: Duration, retry: &Notification) -> Duration {
    match select(Timer::after(backoff), retry.wait()).await {
        Either::First(_) => (backoff * 2).min(REESTABLISH_BACKOFF_MAX),
        Either::Second(_) => {
            debug!("subscription retry requested");
            REESTABLISH_BACKOFF_MIN
        }
    }
}

/// One establish-and-watch pass. Returns `Ok(())` when the subscription went
/// stale or its scope changed (caller should re-establish immediately), or
/// `Err` on an establishment/transport failure (caller should back off then
/// retry).
#[allow(clippy::too_many_arguments)]
async fn run_once<C, FU, FM, FS>(
    matter: &Matter<'_>,
    crypto: C,
    reports: &Reports,
    gate: &Gate,
    dev: DeviceHandle,
    thread: bool,
    which: DiagSet,
    scope: &Scope,
    retry: &Notification,
    coverage: Coverage,
    paths: &[AttrPath],
    events: &[EventPath],
    next_event: u64,
    on_update: &mut FU,
    on_model: &mut FM,
    on_state: &mut FS,
) -> Result<(), Error>
where
    C: Crypto + Copy,
    FU: FnMut(DeviceHealth),
    FM: FnMut(ModelUpdate, bool),
    FS: FnMut(SubState),
{
    let (fab_idx, node_id) = (dev.fabric_index, dev.node_id);

    // Registered before establishing, so that no early report is refused.
    let registration = reports.register(node_id, which, coverage);
    let slot = &registration.slot;

    let ticket = if thread && coverage != Coverage::Health {
        Some(gate.enter().await)
    } else {
        None
    };
    let (subscription_id, max_int) = establish(
        matter, crypto, fab_idx, node_id, which, coverage, paths, events, next_event, on_update,
        on_model,
    )
    .await?;
    drop(ticket);

    slot.subscription_id.set(Some(subscription_id));
    slot.last_report.set(Instant::now());

    // Clamped to ≥1s, so that a pathological max_int=0 does not make the window 0.
    let max_int = max_int.max(1);
    on_state(SubState::Live {
        max_int_secs: max_int,
    });

    // A renewal asked for before this subscription was made is moot.
    let _ = poll_once(retry.wait());

    let window = Duration::from_secs(max_int as u64)
        + report_round_trip(matter, fab_idx, node_id) * ROUND_TRIPS;
    info!(
        "subscription 0x{node_id:016X} ({coverage:?}): counted offline after {} ms without a report",
        window.as_millis()
    );

    loop {
        for health in slot.updates.take() {
            on_update(health);
        }
        let model = slot.model_updates.take();
        if !model.is_empty() {
            on_model(model, false);
        }

        // Stale if no report came in the window after the last one.
        let deadline = slot.last_report.get() + window;
        let now = Instant::now();
        if now >= deadline {
            warn!(
                "subscription 0x{node_id:016X} stale: no report in {}s (max_int={max_int}s); re-establishing",
                (now - slot.last_report.get()).as_secs()
            );
            on_state(SubState::Offline);
            return Ok(());
        }

        // A narrowing coming due wakes us too.
        let wake = scope
            .narrow_at
            .get()
            .map_or(deadline, |at| at.min(deadline));
        match select4(
            slot.received.wait(),
            Timer::at(wake),
            scope.changed.wait(),
            retry.wait(),
        )
        .await
        {
            Either4::Third(()) => debug!("subscription 0x{node_id:016X}: scope changed"),
            Either4::Fourth(()) => {
                info!("subscription 0x{node_id:016X}: the device lost it; re-establishing");
                return Ok(());
            }
            _ => {}
        }

        if scope.settle() || scope.current() != coverage {
            info!(
                "subscription 0x{node_id:016X}: covering {:?} from now on",
                scope.current()
            );
            return Ok(());
        }
    }
}

/// Establish the subscription: send `SubscribeRequest`, feed the priming
/// `ReportData` chunks to `on_update` as the first [`DeviceHealth`], and await
/// the `SubscribeResponse`. Returns the subscription ID and `max_int`.
#[allow(clippy::too_many_arguments)]
async fn establish<C, FU, FM>(
    matter: &Matter<'_>,
    crypto: C,
    fab_idx: NonZeroU8,
    node_id: u64,
    which: DiagSet,
    coverage: Coverage,
    paths: &[AttrPath],
    events: &[EventPath],
    next_event: u64,
    on_update: &mut FU,
    on_model: &mut FM,
) -> Result<(u32, u16), Error>
where
    C: Crypto,
    FU: FnMut(DeviceHealth),
    FM: FnMut(ModelUpdate, bool),
{
    let min_int_floor = if coverage == Coverage::Health {
        MIN_INTERVAL_FLOOR_SECS
    } else {
        MODEL_MIN_INTERVAL_FLOOR_SECS
    };

    let exchange = Exchange::initiate(matter, crypto, fab_idx, node_id).await?;
    let mut sender = exchange.subscribe_sender().await?;

    let mut chunk = loop {
        match sender.tx().await? {
            TxOutcome::BuildRequest(builder) => {
                // Our one subscription to a device replaces any earlier one,
                // which the device may still hold after we gave it up.
                let builder = builder
                    .keep_subs(false)?
                    .min_int_floor(min_int_floor)?
                    .max_int_ceil(MAX_INTERVAL_CEIL_SECS)?
                    .attr_requests_from(paths)?;
                sender = if events.is_empty() {
                    builder.fabric_filtered(false)?.end()?
                } else {
                    builder
                        .event_requests_from(events)?
                        .event_filters_from(&[EventFilter {
                            node: None,
                            event_min: Some(next_event),
                        }])?
                        .fabric_filtered(false)?
                        .end()?
                };
            }
            TxOutcome::GotResponse(c) => break c,
        }
    };

    // Each priming chunk carries a slice of the current attribute values,
    // and of the events since `next_event`.
    let mut primed = ModelUpdate::default();
    let established = loop {
        {
            let resp = chunk.response()?;
            on_update(parse_health(&resp, which));
            if coverage != Coverage::Health {
                primed.extend(&resp);
            }
        }
        match chunk.complete().await? {
            SubscribeOutcome::NextChunk(next) => chunk = next,
            SubscribeOutcome::Established(est) => break est,
        }
    };

    // What the priming reported replaces what was known.
    on_model(primed, true);

    info!(
        "subscription established: sub_id={} max_int={}s {coverage:?} (peer 0x{node_id:016X})",
        established.subscription_id, established.max_int
    );

    Ok((established.subscription_id, established.max_int))
}

/// Build the subscription's `AttrPath` list: one **cluster-wildcard** path per
/// selected diagnostic cluster (endpoint 0, `attr = None`), and the fabrics.
/// Cluster granularity (rather than per-attribute) is what lets the
/// subscription survive a device that implements only a subset of the
/// attributes — see [`health_cluster_ids`](crate::diagnostics::health_cluster_ids).
/// A device need take no more than 3 paths.
fn attr_paths(which: DiagSet) -> Vec<AttrPath> {
    health_cluster_ids(which)
        .into_iter()
        .map(|cluster| {
            AttrPath::from_gp(&GenericPath::new(Some(ROOT_ENDPOINT), Some(cluster), None))
        })
        .chain(fabric_paths(which))
        .collect()
}

/// The paths a subscription of `coverage` takes: with a single application
/// endpoint (`endpoints`), that endpoint and the health's diagnostics and
/// fabrics, as many paths as any device must take; else every attribute.
fn paths_of(which: DiagSet, coverage: Coverage, endpoints: &[u16]) -> Vec<AttrPath> {
    let every = || alloc::vec![AttrPath::from_gp(&GenericPath::new(None, None, None))];

    match (coverage, endpoints) {
        (Coverage::Health, _) => attr_paths(which),
        (Coverage::Application, [endpoint]) => {
            let paths: Vec<AttrPath> =
                core::iter::once(GenericPath::new(Some(*endpoint), None, None))
                    .chain(
                        health_cluster_ids(which).into_iter().map(|cluster| {
                            GenericPath::new(Some(ROOT_ENDPOINT), Some(cluster), None)
                        }),
                    )
                    .chain(core::iter::once(GenericPath::new(
                        Some(ROOT_ENDPOINT),
                        Some(OPERATIONAL_CREDENTIALS),
                        Some(FABRICS),
                    )))
                    .map(|path| AttrPath::from_gp(&path))
                    .collect();
            if paths.len() <= 3 {
                paths
            } else {
                every()
            }
        }
        _ => every(),
    }
}

/// The events a subscription of `coverage` follows: those of the one
/// application endpoint, or all.
fn event_paths_of(coverage: Coverage, endpoints: &[u16]) -> Vec<EventPath> {
    match (coverage, endpoints) {
        (Coverage::Health, _) => Vec::new(),
        (Coverage::Application, [endpoint]) => {
            alloc::vec![datamodel::event_path(Some(*endpoint), None)]
        }
        _ => alloc::vec![datamodel::event_path(None, None)],
    }
}

/// The endpoints of a device but its root: its Descriptor's parts.
async fn app_endpoints<C: Crypto>(
    matter: &Matter<'_>,
    crypto: C,
    dev: DeviceHandle,
) -> Result<Vec<u16>, Error> {
    let reports = datamodel::read(
        matter,
        crypto,
        dev,
        Some(ROOT_ENDPOINT),
        Some(DESCRIPTOR),
        Some(PARTS_LIST),
    )
    .await?;

    Ok(reports
        .iter()
        .filter_map(|r| r.value.as_ref().ok()?.as_array())
        .flatten()
        .filter_map(|e| e.as_u64().map(|e| e as u16))
        .collect())
}

/// The attributes of `report`, decoded.
fn decoded(report: &ReportDataResp<'_>) -> Vec<Decoded> {
    report
        .attr_reports
        .iter()
        .flat_map(|reports| reports.iter())
        .filter_map(Result::ok)
        .filter_map(datamodel::decode)
        .collect()
}
