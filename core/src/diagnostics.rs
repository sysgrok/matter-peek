//! Reading the standard Matter diagnostic clusters over a CASE session.
//!
//! Diagnostics live on endpoint 0 (the root node). Each single-shot client read
//! consumes one [`Exchange`] (one IM transaction), so we open a fresh
//! CASE-secured exchange per attribute via [`Exchange::initiate`] — the same
//! pattern rs-matter's `commissioning` test uses for post-commission reads.
//!
//! Readers are **resilient and vendor-agnostic**: a device need not implement
//! every cluster/attribute, so a failing individual read leaves that field
//! `None` rather than aborting the whole snapshot.
//!
//! Covered so far: General (`0x0033`) and Software (`0x0034`) diagnostics.
//! Thread / Wi-Fi / Ethernet land next.

use core::num::NonZeroU8;

use log::debug;

use rs_matter::crypto::Crypto;
use rs_matter::dm::clusters::decl::general_diagnostics::GeneralDiagnosticsClient as _;
use rs_matter::dm::clusters::decl::software_diagnostics::SoftwareDiagnosticsClient as _;
use rs_matter::dm::EndptId;
use rs_matter::error::Error;
use rs_matter::transport::exchange::Exchange;
use rs_matter::Matter;

use crate::model::{DeviceHealth, DiagSet, GeneralDiag, SoftwareDiag};

/// The root node endpoint, where the diagnostic clusters live.
const ROOT_ENDPOINT: EndptId = 0;

/// Read the requested diagnostic clusters from the device identified by
/// `(fab_idx, node_id)` and assemble a [`DeviceHealth`] snapshot.
///
/// `crypto` is consumed per-exchange, so it must be `Copy` (the rs-matter
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

/// Open a fresh CASE exchange to the device. Each diagnostic read needs its own
/// (single-shot reads consume the exchange).
async fn exchange<'a, C: Crypto>(
    matter: &'a Matter<'a>,
    crypto: C,
    fab_idx: NonZeroU8,
    node_id: u64,
) -> Result<Exchange<'a>, Error> {
    Exchange::initiate(matter, crypto, fab_idx, node_id).await
}

/// Read a single attribute, logging and swallowing any error into `None` so one
/// unsupported attribute never sinks the whole snapshot.
///
/// The client reads return `Result<Option<T>, Error>` (the attributes are
/// nullable in the Matter data model), so a `None` here means *either* the
/// attribute read as null *or* the read/exchange failed — both map to "not
/// available", which is exactly what the UI wants.
macro_rules! soft_read {
    // Nullable attribute: the read yields `Result<Option<T>, Error>`; flatten so
    // null and read-failure both collapse to `None`.
    (nullable $what:literal, $matter:expr, $crypto:expr, $fab:expr, $node:expr, |$ex:ident| $body:expr) => {{
        match exchange($matter, $crypto, $fab, $node).await {
            Ok($ex) => match $body.await {
                Ok(v) => v,
                Err(e) => {
                    debug!(concat!("read ", $what, " failed: {:?}"), e);
                    None
                }
            },
            Err(e) => {
                debug!(concat!("exchange for ", $what, " failed: {:?}"), e);
                None
            }
        }
    }};
    // Non-nullable attribute: the read yields `Result<T, Error>`; wrap success in
    // `Some`, map read-failure to `None`.
    (plain $what:literal, $matter:expr, $crypto:expr, $fab:expr, $node:expr, |$ex:ident| $body:expr) => {{
        match exchange($matter, $crypto, $fab, $node).await {
            Ok($ex) => match $body.await {
                Ok(v) => Some(v),
                Err(e) => {
                    debug!(concat!("read ", $what, " failed: {:?}"), e);
                    None
                }
            },
            Err(e) => {
                debug!(concat!("exchange for ", $what, " failed: {:?}"), e);
                None
            }
        }
    }};
}

async fn read_general<C: Crypto + Copy>(
    matter: &Matter<'_>,
    crypto: C,
    fab_idx: NonZeroU8,
    node_id: u64,
) -> GeneralDiag {
    GeneralDiag {
        up_time_secs: soft_read!(nullable "UpTime", matter, crypto, fab_idx, node_id, |ex| ex
            .general_diagnostics()
            .up_time_read(ROOT_ENDPOINT)),
        reboot_count: soft_read!(plain "RebootCount", matter, crypto, fab_idx, node_id, |ex| ex
            .general_diagnostics()
            .reboot_count_read(ROOT_ENDPOINT)),
        total_operational_hours: soft_read!(
            nullable "TotalOperationalHours",
            matter,
            crypto,
            fab_idx,
            node_id,
            |ex| ex
                .general_diagnostics()
                .total_operational_hours_read(ROOT_ENDPOINT)
        ),
    }
}

async fn read_software<C: Crypto + Copy>(
    matter: &Matter<'_>,
    crypto: C,
    fab_idx: NonZeroU8,
    node_id: u64,
) -> SoftwareDiag {
    SoftwareDiag {
        current_heap_free: soft_read!(nullable "CurrentHeapFree", matter, crypto, fab_idx, node_id, |ex| ex
            .software_diagnostics()
            .current_heap_free_read(ROOT_ENDPOINT)),
        current_heap_used: soft_read!(nullable "CurrentHeapUsed", matter, crypto, fab_idx, node_id, |ex| ex
            .software_diagnostics()
            .current_heap_used_read(ROOT_ENDPOINT)),
        current_heap_high_watermark: soft_read!(
            nullable "CurrentHeapHighWatermark",
            matter,
            crypto,
            fab_idx,
            node_id,
            |ex| ex
                .software_diagnostics()
                .current_heap_high_watermark_read(ROOT_ENDPOINT)
        ),
    }
}
