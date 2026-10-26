//! Reading a device's identity in one round trip: its BasicInformation (the
//! stable string identifiers `UniqueID` / `SerialNumber` / `NodeLabel`, its
//! names and version strings, and a fingerprint from the mandatory numeric
//! attributes as a fallback), the kind of network it is on, and its details:
//! what it serves, its power, how it idles, and its network.
//!
//! All reads are best-effort: an optional attribute the device doesn't serve
//! comes back as an attribute-status error in that attribute's iterator, which
//! we map to an empty field / `None`.

extern crate alloc;
use alloc::collections::BTreeSet;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use core::num::NonZeroU8;

use log::debug;

use rs_matter::crypto::Crypto;
use rs_matter::dm::clusters::decl::basic_information::{self, BasicInformationAttrResponses as _};
use rs_matter::dm::clusters::decl::descriptor::{self, DescriptorAttrResponses as _};
use rs_matter::dm::clusters::decl::icd_management;
use rs_matter::dm::clusters::decl::network_commissioning::{
    self, NetworkCommissioningAttrResponses as _,
};
use rs_matter::dm::clusters::decl::power_source::{self, PowerSourceAttrResponses as _};
use rs_matter::dm::clusters::decl::thread_network_diagnostics::{
    self, RoutingRoleEnum, ThreadNetworkDiagnosticsAttrResponses as _,
};
use rs_matter::dm::EndptId;
use rs_matter::error::Error;
use rs_matter::im::client::ImClient as _;
use rs_matter::im::{AttrPath, GenericPath};
use rs_matter::transport::exchange::Exchange;
use rs_matter::Matter;

use crate::api::{IcdMode, NetworkKind, PowerKind, ThreadRole};
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
    // BasicInformation, the primary interface's Network Commissioning and ICD
    // Management whole (they are small); eight paths in all, within the nine
    // a device must take.
    let paths = [
        AttrPath::from_gp(&GenericPath::new(
            Some(ROOT_ENDPOINT),
            Some(basic_information::FULL_CLUSTER.id),
            None,
        )),
        AttrPath::from_gp(&GenericPath::new(
            Some(ROOT_ENDPOINT),
            Some(network_commissioning::FULL_CLUSTER.id),
            None,
        )),
        // What it is, and what it serves, on every endpoint.
        attr(
            descriptor::FULL_CLUSTER.id,
            descriptor::AttributeId::DeviceTypeList as _,
        ),
        attr(
            descriptor::FULL_CLUSTER.id,
            descriptor::AttributeId::ServerList as _,
        ),
        attr(
            power_source::FULL_CLUSTER.id,
            power_source::AttributeId::FeatureMap as _,
        ),
        AttrPath::from_gp(&GenericPath::new(
            None,
            Some(icd_management::FULL_CLUSTER.id),
            None,
        )),
        attr(
            thread_network_diagnostics::FULL_CLUSTER.id,
            thread_network_diagnostics::AttributeId::NetworkName as _,
        ),
        attr(
            thread_network_diagnostics::FULL_CLUSTER.id,
            thread_network_diagnostics::AttributeId::RoutingRole as _,
        ),
    ];

    let exchange = Exchange::initiate(matter, crypto, fab_idx, node_id).await?;
    let mut chunk = exchange
        .read_with(|msg| msg.attr_requests_from(&paths)?.fabric_filtered(true)?.end())
        .await?;

    // Each chunk carries some of the attributes; an attribute the device does
    // not serve comes back as a status, read as empty / `None`.
    let mut identity = DeviceIdentity::default();
    let mut clusters = BTreeSet::new();
    let mut endpoints = BTreeSet::new();
    let mut power = None;
    let mut icd = None;
    let mut networks: Vec<(Vec<u8>, bool)> = Vec::new();
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
            set(
                &mut identity.details.spec_version,
                reads.specification_version(),
            );

            let mut feature_map = None;
            set(
                &mut feature_map,
                resp.network_commissioning_read_resp().feature_map(),
            );
            if let Some(features) = feature_map {
                identity.network = crate::network::kind_of(features);
            }
            for (_, list) in resp.network_commissioning_read_resp().networks() {
                let Ok(list) = list else { continue };
                for network in list.iter().filter_map(Result::ok) {
                    if let (Ok(id), Ok(connected)) = (network.network_id(), network.connected()) {
                        networks.push((id.to_vec(), connected));
                    }
                }
            }

            for (endpoint, list) in resp.descriptor_read_resp().server_list() {
                let Ok(list) = list else { continue };
                if endpoint != ROOT_ENDPOINT {
                    endpoints.insert(endpoint);
                }
                clusters.extend(list.iter().filter_map(Result::ok));
            }
            for (_, features) in resp.power_source_read_resp().feature_map() {
                if let Ok(features) = features {
                    power = Some(power.unwrap_or(0) | features);
                }
            }
            crate::icd::read(&resp, &mut icd);

            let thread = resp.thread_network_diagnostics_read_resp();
            for (_, name) in thread.network_name() {
                if let Ok(Some(name)) = name.map(|name| name.into_option()) {
                    identity.details.network_name = name.to_string();
                }
            }
            for (_, role) in thread.routing_role() {
                if let Ok(Some(role)) = role.map(|role| role.into_option()) {
                    identity.details.thread_role = Some(thread_role(role));
                }
            }

            for (endpoint, list) in resp.descriptor_read_resp().device_type_list() {
                if endpoint == ROOT_ENDPOINT {
                    continue;
                }
                let types = list.into_iter().flat_map(|list| list.iter());
                for device_type in types.filter_map(Result::ok) {
                    if let Ok(id) = device_type.device_type() {
                        if !identity.device_types.contains(&id) {
                            identity.device_types.push(id);
                        }
                    }
                }
            }
        }

        match chunk.complete().await? {
            Some(next) => chunk = next,
            None => break,
        }
    }

    let details = &mut identity.details;
    if !clusters.is_empty() {
        details.endpoints = Some(endpoints.len() as u16);
        // The ICD Management cluster without a mode is a short-idle one.
        if clusters.contains(&icd_management::FULL_CLUSTER.id) {
            let mut icd = icd.unwrap_or_default();
            icd.mode.get_or_insert(IcdMode::Sit);
            details.icd = Some(icd);
        }
        details.clusters = clusters.into_iter().collect();
    }
    details.power = power.map(PowerKind::of_features).unwrap_or_default();
    // A Wi-Fi device's network: the one flagged connected, or its only one.
    if identity.network == Some(NetworkKind::WiFi) {
        let only = (networks.len() == 1).then(|| &networks[0].0);
        let ssid = networks
            .iter()
            .find(|(_, connected)| *connected)
            .map(|(id, _)| id)
            .or(only);
        if let Some(ssid) = ssid {
            details.network_name = String::from_utf8_lossy(ssid).into_owned();
        }
    }

    Ok(identity)
}

/// Attribute `attribute` of cluster `cluster`, on every endpoint.
fn attr(cluster: u32, attribute: u32) -> AttrPath {
    AttrPath::from_gp(&GenericPath::new(None, Some(cluster), Some(attribute)))
}

/// `role`, as the API has it.
fn thread_role(role: RoutingRoleEnum) -> ThreadRole {
    match role {
        RoutingRoleEnum::Unspecified => ThreadRole::Unspecified,
        RoutingRoleEnum::Unassigned => ThreadRole::Unassigned,
        RoutingRoleEnum::SleepyEndDevice => ThreadRole::SleepyEndDevice,
        RoutingRoleEnum::EndDevice => ThreadRole::EndDevice,
        RoutingRoleEnum::REED => ThreadRole::Reed,
        RoutingRoleEnum::Router => ThreadRole::Router,
        RoutingRoleEnum::Leader => ThreadRole::Leader,
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
