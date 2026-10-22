//! Reading a device's identity in one round trip: its BasicInformation (the
//! stable string identifiers `UniqueID` / `SerialNumber` / `NodeLabel`, its
//! names and version strings, and a fingerprint from the mandatory numeric
//! attributes as a fallback), and the kind of network it is on.
//!
//! All reads are best-effort: an optional attribute the device doesn't serve
//! comes back as an attribute-status error in that attribute's iterator, which
//! we map to an empty field / `None`.

extern crate alloc;
use alloc::string::{String, ToString};

use core::num::NonZeroU8;

use log::debug;

use rs_matter::crypto::Crypto;
use rs_matter::dm::clusters::decl::basic_information::{self, BasicInformationAttrResponses as _};
use rs_matter::dm::clusters::decl::network_commissioning::{
    self, NetworkCommissioningAttrResponses as _,
};
use rs_matter::dm::EndptId;
use rs_matter::error::Error;
use rs_matter::im::client::ImClient as _;
use rs_matter::im::{AttrPath, GenericPath};
use rs_matter::transport::exchange::Exchange;
use rs_matter::Matter;

use crate::model::DeviceIdentity;

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
    // BasicInformation whole (it is small), and the kind of network of the
    // primary interface.
    let paths = [
        AttrPath::from_gp(&GenericPath::new(
            Some(ROOT_ENDPOINT),
            Some(basic_information::FULL_CLUSTER.id),
            None,
        )),
        AttrPath::from_gp(&GenericPath::new(
            Some(ROOT_ENDPOINT),
            Some(network_commissioning::FULL_CLUSTER.id),
            Some(network_commissioning::AttributeId::FeatureMap as _),
        )),
    ];

    let exchange = Exchange::initiate(matter, crypto, fab_idx, node_id).await?;
    let mut chunk = exchange
        .read_with(|msg| msg.attr_requests_from(&paths)?.fabric_filtered(true)?.end())
        .await?;

    // Each chunk carries some of the attributes; an attribute the device does
    // not serve comes back as a status, read as empty / `None`.
    let mut identity = DeviceIdentity::default();
    loop {
        {
            let resp = chunk.response()?;
            let reads = resp.basic_information_read_resp();
            let fp = &mut identity.fingerprint;

            set_string(&mut identity.unique_id, reads.unique_id());
            set_string(&mut identity.serial_number, reads.serial_number());
            set_string(&mut identity.node_label, reads.node_label());
            // The device's own names: more reliable than looking up its IDs.
            set_string(&mut identity.vendor_name, reads.vendor_name());
            set_string(&mut identity.product_name, reads.product_name());
            set_string(
                &mut identity.hardware_version_string,
                reads.hardware_version_string(),
            );
            set_string(
                &mut identity.software_version_string,
                reads.software_version_string(),
            );
            set(&mut fp.vendor_id, reads.vendor_id());
            set(&mut fp.product_id, reads.product_id());
            set(&mut fp.hardware_version, reads.hardware_version());
            set(&mut fp.software_version, reads.software_version());

            let mut feature_map = None;
            set(
                &mut feature_map,
                resp.network_commissioning_read_resp().feature_map(),
            );
            if let Some(features) = feature_map {
                identity.network = crate::network::kind_of(features);
            }
        }

        match chunk.complete().await? {
            Some(next) => chunk = next,
            None => return Ok(identity),
        }
    }
}

/// Set `field` from the first report of a string attribute, if any.
fn set_string<'a, I>(field: &mut String, mut iter: I)
where
    I: Iterator<Item = (EndptId, Result<rs_matter::tlv::Utf8Str<'a>, Error>)>,
{
    if let Some((_, Ok(s))) = iter.next() {
        *field = s.to_string();
    }
}

/// Set `field` from the first report of a scalar attribute, if any.
fn set<T, I>(field: &mut Option<T>, mut iter: I)
where
    I: Iterator<Item = (EndptId, Result<T, Error>)>,
{
    if let Some((_, Ok(v))) = iter.next() {
        *field = Some(v);
    }
}
