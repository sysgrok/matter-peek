//! The controller's own node: what devices reach on it.
//!
//! One responder serves all of it: on endpoint 0 the OTA Provider cluster and
//! the Time Synchronization cluster (our clock, which devices take their time
//! from), OTA images over BDX, the reports of our health subscriptions, which
//! [`Reports`] routes to them, the Check-Ins of the devices that sleep, and
//! CASE, for devices that open a session to us (a time sync client does). A single responder matters: several would race
//! for the same inbound exchanges.

use core::num::NonZeroU8;

use alloc::vec::Vec;

use embassy_futures::select::{select, Either};
use embassy_time::{Duration, Timer};

use log::info;

use rs_matter::acl::{AclEntry, AuthMode, Target};
use rs_matter::bdx::{Bdx, PROTO_ID_BDX};
use rs_matter::crypto::Crypto;
use rs_matter::dm::clusters::desc::{self, ClusterHandler as _, DescHandler};
use rs_matter::dm::clusters::ota_prov::{
    self, OtaBdxHandler, OtaImages, OtaImagesRegistry, OtaProviderHandler,
};
use rs_matter::dm::clusters::ota_req;
use rs_matter::dm::clusters::time_sync::{self, GranularityEnum, TimeSyncHandler};
use rs_matter::dm::devices::DEV_TYPE_OTA_PROVIDER;
use rs_matter::dm::endpoints::ROOT_ENDPOINT_ID;
use rs_matter::dm::{
    Async, AttrChangeNotifier, Cluster, Dataver, EmptyHandler, Endpoint, Node, Privilege,
};
use rs_matter::error::Error;
use rs_matter::im::{InteractionModel, InteractionModelState};
use rs_matter::persist::{DummyKvBlobStore, KvBlobStore};
use rs_matter::respond::{ChainedExchangeHandler, Responder};
use rs_matter::sc::{SecureChannel, PROTO_ID_SECURE_CHANNEL};
use rs_matter::transport::exchange::MatterBuffers;
use rs_matter::{clusters, devices, with, Matter};

use crate::icd::CheckInReceiver;
use crate::persist::ControllerStore;
use crate::subscribe::Reports;
use crate::time;

/// The OTA Provider cluster, with its mandatory attributes.
const OTA_PROVIDER_CLUSTER: Cluster<'static> = ota_prov::FULL_CLUSTER.with_attrs(with!(required));
/// The Time Synchronization cluster, with no features: our clock.
const TIME_SYNC_CLUSTER: Cluster<'static> = time_sync::cluster::<0>();

/// How often our clock is checked against the host's.
const CLOCK_CHECK_SECS: u64 = 30;
/// How far our clock may drift from the host's, in microseconds: the host's
/// can jump (set by hand, NTP stepping it, a suspend).
const MAX_CLOCK_DRIFT_US: u64 = 50_000;

/// The node: endpoint 0, an OTA Provider that announces itself to requestors,
/// and a time source.
const NODE: Node<'static> = Node {
    endpoints: &[Endpoint {
        id: ROOT_ENDPOINT_ID,
        device_types: devices!(DEV_TYPE_OTA_PROVIDER),
        clusters: clusters!(
            DescHandler::CLUSTER,
            OTA_PROVIDER_CLUSTER,
            TIME_SYNC_CLUSTER
        ),
        client_clusters: &[ota_req::FULL_CLUSTER.id],
        unique_id: None,
        semantic_tags: &[],
    }],
};

/// Serve the controller's node until an error: the OTA Provider of `ota`, our
/// clock and the subscription `reports`; the Check-Ins go to `check_ins`.
pub async fn run<C, P>(
    matter: &Matter<'_>,
    crypto: C,
    reports: &Reports,
    ota: &P,
    check_ins: &async_channel::Sender<Vec<u8>>,
) -> Result<(), Error>
where
    C: Crypto + Copy,
    P: OtaImagesRegistry + OtaImages,
{
    let mut rand = crypto.weak_rand()?;

    let handler = EmptyHandler
        .chain(
            |e, c| e == ROOT_ENDPOINT_ID && c == DescHandler::CLUSTER.id,
            Async(desc::HandlerAdaptor(DescHandler::new(Dataver::new_rand(
                &mut rand,
            )))),
        )
        .chain(
            |e, c| e == ROOT_ENDPOINT_ID && c == OTA_PROVIDER_CLUSTER.id,
            OtaProviderHandler::new(Dataver::new_rand(&mut rand), ota).adapt(),
        )
        .chain(
            |e, c| e == ROOT_ENDPOINT_ID && c == TIME_SYNC_CLUSTER.id,
            Async(TimeSyncHandler::new(Dataver::new_rand(&mut rand)).adapt()),
        );

    let buffers: MatterBuffers = MatterBuffers::new();
    let state: InteractionModelState<3, 1024> = InteractionModelState::new();
    let kv = matter.kv(DummyKvBlobStore);

    let im = InteractionModel::new_with_reports(
        matter,
        crypto,
        &buffers,
        (NODE, handler),
        &kv,
        reports,
        &state,
    );

    let bdx_buffers: MatterBuffers<2> = MatterBuffers::new();

    let handler = ChainedExchangeHandler::new(
        PROTO_ID_SECURE_CHANNEL,
        SecureChannel::new_with_handler(crypto, &im, CheckInReceiver(check_ins)),
        ChainedExchangeHandler::new(
            PROTO_ID_BDX,
            Bdx::new(OtaBdxHandler::new(&bdx_buffers, ota)),
            &im,
        ),
    );

    let responder = Responder::new("matter-peek", handler, matter, 0);

    match select(responder.run::<4>(), keep_time(matter, &im)).await {
        Either::First(result) => result,
        Either::Second(never) => match never {},
    }
}

/// Keep our clock, which devices read, on the host's: set it when it drifts
/// or the host's accuracy changes.
async fn keep_time(
    matter: &Matter<'_>,
    notify: &dyn AttrChangeNotifier,
) -> core::convert::Infallible {
    let mut served = None;

    loop {
        if let Some(now) = time::host_time() {
            let quality = now.quality();

            matter.with_state(|state| {
                let drift = state
                    .rtc
                    .utc_time()
                    .reliable()
                    .map(|ours| ours.abs_diff(now.utc_us));

                if served != Some(quality) || drift.is_none_or(|drift| drift > MAX_CLOCK_DRIFT_US) {
                    // The clock reports one step below what it is given.
                    let (granularity, source) = quality;
                    state
                        .rtc
                        .set_utc_time(now.utc_us, finer(granularity), source, notify);
                }
            });

            if served != Some(quality) {
                info!("node: serving our clock, {quality:?}");
                served = Some(quality);
            }
        }

        Timer::after(Duration::from_secs(CLOCK_CHECK_SECS)).await;
    }
}

/// One step finer than `granularity`.
fn finer(granularity: GranularityEnum) -> GranularityEnum {
    match granularity {
        GranularityEnum::NoTimeGranularity => GranularityEnum::MinutesGranularity,
        GranularityEnum::MinutesGranularity => GranularityEnum::SecondsGranularity,
        GranularityEnum::SecondsGranularity => GranularityEnum::MillisecondsGranularity,
        _ => GranularityEnum::MicrosecondsGranularity,
    }
}

/// Let the devices on fabric `fab_idx` use our node: Operate on our OTA
/// Provider cluster, View on our Time Synchronization cluster. An ACL entry for
/// any CASE peer each, added (and the fabric persisted) unless there already is
/// one.
pub fn allow_devices<S: KvBlobStore>(
    matter: &Matter<'_>,
    store: &ControllerStore<'_, S>,
    fab_idx: NonZeroU8,
) -> Result<(), Error> {
    let grants = [
        (Privilege::OPERATE, OTA_PROVIDER_CLUSTER.id),
        (Privilege::VIEW, TIME_SYNC_CLUSTER.id),
    ];

    matter.with_state(|state| {
        let fabric = state.fabrics.fabric_mut(fab_idx)?;
        let mut added = false;

        for (privilege, cluster) in grants {
            let target = Target::new(Some(ROOT_ENDPOINT_ID), Some(cluster), None);

            let present = fabric.acl_iter().any(|entry| {
                entry
                    .targets()
                    .into_option()
                    .is_some_and(|targets| targets.contains(&target))
            });

            if !present {
                let mut entry = AclEntry::new(None, privilege, AuthMode::Case);
                entry.add_target(target)?;
                fabric.acl_add(entry)?;
                added = true;
            }
        }

        if added {
            store.save_fabric(fabric)
        } else {
            Ok(())
        }
    })
}
