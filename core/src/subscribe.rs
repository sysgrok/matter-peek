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
//! ## Concurrency
//!
//! [`subscribe_health`] runs three things concurrently, for the lifetime of the
//! subscription (i.e. until the returned future is dropped):
//! 1. the controller's [`Responder`] (accepts inbound report exchanges),
//! 2. the subscribe-establishment flow (primes the first snapshot), and
//! 3. (implicitly) the caller's transport pump — `matter.run(...)` — which must
//!    already be running in a sibling task, exactly as for BDX log retrieval.

use core::cell::RefCell;
use core::num::NonZeroU8;

use embassy_futures::select::{select, Either};

use log::{debug, info, warn};

use rs_matter::crypto::Crypto;
use rs_matter::dm::clusters::net_comm::DummyNetworks;
use rs_matter::dm::clusters::net_comm::NetworkType;
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
/// (the device must report at least this often, which also serves as a liveness
/// heartbeat — a missed max-interval report means the device or session is gone).
const MIN_INTERVAL_FLOOR_SECS: u16 = 0;
const MAX_INTERVAL_CEIL_SECS: u16 = 60;

/// A [`ReportDataHandler`] that demuxes each incoming `ReportData` into a
/// [`DeviceHealth`] delta and forwards it to the caller's `on_update` closure.
///
/// `which` selects which diagnostic clusters to parse (matching the subscribed
/// attributes). The closure is stored behind a `RefCell` because
/// `handle_report` takes `&self` (the handler is shared, borrowed by the IM),
/// but the callback needs `&mut` to accumulate UI state.
struct HealthReportHandler<F> {
    which: DiagSet,
    on_update: RefCell<F>,
}

impl<F> HealthReportHandler<F>
where
    F: FnMut(DeviceHealth),
{
    fn new(which: DiagSet, on_update: F) -> Self {
        Self {
            which,
            on_update: RefCell::new(on_update),
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

        // Empty deltas (a keep-alive report carrying no attributes we track) are
        // still forwarded — the caller may want to treat any report as a
        // liveness signal. Accepting the report (Ok) is what keeps the
        // subscription alive; rejecting would tear it down.
        (self.on_update.borrow_mut())(health);
        Ok(())
    }
}

/// Subscribe to the device's diagnostic attributes (`which`) and stream updated
/// [`DeviceHealth`] snapshots to `on_update` for the lifetime of the returned
/// future.
///
/// The first invocation of `on_update` carries the **priming** snapshot (the
/// device's current values, delivered during establishment); subsequent
/// invocations carry deltas as attributes change. The caller merges successive
/// snapshots (a delta report may set only the attributes that changed).
///
/// Runs until the future is dropped (cancel the subscription by dropping it), or
/// until the establishment / transport fails. The transport pump
/// (`matter.run(...)`) must already be running in a sibling task.
pub async fn subscribe_health<C, F>(
    matter: &Matter<'_>,
    crypto: C,
    fab_idx: NonZeroU8,
    node_id: u64,
    which: DiagSet,
    on_update: F,
) -> Result<(), Error>
where
    C: Crypto + Copy,
    F: FnMut(DeviceHealth),
{
    // The controller's own Interaction Model: it serves no clusters (empty node)
    // and exists purely to accept the device's inbound `ReportData` exchanges and
    // route them to our report handler.
    let node = Node::new(&[]);
    let handler = (node, EmptyHandler);
    let buffers: MatterBuffers = MatterBuffers::new();
    let state: InteractionModelState<DummyNetworks, 3, 1024> =
        InteractionModelState::new(DummyNetworks);
    let kv = matter.kv(DummyKvBlobStore);

    let reports = HealthReportHandler::new(which, on_update);

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
    // `SecureChannel` handler). A controller *initiates* CASE via the IM client;
    // it must not run a second SecureChannel responder on the shared stack — that
    // would compete with the initiating flow for inbound SC exchanges and corrupt
    // the session on teardown. We only need to accept inbound `ReportData` (IM).
    let responder = Responder::new("matter-peek-reports", &im, matter, 0);

    // Run the report responder concurrently with the establishment flow. The
    // responder must be live *before* establishment completes, because a resumed
    // / eager device can push its first post-priming report immediately.
    match select(
        responder.run::<4>(),
        establish(matter, crypto, fab_idx, node_id, which, &reports),
    )
    .await
    {
        Either::First(r) => {
            // Responder exited — unexpected while we still want reports.
            r?;
            Ok(())
        }
        Either::Second(r) => r,
    }
}

/// Establish the subscription (send `SubscribeRequest`, walk the priming
/// `ReportData` chunks into the first [`DeviceHealth`], await the
/// `SubscribeResponse`), then keep the future alive so the subscription stays up
/// and the responder keeps receiving reports.
///
/// The priming snapshot is delivered through the *same* `reports` handler as
/// live updates, so the caller sees one uniform stream.
async fn establish<C, F>(
    matter: &Matter<'_>,
    crypto: C,
    fab_idx: NonZeroU8,
    node_id: u64,
    which: DiagSet,
    reports: &HealthReportHandler<F>,
) -> Result<(), Error>
where
    C: Crypto,
    F: FnMut(DeviceHealth),
{
    let paths = attr_paths(which);
    if paths.is_empty() {
        warn!("subscribe_health: no attributes selected; nothing to subscribe to");
        return Ok(());
    }

    let exchange = Exchange::initiate(matter, crypto, fab_idx, node_id).await?;
    let mut sender = exchange.subscribe_sender().await?;

    let mut chunk = loop {
        match sender.tx().await? {
            TxOutcome::BuildRequest(builder) => {
                sender = builder
                    .keep_subs(true)?
                    .min_int_floor(MIN_INTERVAL_FLOOR_SECS)?
                    .max_int_ceil(MAX_INTERVAL_CEIL_SECS)?
                    .attr_requests_from(&paths)?
                    .fabric_filtered(false)?
                    .end()?;
            }
            TxOutcome::GotResponse(c) => break c,
        }
    };

    // Walk the priming chunks: each carries a slice of the current attribute
    // values. Feed each through the report handler so the caller's first
    // update(s) are the priming snapshot.
    let established = loop {
        {
            let resp = chunk.response()?;
            let health = parse_health(&resp, which);
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

    // Establishment is done; the subscription now lives as the
    // (fabric, peer, sub_id) triple and reports arrive on the responder. Keep
    // this future pending so the enclosing `select` doesn't complete and tear
    // the subscription down — the caller drops us to cancel.
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
