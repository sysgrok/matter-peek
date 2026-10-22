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
//! Covered so far: General (`0x0033`) and Software (`0x0034`) diagnostics.
//! Thread / Wi-Fi / Ethernet land next.

use core::num::NonZeroU8;

use rs_matter::crypto::Crypto;
use rs_matter::dm::clusters::decl::general_diagnostics::{
    GeneralDiagnosticsAttrReads as _, GeneralDiagnosticsAttrResponses as _,
};
use rs_matter::dm::clusters::decl::software_diagnostics::{
    self as sw_diag, SoftwareDiagnosticsAttrResponses as _, ThreadMetricsStruct,
};
use rs_matter::dm::EndptId;
use rs_matter::error::Error;
use rs_matter::im::client::ImClient as _;
use rs_matter::im::encoding::{AttrResp, ReportDataResp};
use rs_matter::im::{AttrPath, GenericPath};
use rs_matter::tlv::{FromTLV, TLVArray};
use rs_matter::transport::exchange::Exchange;
use rs_matter::Matter;

use crate::api::ThreadMetric;
use crate::model::{DeviceHealth, DiagSet, GeneralDiag, SoftwareDiag};

extern crate alloc;

/// The root node endpoint, where the diagnostic clusters live.
pub(crate) const ROOT_ENDPOINT: EndptId = 0;

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
    use rs_matter::dm::clusters::decl::general_diagnostics as gen_diag;
    use rs_matter::dm::clusters::decl::software_diagnostics as sw_diag;

    let mut clusters = alloc::vec::Vec::new();
    if which.general {
        clusters.push(gen_diag::FULL_CLUSTER.id);
    }
    if which.software {
        clusters.push(sw_diag::FULL_CLUSTER.id);
    }
    clusters
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
        let reads = resp.general_diagnostics_read_resp();
        let g = GeneralDiag {
            up_time_secs: first(reads.up_time()),
            reboot_count: first(reads.reboot_count()),
            total_operational_hours: first(reads.total_operational_hours()),
        };
        if g != GeneralDiag::default() {
            health.general = Some(g);
        }
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
        health.general = Some(read_general(matter, crypto, fab_idx, node_id).await?);
    }
    if which.software {
        health.software = Some(read_software(matter, crypto, fab_idx, node_id).await?);
    }

    Ok(health)
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

async fn read_general<C: Crypto>(
    matter: &Matter<'_>,
    crypto: C,
    fab_idx: NonZeroU8,
    node_id: u64,
) -> Result<GeneralDiag, Error> {
    let exchange = Exchange::initiate(matter, crypto, fab_idx, node_id).await?;

    let mut chunk = exchange
        .read_with(|msg| {
            msg.attr_requests()?
                .general_diagnostics_read()
                .up_time(ROOT_ENDPOINT)?
                .reboot_count(ROOT_ENDPOINT)?
                .total_operational_hours(ROOT_ENDPOINT)?
                .end()?
                .fabric_filtered(true)?
                .end()
        })
        .await?;

    let diag = {
        let resp = chunk.response()?;
        let reads = resp.general_diagnostics_read_resp();
        GeneralDiag {
            up_time_secs: first(reads.up_time()),
            reboot_count: first(reads.reboot_count()),
            total_operational_hours: first(reads.total_operational_hours()),
        }
    };

    while let Some(next) = chunk.complete().await? {
        chunk = next;
    }

    Ok(diag)
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
        {
            let resp = chunk.response()?;
            let reads = resp.software_diagnostics_read_resp();
            diag.current_heap_free = first(reads.current_heap_free()).or(diag.current_heap_free);
            diag.current_heap_used = first(reads.current_heap_used()).or(diag.current_heap_used);
            diag.current_heap_high_watermark =
                first(reads.current_heap_high_watermark()).or(diag.current_heap_high_watermark);
            thread_metrics(&resp, &mut diag.thread_metrics);
        }

        match chunk.complete().await? {
            Some(next) => chunk = next,
            None => break,
        }
    }

    Ok(diag)
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
