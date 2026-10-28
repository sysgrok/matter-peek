//! Reading the standard Matter diagnostic clusters over a CASE session.
//!
//! Diagnostics live on endpoint 0 (the root node). We read each cluster in a
//! **single** IM round-trip: one `read_with` request asks for all the
//! attributes we want from that cluster at once, and the single
//! `ReportDataResp` is demuxed with the generated `*_read_resp()` view (one
//! borrowing iterator per attribute).
//!
//! Readers are **resilient and vendor-agnostic**: a device need not implement
//! every cluster/attribute, so a missing attribute (an attribute-status error
//! in its iterator) leaves that field `None` rather than aborting the snapshot.
//!
//! Covered here: General (`0x0033`) and Software (`0x0034`) diagnostics, and
//! what a health check judges (with the network diagnostics, see
//! [`crate::network`]).

use core::num::NonZeroU8;

use rs_matter::crypto::Crypto;
use rs_matter::dm::clusters::decl::ethernet_network_diagnostics as eth_diag;
use rs_matter::dm::clusters::decl::general_diagnostics::{
    self as gen_diag, BootReasonEnum, GeneralDiagnosticsAttrReads as _,
    GeneralDiagnosticsAttrResponses as _,
};
use rs_matter::dm::clusters::decl::operational_credentials::{
    self as op_creds, FabricDescriptorStruct, OperationalCredentialsAttrResponses as _,
};
use rs_matter::dm::clusters::decl::software_diagnostics::{
    self as sw_diag, SoftwareDiagnosticsAttrResponses as _, ThreadMetricsStruct,
};
use rs_matter::dm::clusters::decl::thread_network_diagnostics as thread_diag;
use rs_matter::dm::clusters::decl::wi_fi_network_diagnostics as wifi_diag;
use rs_matter::dm::EndptId;
use rs_matter::error::Error;
use rs_matter::im::client::ImClient as _;
use rs_matter::im::encoding::{AttrResp, EventResp, ReportDataResp};
use rs_matter::im::{AttrPath, EventPath, GenericPath};
use rs_matter::tlv::{FromTLV, TLVArray};
use rs_matter::transport::exchange::Exchange;
use rs_matter::Matter;

use crate::api::{
    ActiveFaults, BootReason, DeviceFabric, HardwareFault, NetworkFault, RadioFault, ThreadMetric,
};
use crate::health::HealthRead;
use crate::model::{DeviceHealth, DiagSet, GeneralDiag, SoftwareDiag};

extern crate alloc;

/// The root node endpoint, where the diagnostic clusters live.
pub(crate) const ROOT_ENDPOINT: EndptId = 0;

/// Software Diagnostics' `SoftwareFault` event.
const SOFTWARE_FAULT_EVENT: u32 = 0;

/// The diagnostic **cluster IDs** a health snapshot covers, selected by `which`
/// (endpoint is always [`ROOT_ENDPOINT`]).
///
/// The live subscription subscribes at **cluster granularity** (a wildcard path
/// per cluster, `attr = None`) rather than per attribute. This is deliberate and
/// necessary for a vendor-agnostic tool: unlike a *read* (which returns a
/// per-attribute `UnsupportedAttribute` status and carries on), a *subscribe*
/// with a concrete path to an attribute the device doesn't implement is rejected
/// **wholesale** with `InvalidAction` (rs-matter's `validate_subscribe`, per
/// spec). A cluster wildcard instead validates as "does the cluster expose *any*
/// accessible attribute", so a device that implements only a subset still
/// accepts the subscription; [`parse_health`] then picks out the attributes we
/// care about and ignores the rest.
pub(crate) fn health_cluster_ids(which: DiagSet) -> alloc::vec::Vec<u32> {
    let mut clusters = alloc::vec::Vec::new();
    if which.general {
        clusters.push(gen_diag::FULL_CLUSTER.id);
    }
    if which.software {
        clusters.push(sw_diag::FULL_CLUSTER.id);
    }
    clusters
}

/// The concrete paths of the fabrics a health snapshot covers, if `which`
/// does: mandatory attributes, so a device rejects no subscription for them.
pub(crate) fn fabric_paths(which: DiagSet) -> alloc::vec::Vec<AttrPath> {
    if !which.fabrics {
        return alloc::vec::Vec::new();
    }

    [
        op_creds::AttributeId::Fabrics,
        op_creds::AttributeId::SupportedFabrics,
    ]
    .into_iter()
    .map(|attr| {
        AttrPath::from_gp(&GenericPath::new(
            Some(ROOT_ENDPOINT),
            Some(op_creds::FULL_CLUSTER.id),
            Some(attr as _),
        ))
    })
    .collect()
}

/// Demux a `ReportData` response into a [`DeviceHealth`] snapshot, filling only
/// the clusters selected by `which`. Works on any [`ReportDataResp`] — the
/// initial priming report of a subscription, a server-initiated update report,
/// or a one-shot read response — because the generated `*_read_resp()` views
/// are implemented on `ReportDataResp` itself.
///
/// Only fields actually present in this report are set; absent attributes stay
/// `None`. (A subscription update may carry a *subset* of the subscribed
/// attributes — only the ones that changed — so callers merge successive
/// snapshots rather than replacing.)
pub(crate) fn parse_health(resp: &ReportDataResp<'_>, which: DiagSet) -> DeviceHealth {
    let mut health = DeviceHealth::default();
    if which.general {
        general(resp, &mut health);
    }
    if which.software {
        let reads = resp.software_diagnostics_read_resp();
        let s = SoftwareDiag {
            current_heap_free: first(reads.current_heap_free()),
            current_heap_used: first(reads.current_heap_used()),
            current_heap_high_watermark: first(reads.current_heap_high_watermark()),
            thread_metrics: None,
        };
        if s != SoftwareDiag::default() {
            health.software = Some(s);
        }
    }
    if which.fabrics {
        let (cluster, attr) = (
            op_creds::FULL_CLUSTER.id,
            op_creds::AttributeId::Fabrics as u32,
        );
        let mut fabrics = alloc::vec::Vec::new();
        if list_items(resp, cluster, attr, &mut fabrics, fabric) {
            health.fabrics_appended = !has_whole_list(resp, cluster, attr);
            health.fabrics = Some(fabrics);
        }
        health.supported_fabrics =
            first(resp.operational_credentials_read_resp().supported_fabrics());
    }
    health
}

/// Read the requested diagnostic clusters from the device identified by
/// `(fab_idx, node_id)` and assemble a [`DeviceHealth`] snapshot.
///
/// Fails if the device cannot be reached; attributes it does not support are
/// left `None`. `crypto` is consumed per cluster read, so it must be `Copy`
/// (the rs-matter crypto backends are).
pub async fn read_health<C: Crypto + Copy>(
    matter: &Matter<'_>,
    crypto: C,
    fab_idx: NonZeroU8,
    node_id: u64,
    which: DiagSet,
) -> Result<DeviceHealth, Error> {
    let mut health = DeviceHealth::default();

    if which.general {
        read_general(matter, crypto, fab_idx, node_id, &mut health).await?;
    }
    if which.software {
        health.software = Some(read_software(matter, crypto, fab_idx, node_id).await?);
    }
    if which.fabrics {
        health.merge(read_fabrics(matter, crypto, fab_idx, node_id).await?);
    }

    Ok(health)
}

/// Read the fabrics the device is on; every one, not only ours.
pub(crate) async fn read_fabrics<C: Crypto>(
    matter: &Matter<'_>,
    crypto: C,
    fab_idx: NonZeroU8,
    node_id: u64,
) -> Result<DeviceHealth, Error> {
    let only = DiagSet {
        general: false,
        software: false,
        thread: false,
        wifi: false,
        ethernet: false,
        fabrics: true,
    };
    let paths = fabric_paths(only);

    let exchange = Exchange::initiate(matter, crypto, fab_idx, node_id).await?;
    let mut chunk = exchange
        .read_with(|msg| {
            msg.attr_requests_from(&paths)?
                .fabric_filtered(false)?
                .end()
        })
        .await?;

    let mut health = DeviceHealth::default();
    loop {
        health.merge(parse_health(&chunk.response()?, only));

        match chunk.complete().await? {
            Some(next) => chunk = next,
            None => return Ok(health),
        }
    }
}

/// What a health check judges, in one read: General and Software
/// Diagnostics whole, the diagnostics of each network the device has, and the
/// software faults it reported since it started.
pub(crate) async fn read_check<C: Crypto>(
    matter: &Matter<'_>,
    crypto: C,
    fab_idx: NonZeroU8,
    node_id: u64,
) -> Result<HealthRead, Error> {
    let attrs = [
        cluster_path(Some(ROOT_ENDPOINT), gen_diag::FULL_CLUSTER.id),
        cluster_path(Some(ROOT_ENDPOINT), sw_diag::FULL_CLUSTER.id),
        cluster_path(None, wifi_diag::FULL_CLUSTER.id),
        cluster_path(None, thread_diag::FULL_CLUSTER.id),
        cluster_path(None, eth_diag::FULL_CLUSTER.id),
    ];
    let events = [EventPath {
        endpoint: Some(ROOT_ENDPOINT),
        cluster: Some(sw_diag::FULL_CLUSTER.id),
        event: Some(SOFTWARE_FAULT_EVENT),
        ..Default::default()
    }];

    let exchange = Exchange::initiate(matter, crypto, fab_idx, node_id).await?;
    let mut chunk = exchange
        .read_with(|msg| {
            msg.attr_requests_from(&attrs)?
                .event_requests_from(&events)?
                .fabric_filtered(true)?
                .end()
        })
        .await?;

    let mut read = HealthRead::default();
    let mut software = SoftwareDiag::default();
    loop {
        {
            let resp = chunk.response()?;
            general(&resp, &mut read.health);
            software_diag(&resp, &mut software);
            crate::network::interfaces(&resp, &mut read.network);
            crate::network::wifi(&resp, &mut read.network);
            crate::network::thread(&resp, &mut read.network);
            crate::network::ethernet(&resp, &mut read.network);
            software_faults(&resp, &mut read.software_faults);
        }

        match chunk.complete().await? {
            Some(next) => chunk = next,
            None => break,
        }
    }
    read.health.software = Some(software);

    Ok(read)
}

/// Every attribute of `cluster`, on `endpoint` (any if `None`).
fn cluster_path(endpoint: Option<EndptId>, cluster: u32) -> AttrPath {
    AttrPath::from_gp(&GenericPath::new(endpoint, Some(cluster), None))
}

/// The software faults in `resp`, by name, else by ID, onto `into`.
fn software_faults(resp: &ReportDataResp<'_>, into: &mut alloc::vec::Vec<alloc::string::String>) {
    let events = resp
        .event_reports
        .iter()
        .flat_map(|reports| reports.iter())
        .filter_map(Result::ok);

    for event in events {
        let EventResp::Data(data) = event else {
            continue;
        };
        if data.path.cluster != Some(sw_diag::FULL_CLUSTER.id)
            || data.path.event != Some(SOFTWARE_FAULT_EVENT)
        {
            continue;
        }

        let Ok(fields) = data.data.structure() else {
            continue;
        };
        let name = fields
            .find_ctx(1)
            .and_then(|name| name.utf8())
            .ok()
            .filter(|name| !name.is_empty())
            .map(alloc::string::ToString::to_string);
        let id = fields.find_ctx(0).and_then(|id| id.u64()).ok();
        into.extend(name.or_else(|| id.map(|id| alloc::format!("#{id}"))));
    }
}

fn boot_reason(reason: BootReasonEnum) -> BootReason {
    match reason {
        BootReasonEnum::Unspecified => BootReason::Unspecified,
        BootReasonEnum::PowerOnReboot => BootReason::PowerOnReboot,
        BootReasonEnum::BrownOutReset => BootReason::BrownOutReset,
        BootReasonEnum::SoftwareWatchdogReset => BootReason::SoftwareWatchdogReset,
        BootReasonEnum::HardwareWatchdogReset => BootReason::HardwareWatchdogReset,
        BootReasonEnum::SoftwareUpdateCompleted => BootReason::SoftwareUpdateCompleted,
        BootReasonEnum::SoftwareReset => BootReason::SoftwareReset,
    }
}

fn hardware_fault(code: u8) -> HardwareFault {
    match code {
        1 => HardwareFault::Radio,
        2 => HardwareFault::Sensor,
        3 => HardwareFault::ResettableOverTemp,
        4 => HardwareFault::NonResettableOverTemp,
        5 => HardwareFault::PowerSource,
        6 => HardwareFault::VisualDisplay,
        7 => HardwareFault::AudioOutput,
        8 => HardwareFault::UserInterface,
        9 => HardwareFault::NonVolatileMemory,
        10 => HardwareFault::TamperDetected,
        _ => HardwareFault::Unspecified,
    }
}

fn radio_fault(code: u8) -> RadioFault {
    match code {
        1 => RadioFault::WiFi,
        2 => RadioFault::Cellular,
        3 => RadioFault::Thread,
        4 => RadioFault::Nfc,
        5 => RadioFault::Ble,
        6 => RadioFault::Ethernet,
        _ => RadioFault::Unspecified,
    }
}

fn network_fault(code: u8) -> NetworkFault {
    match code {
        1 => NetworkFault::HardwareFailure,
        2 => NetworkFault::NetworkJammed,
        3 => NetworkFault::ConnectionFailed,
        _ => NetworkFault::Unspecified,
    }
}

/// First report from a scalar-attribute iterator. `None` on absence/error
/// (an unsupported attribute surfaces as the iterator's `Err` arm).
fn first<T, I>(mut iter: I) -> Option<T>
where
    I: Iterator<Item = (EndptId, Result<T, Error>)>,
{
    match iter.next() {
        Some((_, Ok(v))) => Some(v),
        _ => None,
    }
}

/// What General Diagnostics tells of the device's health, onto `health`.
async fn read_general<C: Crypto>(
    matter: &Matter<'_>,
    crypto: C,
    fab_idx: NonZeroU8,
    node_id: u64,
    health: &mut DeviceHealth,
) -> Result<(), Error> {
    let exchange = Exchange::initiate(matter, crypto, fab_idx, node_id).await?;

    let mut chunk = exchange
        .read_with(|msg| {
            msg.attr_requests()?
                .general_diagnostics_read()
                .up_time(ROOT_ENDPOINT)?
                .reboot_count(ROOT_ENDPOINT)?
                .total_operational_hours(ROOT_ENDPOINT)?
                .boot_reason(ROOT_ENDPOINT)?
                .active_hardware_faults(ROOT_ENDPOINT)?
                .active_radio_faults(ROOT_ENDPOINT)?
                .active_network_faults(ROOT_ENDPOINT)?
                .end()?
                .fabric_filtered(true)?
                .end()
        })
        .await?;

    loop {
        general(&chunk.response()?, health);

        match chunk.complete().await? {
            Some(next) => chunk = next,
            None => break,
        }
    }

    health.general.get_or_insert_with(Default::default);

    Ok(())
}

/// What General Diagnostics tells of the device's health in `resp`, merged
/// into `health`.
fn general(resp: &ReportDataResp<'_>, health: &mut DeviceHealth) {
    let reads = resp.general_diagnostics_read_resp();
    let g = GeneralDiag {
        up_time_secs: first(reads.up_time()),
        reboot_count: first(reads.reboot_count()),
        total_operational_hours: first(reads.total_operational_hours()),
        boot_reason: first(reads.boot_reason()).map(boot_reason),
    };

    let cluster = gen_diag::FULL_CLUSTER.id;
    let mut faults = ActiveFaults::default();
    let list = |attr: gen_diag::AttributeId| {
        let mut items = alloc::vec::Vec::new();
        list_items(resp, cluster, attr as _, &mut items, |fault: &u8| {
            Some(*fault)
        })
        .then_some(items)
    };
    faults.hardware = list(gen_diag::AttributeId::ActiveHardwareFaults)
        .map(|codes| codes.into_iter().map(hardware_fault).collect());
    faults.radio = list(gen_diag::AttributeId::ActiveRadioFaults)
        .map(|codes| codes.into_iter().map(radio_fault).collect());
    faults.network = list(gen_diag::AttributeId::ActiveNetworkFaults)
        .map(|codes| codes.into_iter().map(network_fault).collect());

    let mut delta = DeviceHealth::default();
    if g != GeneralDiag::default() {
        delta.general = Some(g);
    }
    if faults != ActiveFaults::default() {
        delta.faults = Some(faults);
    }
    health.merge(delta);
}

/// The whole of Software Diagnostics, so that attributes a device does not
/// have are left out rather than refused; with its threads' stacks, which a
/// device sends item by item where the list does not fit a chunk.
async fn read_software<C: Crypto>(
    matter: &Matter<'_>,
    crypto: C,
    fab_idx: NonZeroU8,
    node_id: u64,
) -> Result<SoftwareDiag, Error> {
    let paths = [AttrPath::from_gp(&GenericPath::new(
        Some(ROOT_ENDPOINT),
        Some(sw_diag::FULL_CLUSTER.id),
        None,
    ))];

    let exchange = Exchange::initiate(matter, crypto, fab_idx, node_id).await?;
    let mut chunk = exchange
        .read_with(|msg| msg.attr_requests_from(&paths)?.fabric_filtered(true)?.end())
        .await?;

    let mut diag = SoftwareDiag::default();
    loop {
        software_diag(&chunk.response()?, &mut diag);

        match chunk.complete().await? {
            Some(next) => chunk = next,
            None => break,
        }
    }

    Ok(diag)
}

/// The Software Diagnostics in `resp`, onto `diag`.
fn software_diag(resp: &ReportDataResp<'_>, diag: &mut SoftwareDiag) {
    let reads = resp.software_diagnostics_read_resp();
    diag.current_heap_free = first(reads.current_heap_free()).or(diag.current_heap_free);
    diag.current_heap_used = first(reads.current_heap_used()).or(diag.current_heap_used);
    diag.current_heap_high_watermark =
        first(reads.current_heap_high_watermark()).or(diag.current_heap_high_watermark);
    thread_metrics(resp, &mut diag.thread_metrics);
}

/// The ThreadMetrics in `resp`, onto `into`.
fn thread_metrics(resp: &ReportDataResp<'_>, into: &mut Option<alloc::vec::Vec<ThreadMetric>>) {
    let had = into.is_some();
    let mut list = into.take().unwrap_or_default();

    let seen = list_items(
        resp,
        sw_diag::FULL_CLUSTER.id,
        sw_diag::AttributeId::ThreadMetrics as _,
        &mut list,
        thread_metric,
    );

    *into = (had || seen).then_some(list);
}

/// The items of list attribute `attr` of `cluster` in `resp`, onto `into`: a
/// whole list replaces it, an item is appended to it (a device sends a list
/// item by item where it does not fit a chunk). Whether `resp` has any.
pub(crate) fn list_items<'a, T, U>(
    resp: &ReportDataResp<'a>,
    cluster: u32,
    attr: u32,
    into: &mut alloc::vec::Vec<U>,
    map: impl Fn(&T) -> Option<U>,
) -> bool
where
    T: FromTLV<'a> + 'a,
{
    let reports = resp
        .attr_reports
        .iter()
        .flat_map(|reports| reports.iter())
        .filter_map(Result::ok);

    let mut seen = false;
    for report in reports {
        let AttrResp::Data(data) = report else {
            continue;
        };
        if data.path.cluster != Some(cluster) || data.path.attr != Some(attr) {
            continue;
        }
        seen = true;

        if data.path.list_index.is_some() {
            if let Ok(item) = T::from_tlv(&data.data) {
                into.extend(map(&item));
            }
        } else if let Ok(list) = TLVArray::<T>::from_tlv(&data.data) {
            *into = list.iter().filter_map(|item| map(&item.ok()?)).collect();
        }
    }

    seen
}

/// Whether `resp` has list attribute `attr` of `cluster` whole, rather than
/// item by item.
fn has_whole_list(resp: &ReportDataResp<'_>, cluster: u32, attr: u32) -> bool {
    resp.attr_reports
        .iter()
        .flat_map(|reports| reports.iter())
        .filter_map(Result::ok)
        .any(|report| {
            matches!(report, AttrResp::Data(data)
                if data.path.cluster == Some(cluster)
                    && data.path.attr == Some(attr)
                    && data.path.list_index.is_none())
        })
}

fn fabric(item: &FabricDescriptorStruct<'_>) -> Option<DeviceFabric> {
    Some(DeviceFabric {
        index: item.fabric_index().ok().flatten(),
        vendor_id: item.vendor_id().ok()?,
        label: item
            .label()
            .map(|label| alloc::string::ToString::to_string(&label))
            .unwrap_or_default(),
        fabric_id: item.fabric_id().ok()?,
        node_id: item.node_id().ok()?,
        root_key: item
            .root_public_key()
            .map(|key| key.iter().map(|b| alloc::format!("{b:02X}")).collect())
            .unwrap_or_default(),
        ours: false,
        our_name: None,
    })
}

fn thread_metric(item: &ThreadMetricsStruct<'_>) -> Option<ThreadMetric> {
    Some(ThreadMetric {
        id: item.id().ok()?,
        name: item
            .name()
            .ok()
            .flatten()
            .map(|name| alloc::string::ToString::to_string(&name)),
        stack_size: item.stack_size().ok().flatten(),
        stack_free_current: item.stack_free_current().ok().flatten(),
        stack_free_minimum: item.stack_free_minimum().ok().flatten(),
    })
}

#[cfg(test)]
mod tests {
    use rs_matter::tlv::TLVElement;

    use super::*;

    #[test]
    fn thread_metric_with_every_field() {
        // id 7, name "main", stack free now 1024, least free 512, size 4096.
        let tlv = [
            0x15, 0x24, 0x00, 0x07, 0x2C, 0x01, 0x04, b'm', b'a', b'i', b'n', 0x25, 0x02, 0x00,
            0x04, 0x25, 0x03, 0x00, 0x02, 0x25, 0x04, 0x00, 0x10, 0x18,
        ];
        let item = ThreadMetricsStruct::from_tlv(&TLVElement::new(&tlv)).unwrap();

        let metric = thread_metric(&item).unwrap();
        assert_eq!(metric.id, 7);
        assert_eq!(metric.name.as_deref(), Some("main"));
        assert_eq!(metric.stack_free_current, Some(1024));
        assert_eq!(metric.stack_free_minimum, Some(512));
        assert_eq!(metric.stack_size, Some(4096));
        assert_eq!(metric.stack_peak(), Some(3584));

        // Only the id and the name, as some devices send them.
        let tlv = [
            0x15, 0x24, 0x00, 0x09, 0x2C, 0x01, 0x03, b'I', b'D', b'L', 0x18,
        ];
        let item = ThreadMetricsStruct::from_tlv(&TLVElement::new(&tlv)).unwrap();
        let metric = thread_metric(&item).unwrap();
        assert_eq!(
            (metric.id, metric.stack_size, metric.stack_peak()),
            (9, None, None)
        );
    }
}
