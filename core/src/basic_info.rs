//! Reading a device's identity from the BasicInformation cluster (`0x0028`)
//! in a **single** IM read round-trip: the stable string identifiers
//! `UniqueID` / `SerialNumber` / `NodeLabel`, plus a fingerprint from the
//! mandatory numeric attributes as a fallback.
//!
//! One `read_with` request asks for every attribute we care about at once;
//! the single `ReportDataResp` is then demuxed with the generated
//! `.basic_information_read_resp()` view, which (since the rs-matter codegen
//! emits view methods for *all* attribute types, strings included) hands back
//! a borrowing iterator per attribute. We copy the borrowed `&str`s out before
//! the response chunk is dropped.
//!
//! All reads are best-effort: an optional attribute the device doesn't serve
//! comes back as an attribute-status error in that attribute's iterator, which
//! we map to an empty field / `None`.

extern crate alloc;
use alloc::string::{String, ToString};

use core::num::NonZeroU8;

use log::debug;

use rs_matter::crypto::Crypto;
use rs_matter::dm::clusters::decl::basic_information::{
    BasicInformationAttrReads as _, BasicInformationAttrResponses as _,
};
use rs_matter::dm::EndptId;
use rs_matter::error::Error;
use rs_matter::im::client::ImClient as _;
use rs_matter::transport::exchange::Exchange;
use rs_matter::Matter;

use crate::model::{DeviceFingerprint, DeviceIdentity};

/// BasicInformation lives on the root node endpoint.
const ROOT_ENDPOINT: EndptId = 0;

/// Read the device's identity in one round-trip: the stable string identifiers
/// when served, plus the always-available numeric fingerprint.
///
/// Never fails outright — a failed read (or an unsupported optional attribute)
/// leaves the corresponding field empty / `None`.
pub async fn read_basic_info<C: Crypto>(
    matter: &Matter<'_>,
    crypto: C,
    fab_idx: NonZeroU8,
    node_id: u64,
) -> DeviceIdentity {
    match try_read_basic_info(matter, crypto, fab_idx, node_id).await {
        Ok(identity) => identity,
        Err(e) => {
            debug!("BasicInformation read failed: {e:?}");
            DeviceIdentity::default()
        }
    }
}

async fn try_read_basic_info<C: Crypto>(
    matter: &Matter<'_>,
    crypto: C,
    fab_idx: NonZeroU8,
    node_id: u64,
) -> Result<DeviceIdentity, Error> {
    let exchange = Exchange::initiate(matter, crypto, fab_idx, node_id).await?;

    // One request for every attribute we want. The builder chains, so all
    // seven paths go into a single read.
    let mut chunk = exchange
        .read_with(|msg| {
            msg.attr_requests()?
                .basic_information_read()
                .unique_id(ROOT_ENDPOINT)?
                .serial_number(ROOT_ENDPOINT)?
                .node_label(ROOT_ENDPOINT)?
                .vendor_id(ROOT_ENDPOINT)?
                .product_id(ROOT_ENDPOINT)?
                .hardware_version(ROOT_ENDPOINT)?
                .software_version(ROOT_ENDPOINT)?
                .end()?
                .fabric_filtered(true)?
                .end()
        })
        .await?;

    // Demux the single response. Each accessor yields `(endpoint, Result<T>)`;
    // we take the first matching report (root endpoint), mapping any
    // attribute-status error (e.g. unsupported optional) to empty / `None`.
    let identity = {
        let resp = chunk.response()?;
        let reads = resp.basic_information_read_resp();

        DeviceIdentity {
            unique_id: first_string(reads.unique_id()),
            serial_number: first_string(reads.serial_number()),
            node_label: first_string(reads.node_label()),
            fingerprint: DeviceFingerprint {
                vendor_id: first_scalar(reads.vendor_id()),
                product_id: first_scalar(reads.product_id()),
                hardware_version: first_scalar(reads.hardware_version()),
                software_version: first_scalar(reads.software_version()),
            },
        }
    };

    // Drain any remaining chunks (sends the trailing StatusResponse).
    while let Some(next) = chunk.complete().await? {
        chunk = next;
    }

    Ok(identity)
}

/// First report from a string-attribute iterator, owned. Empty on
/// absence/error (e.g. an unsupported optional attribute).
fn first_string<'a, I>(mut iter: I) -> String
where
    I: Iterator<Item = (EndptId, Result<rs_matter::tlv::Utf8Str<'a>, Error>)>,
{
    match iter.next() {
        Some((_, Ok(s))) => s.to_string(),
        _ => String::new(),
    }
}

/// First report from a scalar-attribute iterator. `None` on absence/error.
fn first_scalar<T, I>(mut iter: I) -> Option<T>
where
    I: Iterator<Item = (EndptId, Result<T, Error>)>,
{
    match iter.next() {
        Some((_, Ok(v))) => Some(v),
        _ => None,
    }
}
