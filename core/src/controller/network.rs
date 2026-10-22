//! Reading a device's networking: its interfaces (General Diagnostics), the
//! networks it knows (Network Commissioning, one instance per interface it can
//! be given networks for), and the diagnostics of the network it is on (Wi-Fi,
//! Thread or Ethernet Network Diagnostics).

use core::net::{Ipv4Addr, Ipv6Addr};
use core::num::NonZeroU8;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use log::debug;

use rs_matter::crypto::Crypto;
use rs_matter::dm::clusters::decl::ethernet_network_diagnostics::{
    self as eth_diag, EthernetNetworkDiagnosticsAttrResponses as _, PHYRateEnum,
};
use rs_matter::dm::clusters::decl::general_diagnostics::{
    self as gen_diag, GeneralDiagnosticsAttrResponses as _, InterfaceTypeEnum,
};
use rs_matter::dm::clusters::decl::icd_management::{
    self as icd_mgmt, IcdManagementAttrResponses as _, OperatingModeEnum,
};
use rs_matter::dm::clusters::decl::network_commissioning::{
    self as net_comm, Feature, NetworkCommissioningAttrResponses as _,
    NetworkCommissioningStatusEnum,
};
use rs_matter::dm::clusters::decl::thread_network_diagnostics::{
    self as thread_diag, NeighborTableStruct, RouteTableStruct, RoutingRoleEnum,
    ThreadNetworkDiagnosticsAttrResponses as _,
};
use rs_matter::dm::clusters::decl::wi_fi_network_diagnostics::{
    self as wifi_diag, SecurityTypeEnum, WiFiNetworkDiagnosticsAttrResponses as _, WiFiVersionEnum,
};
use rs_matter::dm::EndptId;
use rs_matter::error::{Error, ErrorCode};
use rs_matter::im::client::ImClient as _;
use rs_matter::im::encoding::ReportDataResp;
use rs_matter::im::{AttrPath, GenericPath};
use rs_matter::tlv::Nullable;
use rs_matter::transport::exchange::Exchange;
use rs_matter::Matter;

use crate::api::{
    IcdMode, KnownNetwork, NetworkConfig, NetworkInfo, NetworkInterfaceInfo, NetworkKind,
    NetworkStatus, ThreadDiag, ThreadNeighbor, ThreadRole, ThreadRoute, WifiSecurity, WifiVersion,
};

use crate::diagnostics::list_items;

/// General Diagnostics lives on the root endpoint.
const ROOT_ENDPOINT: EndptId = 0;

/// Read the networking of the device `(fab_idx, node_id)`.
pub async fn read_network<C: Crypto>(
    matter: &Matter<'_>,
    crypto: C,
    fab_idx: NonZeroU8,
    node_id: u64,
) -> Result<NetworkInfo, Error> {
    // The network clusters whole (small but for the Thread tables), on every
    // endpoint that has them: one per network interface.
    let paths = [
        AttrPath::from_gp(&GenericPath::new(
            Some(ROOT_ENDPOINT),
            Some(gen_diag::FULL_CLUSTER.id),
            Some(gen_diag::AttributeId::NetworkInterfaces as _),
        )),
        whole(net_comm::FULL_CLUSTER.id),
        whole(wifi_diag::FULL_CLUSTER.id),
        whole(thread_diag::FULL_CLUSTER.id),
        whole(eth_diag::FULL_CLUSTER.id),
        whole(icd_mgmt::FULL_CLUSTER.id),
    ];

    let exchange = Exchange::initiate(matter, crypto, fab_idx, node_id).await?;
    let mut chunk = exchange
        .read_with(|msg| msg.attr_requests_from(&paths)?.fabric_filtered(true)?.end())
        .await?;

    let mut info = NetworkInfo::default();
    loop {
        {
            let resp = chunk.response()?;
            interfaces(&resp, &mut info);
            configs(&resp, &mut info);
            wifi(&resp, &mut info);
            thread(&resp, &mut info);
            ethernet(&resp, &mut info);
            icd(&resp, &mut info);
        }

        match chunk.complete().await? {
            Some(next) => chunk = next,
            None => break,
        }
    }

    for config in &mut info.configs {
        name_networks(config, info.thread.as_ref());
    }
    info.configs.sort_by_key(|config| config.endpoint);

    Ok(info)
}

/// The kind of network of a Network Commissioning instance with `features`.
pub fn kind_of(features: u32) -> Option<NetworkKind> {
    let features = Feature::from_bits_truncate(features);

    if features.contains(Feature::WI_FI_NETWORK_INTERFACE) {
        Some(NetworkKind::WiFi)
    } else if features.contains(Feature::THREAD_NETWORK_INTERFACE) {
        Some(NetworkKind::Thread)
    } else if features.contains(Feature::ETHERNET_NETWORK_INTERFACE) {
        Some(NetworkKind::Ethernet)
    } else {
        None
    }
}

/// Every attribute of `cluster`, on every endpoint.
fn whole(cluster: u32) -> AttrPath {
    AttrPath::from_gp(&GenericPath::new(None, Some(cluster), None))
}

fn interfaces(resp: &ReportDataResp<'_>, info: &mut NetworkInfo) {
    for (_, list) in resp.general_diagnostics_read_resp().network_interfaces() {
        let Ok(list) = list else { continue };

        for iface in list.iter() {
            let parsed = iface.and_then(|iface| {
                Ok(NetworkInterfaceInfo {
                    name: iface.name()?.to_string(),
                    kind: match iface.r#type()? {
                        InterfaceTypeEnum::WiFi => NetworkKind::WiFi,
                        InterfaceTypeEnum::Thread => NetworkKind::Thread,
                        InterfaceTypeEnum::Ethernet => NetworkKind::Ethernet,
                        InterfaceTypeEnum::Cellular => NetworkKind::Cellular,
                        InterfaceTypeEnum::Unspecified => NetworkKind::Unspecified,
                    },
                    operational: iface.is_operational()?,
                    hardware_address: hardware_address(&iface.hardware_address()?),
                    ipv4_addresses: iface
                        .i_pv_4_addresses()?
                        .iter()
                        .filter_map(|addr| ipv4(&addr.ok()?))
                        .collect(),
                    ipv6_addresses: iface
                        .i_pv_6_addresses()?
                        .iter()
                        .filter_map(|addr| ipv6(&addr.ok()?))
                        .collect(),
                    off_premise_ipv4: iface.off_premise_services_reachable_i_pv_4()?.into_option(),
                    off_premise_ipv6: iface.off_premise_services_reachable_i_pv_6()?.into_option(),
                })
            });

            match parsed {
                Ok(iface) => info.interfaces.push(iface),
                Err(e) => debug!("network: a network interface does not parse: {e:?}"),
            }
        }
    }
}

fn configs(resp: &ReportDataResp<'_>, info: &mut NetworkInfo) {
    let reads = resp.network_commissioning_read_resp();

    for (endpoint, value) in reads.feature_map() {
        if let Ok(features) = value {
            config(info, endpoint).kind = kind_of(features);
        }
    }
    for (endpoint, value) in reads.interface_enabled() {
        if let Ok(value) = value {
            config(info, endpoint).interface_enabled = Some(value);
        }
    }
    for (endpoint, value) in reads.max_networks() {
        if let Ok(value) = value {
            config(info, endpoint).max_networks = Some(value);
        }
    }
    for (endpoint, value) in reads.connect_max_time_seconds() {
        if let Ok(value) = value {
            config(info, endpoint).connect_max_time_secs = Some(value);
        }
    }
    for (endpoint, value) in reads.scan_max_time_seconds() {
        if let Ok(value) = value {
            config(info, endpoint).scan_max_time_secs = Some(value);
        }
    }
    for (endpoint, value) in reads.last_networking_status() {
        if let Ok(value) = value {
            config(info, endpoint).last_status = value.into_option().map(status);
        }
    }
    for (endpoint, value) in reads.last_network_id() {
        if let Ok(value) = value {
            // Named once the kind of network is known.
            config(info, endpoint).last_network_id = value.into_option().map(|id| id.to_vec());
        }
    }
    for (endpoint, value) in reads.last_connect_error_value() {
        if let Ok(value) = value {
            config(info, endpoint).last_connect_error = value.into_option();
        }
    }
    for (endpoint, value) in reads.networks() {
        let Ok(list) = value else { continue };

        let networks: Vec<_> = list
            .iter()
            .filter_map(|network| {
                let network = network.ok()?;
                Some(KnownNetwork {
                    id: network.network_id().ok()?.to_vec(),
                    name: String::new(),
                    connected: network.connected().ok()?,
                })
            })
            .collect();
        config(info, endpoint).networks = networks;
    }
}

fn wifi(resp: &ReportDataResp<'_>, info: &mut NetworkInfo) {
    let reads = resp.wi_fi_network_diagnostics_read_resp();

    // Present on a Wi-Fi device only: it reports at least one attribute.
    let mut seen = info.wifi.is_some();
    let mut diag = info.wifi.take().unwrap_or_default();

    nullable(&mut seen, &mut diag.bssid, reads.bssid(), |bssid| {
        hardware_address(&bssid)
    });
    nullable(
        &mut seen,
        &mut diag.security,
        reads.security_type(),
        |s| match s {
            SecurityTypeEnum::Unspecified => WifiSecurity::Unspecified,
            SecurityTypeEnum::None => WifiSecurity::None,
            SecurityTypeEnum::WEP => WifiSecurity::Wep,
            SecurityTypeEnum::WPA => WifiSecurity::Wpa,
            SecurityTypeEnum::WPA2 => WifiSecurity::Wpa2,
            SecurityTypeEnum::WPA3 => WifiSecurity::Wpa3,
        },
    );
    nullable(
        &mut seen,
        &mut diag.version,
        reads.wi_fi_version(),
        |v| match v {
            WiFiVersionEnum::A => WifiVersion::A,
            WiFiVersionEnum::B => WifiVersion::B,
            WiFiVersionEnum::G => WifiVersion::G,
            WiFiVersionEnum::N => WifiVersion::N,
            WiFiVersionEnum::Ac => WifiVersion::Ac,
            WiFiVersionEnum::Ax => WifiVersion::Ax,
            WiFiVersionEnum::Ah => WifiVersion::Ah,
        },
    );
    nullable(&mut seen, &mut diag.channel, reads.channel_number(), |v| v);
    nullable(&mut seen, &mut diag.rssi, reads.rssi(), |v| v);
    nullable(
        &mut seen,
        &mut diag.current_max_rate,
        reads.current_max_rate(),
        |v| v,
    );
    nullable(
        &mut seen,
        &mut diag.beacon_lost_count,
        reads.beacon_lost_count(),
        |v| v,
    );
    nullable(
        &mut seen,
        &mut diag.beacon_rx_count,
        reads.beacon_rx_count(),
        |v| v,
    );
    nullable(
        &mut seen,
        &mut diag.packet_unicast_rx_count,
        reads.packet_unicast_rx_count(),
        |v| v,
    );
    nullable(
        &mut seen,
        &mut diag.packet_unicast_tx_count,
        reads.packet_unicast_tx_count(),
        |v| v,
    );
    nullable(
        &mut seen,
        &mut diag.packet_multicast_rx_count,
        reads.packet_multicast_rx_count(),
        |v| v,
    );
    nullable(
        &mut seen,
        &mut diag.packet_multicast_tx_count,
        reads.packet_multicast_tx_count(),
        |v| v,
    );
    nullable(
        &mut seen,
        &mut diag.overrun_count,
        reads.overrun_count(),
        |v| v,
    );

    info.wifi = seen.then_some(diag);
}

fn thread(resp: &ReportDataResp<'_>, info: &mut NetworkInfo) {
    let reads = resp.thread_network_diagnostics_read_resp();

    let mut seen = info.thread.is_some();
    let mut diag = info.thread.take().unwrap_or_default();

    nullable(
        &mut seen,
        &mut diag.role,
        reads.routing_role(),
        |role| match role {
            RoutingRoleEnum::Unspecified => ThreadRole::Unspecified,
            RoutingRoleEnum::Unassigned => ThreadRole::Unassigned,
            RoutingRoleEnum::SleepyEndDevice => ThreadRole::SleepyEndDevice,
            RoutingRoleEnum::EndDevice => ThreadRole::EndDevice,
            RoutingRoleEnum::REED => ThreadRole::Reed,
            RoutingRoleEnum::Router => ThreadRole::Router,
            RoutingRoleEnum::Leader => ThreadRole::Leader,
        },
    );
    nullable(
        &mut seen,
        &mut diag.network_name,
        reads.network_name(),
        |name| name.to_string(),
    );
    nullable(&mut seen, &mut diag.channel, reads.channel(), |v| v);
    nullable(&mut seen, &mut diag.pan_id, reads.pan_id(), |v| v);
    nullable(
        &mut seen,
        &mut diag.extended_pan_id,
        reads.extended_pan_id(),
        |v| v,
    );
    nullable(
        &mut seen,
        &mut diag.mesh_local_prefix,
        reads.mesh_local_prefix(),
        |p| prefix(&p),
    );
    nullable(&mut seen, &mut diag.rloc16, reads.rloc_16(), |v| v);
    nullable(&mut seen, &mut diag.ext_address, reads.ext_address(), |v| v);
    nullable(
        &mut seen,
        &mut diag.partition_id,
        reads.partition_id(),
        |v| v,
    );
    nullable(
        &mut seen,
        &mut diag.leader_router_id,
        reads.leader_router_id(),
        |v| v,
    );
    plain(
        &mut seen,
        &mut diag.attach_attempt_count,
        reads.attach_attempt_count(),
    );
    plain(
        &mut seen,
        &mut diag.parent_change_count,
        reads.parent_change_count(),
    );
    plain(
        &mut seen,
        &mut diag.partition_id_change_count,
        reads.partition_id_change_count(),
    );
    plain(
        &mut seen,
        &mut diag.detached_role_count,
        reads.detached_role_count(),
    );

    seen |= list_items(
        resp,
        thread_diag::FULL_CLUSTER.id,
        thread_diag::AttributeId::NeighborTable as _,
        &mut diag.neighbors,
        neighbor,
    );
    if list_items(
        resp,
        thread_diag::FULL_CLUSTER.id,
        thread_diag::AttributeId::RouteTable as _,
        &mut diag.routes,
        route,
    ) {
        seen = true;
        diag.route_count = Some(diag.routes.len() as u16);
    }

    info.thread = seen.then_some(diag);
}

fn neighbor(n: &NeighborTableStruct<'_>) -> Option<ThreadNeighbor> {
    Some(ThreadNeighbor {
        ext_address: n.ext_address().ok()?,
        rloc16: n.rloc_16().ok()?,
        age_secs: n.age().ok()?,
        lqi: n.lqi().ok()?,
        average_rssi: n.average_rssi().ok()?.into_option(),
        last_rssi: n.last_rssi().ok()?.into_option(),
        frame_error_rate: n.frame_error_rate().ok()?,
        message_error_rate: n.message_error_rate().ok()?,
        is_child: n.is_child().ok()?,
        rx_on_when_idle: n.rx_on_when_idle().ok()?,
        full_thread_device: n.full_thread_device().ok()?,
    })
}

fn route(r: &RouteTableStruct<'_>) -> Option<ThreadRoute> {
    Some(ThreadRoute {
        ext_address: r.ext_address().ok()?,
        rloc16: r.rloc_16().ok()?,
        router_id: r.router_id().ok()?,
        next_hop: r.next_hop().ok()?,
        path_cost: r.path_cost().ok()?,
        lqi_in: r.lqi_in().ok()?,
        lqi_out: r.lqi_out().ok()?,
        age_secs: r.age().ok()?,
        allocated: r.allocated().ok()?,
        link_established: r.link_established().ok()?,
    })
}

fn icd(resp: &ReportDataResp<'_>, info: &mut NetworkInfo) {
    let reads = resp.icd_management_read_resp();

    let mut seen = info.icd.is_some();
    let mut icd = info.icd.take().unwrap_or_default();

    plain(
        &mut seen,
        &mut icd.idle_mode_secs,
        reads.idle_mode_duration(),
    );
    plain(
        &mut seen,
        &mut icd.active_mode_ms,
        reads.active_mode_duration(),
    );
    plain(
        &mut seen,
        &mut icd.active_threshold_ms,
        reads.active_mode_threshold(),
    );
    if let Some((_, Ok(mode))) = reads.operating_mode().next() {
        seen = true;
        icd.mode = Some(match mode {
            OperatingModeEnum::SIT => IcdMode::Sit,
            OperatingModeEnum::LIT => IcdMode::Lit,
        });
    }

    info.icd = seen.then_some(icd);
}

fn ethernet(resp: &ReportDataResp<'_>, info: &mut NetworkInfo) {
    let reads = resp.ethernet_network_diagnostics_read_resp();

    let mut seen = info.ethernet.is_some();
    let mut diag = info.ethernet.take().unwrap_or_default();

    nullable(
        &mut seen,
        &mut diag.phy_rate_mbps,
        reads.phy_rate(),
        |rate| match rate {
            PHYRateEnum::Rate10M => 10,
            PHYRateEnum::Rate100M => 100,
            PHYRateEnum::Rate1G => 1_000,
            PHYRateEnum::Rate25G => 2_500,
            PHYRateEnum::Rate5G => 5_000,
            PHYRateEnum::Rate10G => 10_000,
            PHYRateEnum::Rate40G => 40_000,
            PHYRateEnum::Rate100G => 100_000,
            PHYRateEnum::Rate200G => 200_000,
            PHYRateEnum::Rate400G => 400_000,
        },
    );
    nullable(&mut seen, &mut diag.full_duplex, reads.full_duplex(), |v| v);
    nullable(
        &mut seen,
        &mut diag.carrier_detect,
        reads.carrier_detect(),
        |v| v,
    );
    plain(
        &mut seen,
        &mut diag.packet_rx_count,
        reads.packet_rx_count(),
    );
    plain(
        &mut seen,
        &mut diag.packet_tx_count,
        reads.packet_tx_count(),
    );
    plain(&mut seen, &mut diag.tx_err_count, reads.tx_err_count());
    plain(
        &mut seen,
        &mut diag.collision_count,
        reads.collision_count(),
    );
    plain(&mut seen, &mut diag.overrun_count, reads.overrun_count());
    plain(
        &mut seen,
        &mut diag.time_since_reset_secs,
        reads.time_since_reset(),
    );

    info.ethernet = seen.then_some(diag);
}

/// The configuration of the Network Commissioning instance on `endpoint`.
fn config(info: &mut NetworkInfo, endpoint: EndptId) -> &mut NetworkConfig {
    let i = match info.configs.iter().position(|c| c.endpoint == endpoint) {
        Some(i) => i,
        None => {
            info.configs.push(NetworkConfig {
                endpoint,
                kind: None,
                interface_enabled: None,
                max_networks: None,
                networks: Vec::new(),
                last_status: None,
                last_network_id: None,
                last_network: None,
                last_connect_error: None,
                connect_max_time_secs: None,
                scan_max_time_secs: None,
            });
            info.configs.len() - 1
        }
    };

    &mut info.configs[i]
}

/// Name the networks of `config` by the kind of network: SSIDs and interface
/// names as text; a Thread network by the name the device's Thread
/// diagnostics give the one it is on, else by its extended PAN ID in hex.
fn name_networks(config: &mut NetworkConfig, thread: Option<&ThreadDiag>) {
    let kind = config.kind;
    // The diagnostics are of the network the device is on, which they may
    // leave unidentified.
    let connected: Option<Vec<u8>> = config
        .networks
        .iter()
        .find(|network| network.connected)
        .map(|network| network.id.clone());
    let thread_name = |id: &[u8]| {
        let thread = thread?;
        let on = match thread.extended_pan_id {
            Some(xpan) => extended_pan_id(id) == Some(xpan),
            None => connected.as_deref() == Some(id),
        };

        on.then(|| thread.network_name.clone()).flatten()
    };
    let name = |id: &[u8]| match kind {
        Some(NetworkKind::Thread) => thread_name(id).unwrap_or_else(|| hex(id)),
        _ => match core::str::from_utf8(id) {
            Ok(text) => text.to_string(),
            Err(_) => hex(id),
        },
    };

    for network in &mut config.networks {
        network.name = name(&network.id);
    }

    config.last_network = config.last_network_id.as_deref().map(name);
}

/// The extended PAN IDs of the Thread networks of `info` known by no name.
pub fn unnamed_thread_networks(info: &NetworkInfo) -> Vec<u64> {
    let mut unnamed: Vec<u64> = thread_configs(info)
        .flat_map(|config| &config.networks)
        .filter(|network| network.name == hex(&network.id))
        .filter_map(|network| extended_pan_id(&network.id))
        .collect();
    unnamed.sort_unstable();
    unnamed.dedup();

    unnamed
}

/// Name the Thread networks of `info` known by no name with `name`, by their
/// extended PAN ID.
pub fn name_thread_networks(info: &mut NetworkInfo, name: impl Fn(u64) -> Option<String>) {
    let name = |id: &[u8], current: &mut String| {
        if *current == hex(id) {
            if let Some(found) = extended_pan_id(id).and_then(&name) {
                *current = found;
            }
        }
    };

    for config in info
        .configs
        .iter_mut()
        .filter(|config| config.kind == Some(NetworkKind::Thread))
    {
        for network in &mut config.networks {
            name(&network.id, &mut network.name);
        }
        if let (Some(id), Some(last)) = (&config.last_network_id, &mut config.last_network) {
            name(id, last);
        }
    }
}

fn thread_configs(info: &NetworkInfo) -> impl Iterator<Item = &NetworkConfig> {
    info.configs
        .iter()
        .filter(|config| config.kind == Some(NetworkKind::Thread))
}

/// The extended PAN ID a Thread network ID is.
fn extended_pan_id(id: &[u8]) -> Option<u64> {
    id.try_into().ok().map(u64::from_be_bytes)
}

pub(crate) fn status(status: NetworkCommissioningStatusEnum) -> NetworkStatus {
    match status {
        NetworkCommissioningStatusEnum::Success => NetworkStatus::Success,
        NetworkCommissioningStatusEnum::OutOfRange => NetworkStatus::OutOfRange,
        NetworkCommissioningStatusEnum::BoundsExceeded => NetworkStatus::BoundsExceeded,
        NetworkCommissioningStatusEnum::NetworkIDNotFound => NetworkStatus::NetworkIdNotFound,
        NetworkCommissioningStatusEnum::DuplicateNetworkID => NetworkStatus::DuplicateNetworkId,
        NetworkCommissioningStatusEnum::NetworkNotFound => NetworkStatus::NetworkNotFound,
        NetworkCommissioningStatusEnum::RegulatoryError => NetworkStatus::RegulatoryError,
        NetworkCommissioningStatusEnum::AuthFailure => NetworkStatus::AuthFailure,
        NetworkCommissioningStatusEnum::UnsupportedSecurity => NetworkStatus::UnsupportedSecurity,
        NetworkCommissioningStatusEnum::OtherConnectionFailure => {
            NetworkStatus::OtherConnectionFailure
        }
        NetworkCommissioningStatusEnum::IPV6Failed => NetworkStatus::Ipv6Failed,
        NetworkCommissioningStatusEnum::IPBindFailed => NetworkStatus::IpBindFailed,
        NetworkCommissioningStatusEnum::UnknownError => NetworkStatus::UnknownError,
    }
}

/// Set `field` from the first report of a nullable attribute, noting in `seen`
/// that the cluster reported.
fn nullable<T, U>(
    seen: &mut bool,
    field: &mut Option<U>,
    mut iter: impl Iterator<Item = (EndptId, Result<Nullable<T>, Error>)>,
    f: impl FnOnce(T) -> U,
) {
    if let Some((_, Ok(value))) = iter.next() {
        *seen = true;
        *field = value.into_option().map(f);
    }
}

/// Set `field` from the first report of an attribute, noting in `seen` that the
/// cluster reported.
fn plain<T>(
    seen: &mut bool,
    field: &mut Option<T>,
    mut iter: impl Iterator<Item = (EndptId, Result<T, Error>)>,
) {
    if let Some((_, Ok(value))) = iter.next() {
        *seen = true;
        *field = Some(value);
    }
}

/// A MAC or EUI-64 address, e.g. `AA:BB:CC:DD:EE:FF`.
pub fn hardware_address(bytes: &[u8]) -> String {
    let mut out = String::new();

    for (i, byte) in bytes.iter().enumerate() {
        if i > 0 {
            out.push(':');
        }
        out.push_str(&format!("{byte:02X}"));
    }

    out
}

fn ipv4(bytes: &[u8]) -> Option<String> {
    let octets: [u8; 4] = bytes.try_into().ok()?;
    Some(Ipv4Addr::from(octets).to_string())
}

fn ipv6(bytes: &[u8]) -> Option<String> {
    let octets: [u8; 16] = bytes.try_into().ok()?;
    Some(Ipv6Addr::from(octets).to_string())
}

/// An IPv6 prefix as Thread carries it: its length in bits, then its bytes.
fn prefix(bytes: &[u8]) -> String {
    let Some((&len, rest)) = bytes.split_first() else {
        return String::new();
    };

    let mut octets = [0; 16];
    let n = rest.len().min(16);
    octets[..n].copy_from_slice(&rest[..n]);

    format!("{}/{len}", Ipv6Addr::from(octets))
}

/// Bytes in uppercase hex.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect()
}

/// Decode a hex string (with optional whitespace) into bytes, e.g. a Thread
/// operational dataset.
pub fn decode_hex(s: &str) -> Result<Vec<u8>, Error> {
    let cleaned: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();

    if !cleaned.len().is_multiple_of(2) {
        return Err(ErrorCode::InvalidData.into());
    }

    let mut out = Vec::with_capacity(cleaned.len() / 2);
    for &[hi, lo] in cleaned.as_chunks::<2>().0 {
        let hi = hex_val(hi).ok_or(ErrorCode::InvalidData)?;
        let lo = hex_val(lo).ok_or(ErrorCode::InvalidData)?;
        out.push((hi << 4) | lo);
    }

    Ok(out)
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod test {
    use alloc::vec;

    use alloc::string::{String, ToString};

    use super::*;

    #[test]
    fn decode_hex_round_trips() {
        assert_eq!(decode_hex("00ff10").unwrap(), vec![0x00, 0xff, 0x10]);
        assert_eq!(
            decode_hex("DE AD BE EF").unwrap(),
            vec![0xde, 0xad, 0xbe, 0xef]
        );
        assert!(decode_hex("abc").is_err()); // odd length
        assert!(decode_hex("zz").is_err()); // non-hex
    }

    /// A device on Thread network `on`, knowing `known` too, with
    /// diagnostics `thread`.
    fn thread_device(on: u64, known: u64, thread: Option<ThreadDiag>) -> NetworkInfo {
        let mut info = NetworkInfo {
            thread,
            ..Default::default()
        };
        let config = config(&mut info, 0);
        config.kind = Some(NetworkKind::Thread);
        config.networks = [(on, true), (known, false)]
            .into_iter()
            .map(|(xpan, connected)| KnownNetwork {
                id: xpan.to_be_bytes().to_vec(),
                name: String::new(),
                connected,
            })
            .collect();
        config.last_network_id = Some(on.to_be_bytes().to_vec());

        let thread = info.thread.clone();
        name_networks(&mut info.configs[0], thread.as_ref());

        info
    }

    fn names(info: &NetworkInfo) -> Vec<&str> {
        info.configs[0]
            .networks
            .iter()
            .map(|network| network.name.as_str())
            .collect()
    }

    #[test]
    fn thread_networks_named_by_their_diagnostics() {
        let diag = ThreadDiag {
            network_name: Some("NEST-PAN-0FD8".into()),
            extended_pan_id: Some(0xB566_147D_38E3_8420),
            ..Default::default()
        };
        let info = thread_device(0xB566_147D_38E3_8420, 0x1111_2222_3333_4444, Some(diag));
        assert_eq!(names(&info), ["NEST-PAN-0FD8", "1111222233334444"]);
        assert_eq!(
            info.configs[0].last_network.as_deref(),
            Some("NEST-PAN-0FD8")
        );
        assert_eq!(unnamed_thread_networks(&info), [0x1111_2222_3333_4444]);

        // Diagnostics without the extended PAN ID name the connected one.
        let diag = ThreadDiag {
            network_name: Some("WPAN-1".into()),
            ..Default::default()
        };
        let info = thread_device(1, 2, Some(diag));
        assert_eq!(names(&info), ["WPAN-1", "0000000000000002"]);
    }

    #[test]
    fn thread_networks_named_by_border_routers() {
        // All null, as some devices report.
        let mut info = thread_device(0xB566_147D_38E3_8420, 2, Some(ThreadDiag::default()));
        assert_eq!(names(&info), ["B566147D38E38420", "0000000000000002"]);
        assert_eq!(unnamed_thread_networks(&info), [2, 0xB566_147D_38E3_8420]);

        name_thread_networks(&mut info, |xpan| {
            (xpan == 0xB566_147D_38E3_8420).then(|| "NEST-PAN-0FD8".to_string())
        });
        assert_eq!(names(&info), ["NEST-PAN-0FD8", "0000000000000002"]);
        assert_eq!(
            info.configs[0].last_network.as_deref(),
            Some("NEST-PAN-0FD8")
        );
    }
}
