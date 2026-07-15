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

use log::debug;

use rs_matter::crypto::Crypto;
use rs_matter::dm::clusters::decl::general_diagnostics::{
    GeneralDiagnosticsAttrReads as _, GeneralDiagnosticsAttrResponses as _,
};
use rs_matter::dm::clusters::decl::software_diagnostics::{
    SoftwareDiagnosticsAttrReads as _, SoftwareDiagnosticsAttrResponses as _,
};
use rs_matter::dm::EndptId;
use rs_matter::error::Error;
use rs_matter::im::client::ImClient as _;
use rs_matter::im::encoding::ReportDataResp;
use rs_matter::transport::exchange::Exchange;
use rs_matter::Matter;

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
/// `crypto` is consumed per cluster read, so it must be `Copy` (the rs-matter
/// crypto backends are).
pub async fn read_health<C: Crypto + Copy>(
    matter: &Matter<'_>,
    crypto: C,
    fab_idx: NonZeroU8,
    node_id: u64,
    which: DiagSet,
) -> DeviceHealth {
    let mut health = DeviceHealth::default();

    if which.general {
        health.general = Some(read_general(matter, crypto, fab_idx, node_id).await);
    }
    if which.software {
        health.software = Some(read_software(matter, crypto, fab_idx, node_id).await);
    }

    health
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
) -> GeneralDiag {
    match try_read_general(matter, crypto, fab_idx, node_id).await {
        Ok(diag) => diag,
        Err(e) => {
            debug!("GeneralDiagnostics read failed: {e:?}");
            GeneralDiag::default()
        }
    }
}

async fn try_read_general<C: Crypto>(
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

async fn read_software<C: Crypto>(
    matter: &Matter<'_>,
    crypto: C,
    fab_idx: NonZeroU8,
    node_id: u64,
) -> SoftwareDiag {
    match try_read_software(matter, crypto, fab_idx, node_id).await {
        Ok(diag) => diag,
        Err(e) => {
            debug!("SoftwareDiagnostics read failed: {e:?}");
            SoftwareDiag::default()
        }
    }
}

async fn try_read_software<C: Crypto>(
    matter: &Matter<'_>,
    crypto: C,
    fab_idx: NonZeroU8,
    node_id: u64,
) -> Result<SoftwareDiag, Error> {
    let exchange = Exchange::initiate(matter, crypto, fab_idx, node_id).await?;

    let mut chunk = exchange
        .read_with(|msg| {
            msg.attr_requests()?
                .software_diagnostics_read()
                .current_heap_free(ROOT_ENDPOINT)?
                .current_heap_used(ROOT_ENDPOINT)?
                .current_heap_high_watermark(ROOT_ENDPOINT)?
                .end()?
                .fabric_filtered(true)?
                .end()
        })
        .await?;

    let diag = {
        let resp = chunk.response()?;
        let reads = resp.software_diagnostics_read_resp();
        SoftwareDiag {
            current_heap_free: first(reads.current_heap_free()),
            current_heap_used: first(reads.current_heap_used()),
            current_heap_high_watermark: first(reads.current_heap_high_watermark()),
        }
    };

    while let Some(next) = chunk.complete().await? {
        chunk = next;
    }

    Ok(diag)
}
