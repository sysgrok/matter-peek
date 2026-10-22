//! The data the API speaks in, on every binding: Rust, the wire, UniFFI.

use core::fmt;

use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

/// How a [`MatterPeek`](super::MatterPeek) runs on this host.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct PeekConfig {
    /// Where the controller keeps its fabric, device records and OTA images; by
    /// default the platform's data directory.
    #[serde(default)]
    pub store_dir: Option<String>,
    /// Sync the device list shortly after start, and every few minutes after.
    #[serde(default)]
    pub periodic_sync: bool,
}

/// Why a request failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Error))]
#[serde(tag = "error", rename_all = "snake_case")]
pub enum PeekError {
    /// No listed device has this node ID.
    NoDevice { node_id: u64 },
    /// The commission was cancelled. `decommissioned` is `true` if the device
    /// had been given our fabric already, and was decommissioned on the way out.
    Cancelled { decommissioned: bool },
    /// Anything else.
    Failed { message: String },
}

#[cfg(feature = "controller")]
impl PeekError {
    pub(crate) fn failed(message: impl Into<String>) -> Self {
        Self::Failed {
            message: message.into(),
        }
    }
}

impl fmt::Display for PeekError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoDevice { node_id } => write!(f, "no device with node_id 0x{node_id:016X}"),
            Self::Cancelled {
                decommissioned: true,
            } => f.write_str("cancelled; the device was decommissioned"),
            Self::Cancelled {
                decommissioned: false,
            } => f.write_str("cancelled"),
            Self::Failed { message } => f.write_str(message),
        }
    }
}

impl core::error::Error for PeekError {}

/// How a commission reaches the device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(tag = "via", rename_all = "snake_case")]
pub enum CommissionVia {
    /// Over IP, at `addr` (e.g. `"[fdb5::1]:5540"`).
    Ip { addr: String },
    /// Over Bluetooth LE: the device at `addr` (e.g. `"E8:F6:0A:60:E4:72"`), or
    /// else the first one matching the setup code. It then joins `network`;
    /// none for a device already on its network (Ethernet).
    Ble {
        #[serde(default)]
        addr: Option<String>,
        #[serde(default)]
        network: Option<NetworkCredentials>,
    },
}

/// The operational network a device commissioned over Bluetooth joins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NetworkCredentials {
    Wifi {
        ssid: String,
        password: String,
    },
    /// The Thread operational dataset, hex-encoded.
    Thread {
        dataset: String,
    },
}

/// The steps of a commission, reported as it goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum CommissionStep {
    Commission,
}

/// How a commission step goes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum StepStatus {
    Started,
    Done,
    Failed { message: String },
}

/// The result of a commission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct CommissionResult {
    pub handle: DeviceHandle,
    /// Set when the just-remembered device's *per-model fingerprint* matched an
    /// existing remembered device (but no per-unit id did): the `node_id` of that
    /// existing device, offered as a "link these?" suggestion. `None` when the
    /// device was an exact re-match (auto-merged) or genuinely new.
    pub rematch_candidate: Option<u64>,
}

/// The commissionable devices found by a discovery.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct Discovered {
    /// On the network.
    pub devices: Vec<DiscoveredDevice>,
    /// Over Bluetooth LE.
    #[serde(default)]
    pub ble_devices: Vec<BleDevice>,
}

/// A commissionable device found on the network.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct DiscoveredDevice {
    /// Its address (e.g. `"[fdb5:…]:5540"`), for [`CommissionVia::Ip`].
    pub addr: String,
    /// The commissionable instance id from the advertisement (stable per device
    /// while its commissioning window is open).
    pub commissionable_id: u64,
}

/// A device advertising as commissionable over Bluetooth LE.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct BleDevice {
    /// Its Bluetooth address, for [`CommissionVia::Ble`].
    pub addr: String,
    pub discriminator: u16,
    pub vendor_id: u16,
    pub product_id: u16,
}

/// Identifies a commissioned device on our fabric.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct DeviceHandle {
    /// Our fabric, in the controller's fabric table.
    pub fabric_index: u8,
    pub node_id: u64,
}

/// The devices on our fabric: the remembered ones (also while offline), plus
/// the others while they are on it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct DeviceList {
    pub devices: Vec<DeviceInfo>,
    /// A remark about the operation that produced the list, e.g. that a
    /// decommission could not reach the device.
    #[serde(default)]
    pub notice: Option<String>,
}

/// A device on our fabric. See also its [`display`](super::display) helpers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct DeviceInfo {
    pub handle: DeviceHandle,
    /// BasicInformation `UniqueID` (stable across resets).
    pub unique_id: String,
    pub serial_number: String,
    /// User-set name (from the primary ecosystem, if it wrote `NodeLabel`).
    pub node_label: String,
    /// BasicInformation `VendorName` (empty if the device didn't serve it).
    pub vendor_name: String,
    /// BasicInformation `ProductName` (empty if the device didn't serve it).
    pub product_name: String,
    /// Per-model numeric fingerprint (vendor/product/hw/sw), for display and the
    /// re-match suggestion.
    pub fingerprint: Fingerprint,
    /// Local user tags for filtering/grouping; only a remembered device has any.
    pub tags: Vec<String>,
    /// Whether the device is reachable: as of the last sync, or since then its
    /// subscription or a peek; `None` before the first sync.
    #[serde(default)]
    pub online: Option<bool>,
    /// Whether we keep a record of the device (see
    /// [`MatterPeek::remember`](super::MatterPeek::remember)).
    pub remembered: bool,
    /// The kind of network it is on, if known.
    #[serde(default)]
    pub network: Option<NetworkKind>,
}

/// The numeric BasicInformation attributes of a device.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct Fingerprint {
    pub vendor_id: Option<u16>,
    pub product_id: Option<u16>,
    pub hardware_version: Option<u16>,
    pub software_version: Option<u32>,
}

/// A health snapshot read from a device's diagnostic clusters. Every field is
/// optional: devices implement different parts of them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct DeviceHealth {
    pub general: Option<GeneralDiag>,
    pub software: Option<SoftwareDiag>,
}

impl DeviceHealth {
    /// Merge in a subscription report, which carries only the attributes that
    /// changed: those replace the values held so far.
    pub fn merge(&mut self, delta: DeviceHealth) {
        if let Some(g) = delta.general {
            let base = self.general.get_or_insert_with(Default::default);
            base.up_time_secs = g.up_time_secs.or(base.up_time_secs);
            base.reboot_count = g.reboot_count.or(base.reboot_count);
            base.total_operational_hours =
                g.total_operational_hours.or(base.total_operational_hours);
        }
        if let Some(s) = delta.software {
            let base = self.software.get_or_insert_with(Default::default);
            base.current_heap_free = s.current_heap_free.or(base.current_heap_free);
            base.current_heap_used = s.current_heap_used.or(base.current_heap_used);
            base.current_heap_high_watermark = s
                .current_heap_high_watermark
                .or(base.current_heap_high_watermark);
        }
    }
}

/// Part of the General Diagnostics cluster (`0x0033`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct GeneralDiag {
    /// Seconds since the device last booted.
    pub up_time_secs: Option<u64>,
    /// Number of reboots since first commissioning.
    pub reboot_count: Option<u16>,
    /// Cumulative operational hours.
    pub total_operational_hours: Option<u32>,
}

/// Part of the Software Diagnostics cluster (`0x0034`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct SoftwareDiag {
    pub current_heap_free: Option<u64>,
    pub current_heap_used: Option<u64>,
    pub current_heap_high_watermark: Option<u64>,
}

/// The state of a live health subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SubscriptionState {
    /// Established and reporting; the device reports at least every
    /// `max_int_secs` seconds.
    Live { max_int_secs: u16 },
    /// The device went silent past its deadline (or the subscription failed to
    /// establish): it is offline. The subscription keeps being re-established in
    /// the background; the next `Live` marks recovery.
    Offline,
    /// The subscription was stopped (e.g. the device was decommissioned). No
    /// further reports arrive.
    Stopped,
}

/// The kinds of diagnostic log a device may keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum LogIntent {
    EndUserSupport,
    NetworkDiag,
    CrashLogs,
}

/// A stored OTA image, as its header describes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct OtaImageInfo {
    pub name: String,
    pub vendor_id: u16,
    pub product_id: u16,
    pub version: u32,
    pub version_str: String,
    pub min_applicable_version: Option<u32>,
    pub max_applicable_version: Option<u32>,
    pub release_notes_url: Option<String>,
    pub size: u64,
}

/// The stages of an OTA update, as the provider sees them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(tag = "stage", rename_all = "snake_case")]
pub enum OtaProgress {
    /// The device asked for an update and was offered `name`.
    Offered { name: String },
    /// The device asked for an update and was told there is none for it.
    NotOffered,
    /// The device downloaded `percent` of its image.
    Downloading { percent: u8 },
    /// The device was told to apply `version`.
    Applying { version: u32 },
    /// The device reports that it runs `version`.
    Applied { version: u32 },
}

/// What kind of network a device, or one of its interfaces, is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum NetworkKind {
    WiFi,
    Thread,
    Ethernet,
    Cellular,
    Unspecified,
}

/// A device's networking: its interfaces, the networks it knows, and the
/// diagnostics of the network it is on.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct NetworkInfo {
    pub interfaces: Vec<NetworkInterfaceInfo>,
    /// One per network interface it can be given networks for (its primary
    /// one on endpoint 0).
    pub configs: Vec<NetworkConfig>,
    pub wifi: Option<WifiDiag>,
    pub thread: Option<ThreadDiag>,
    pub ethernet: Option<EthernetDiag>,
}

/// A change to the networks a device knows (see
/// [`MatterPeek::change_network`](super::MatterPeek::change_network)).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(tag = "change", rename_all = "snake_case")]
pub enum NetworkChange {
    /// Add a network, or update the credentials of a known one.
    AddOrUpdate { credentials: NetworkCredentials },
    /// Forget a network; not the connected one.
    Remove { network_id: Vec<u8> },
    /// Connect to a known network.
    Connect { network_id: Vec<u8> },
    /// Move a network to `index` in the order the device tries them.
    Reorder { network_id: Vec<u8>, index: u8 },
}

/// A network interface of a device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct NetworkInterfaceInfo {
    pub name: String,
    pub kind: NetworkKind,
    pub operational: bool,
    /// Its MAC (or, on Thread, extended) address, e.g. `AA:BB:CC:DD:EE:FF`.
    pub hardware_address: String,
    pub ipv4_addresses: Vec<String>,
    pub ipv6_addresses: Vec<String>,
    /// Whether services off the premises are reachable over IPv4 / IPv6, if
    /// the device knows.
    pub off_premise_ipv4: Option<bool>,
    pub off_premise_ipv6: Option<bool>,
}

/// The networks a device can be given for one of its interfaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct NetworkConfig {
    pub endpoint: u16,
    pub kind: Option<NetworkKind>,
    pub interface_enabled: Option<bool>,
    /// How many networks it can keep.
    pub max_networks: Option<u8>,
    pub networks: Vec<KnownNetwork>,
    /// The outcome of its last network operation, e.g. a failed connect.
    pub last_status: Option<NetworkStatus>,
    /// The network of that operation, as in [`KnownNetwork`].
    pub last_network_id: Option<Vec<u8>>,
    pub last_network: Option<String>,
    /// The error that operation's connect failed with: an 802.11 reason or
    /// status code on Wi-Fi.
    pub last_connect_error: Option<i32>,
    pub connect_max_time_secs: Option<u8>,
    pub scan_max_time_secs: Option<u8>,
}

/// A network a device knows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct KnownNetwork {
    /// Its ID, as the device has it: an SSID, a Thread extended PAN ID, an
    /// Ethernet interface name.
    pub id: Vec<u8>,
    /// Its ID as text: the SSID, the extended PAN ID in hex, the name.
    pub name: String,
    pub connected: bool,
}

/// The outcome of a network operation on a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum NetworkStatus {
    Success,
    OutOfRange,
    BoundsExceeded,
    NetworkIdNotFound,
    DuplicateNetworkId,
    NetworkNotFound,
    RegulatoryError,
    AuthFailure,
    UnsupportedSecurity,
    OtherConnectionFailure,
    Ipv6Failed,
    IpBindFailed,
    UnknownError,
}

/// Wi-Fi Network Diagnostics.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct WifiDiag {
    /// The access point's address, e.g. `AA:BB:CC:DD:EE:FF`.
    pub bssid: Option<String>,
    pub security: Option<WifiSecurity>,
    pub version: Option<WifiVersion>,
    pub channel: Option<u16>,
    /// dBm.
    pub rssi: Option<i8>,
    /// Bits per second.
    pub current_max_rate: Option<u64>,
    pub beacon_lost_count: Option<u32>,
    pub beacon_rx_count: Option<u32>,
    pub packet_unicast_rx_count: Option<u32>,
    pub packet_unicast_tx_count: Option<u32>,
    pub packet_multicast_rx_count: Option<u32>,
    pub packet_multicast_tx_count: Option<u32>,
    pub overrun_count: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum WifiSecurity {
    Unspecified,
    None,
    Wep,
    Wpa,
    Wpa2,
    Wpa3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum WifiVersion {
    A,
    B,
    G,
    N,
    Ac,
    Ax,
    Ah,
}

/// Thread Network Diagnostics.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct ThreadDiag {
    pub role: Option<ThreadRole>,
    pub network_name: Option<String>,
    pub channel: Option<u16>,
    pub pan_id: Option<u16>,
    pub extended_pan_id: Option<u64>,
    /// E.g. `fd12:3456:789a:1::/64`.
    pub mesh_local_prefix: Option<String>,
    pub rloc16: Option<u16>,
    pub ext_address: Option<u64>,
    pub partition_id: Option<u32>,
    pub leader_router_id: Option<u8>,
    pub attach_attempt_count: Option<u16>,
    pub parent_change_count: Option<u16>,
    pub partition_id_change_count: Option<u16>,
    pub detached_role_count: Option<u16>,
    pub neighbors: Vec<ThreadNeighbor>,
    /// How many routes its route table holds.
    pub route_count: Option<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum ThreadRole {
    Unspecified,
    Unassigned,
    SleepyEndDevice,
    EndDevice,
    Reed,
    Router,
    Leader,
}

/// A neighbor of a Thread device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct ThreadNeighbor {
    pub ext_address: u64,
    pub rloc16: u16,
    /// Seconds since last heard.
    pub age_secs: u32,
    pub lqi: u8,
    /// dBm.
    pub average_rssi: Option<i8>,
    pub last_rssi: Option<i8>,
    /// Percent.
    pub frame_error_rate: u8,
    pub message_error_rate: u8,
    /// Whether it is our child (else, e.g., our parent or a router).
    pub is_child: bool,
    pub rx_on_when_idle: bool,
    pub full_thread_device: bool,
}

/// Ethernet Network Diagnostics.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct EthernetDiag {
    /// Mbit/s.
    pub phy_rate_mbps: Option<u32>,
    pub full_duplex: Option<bool>,
    pub carrier_detect: Option<bool>,
    pub packet_rx_count: Option<u64>,
    pub packet_tx_count: Option<u64>,
    pub tx_err_count: Option<u64>,
    pub collision_count: Option<u64>,
    pub overrun_count: Option<u64>,
    pub time_since_reset_secs: Option<u64>,
}

/// The assistant's model, as the API shows it: never its key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct AssistantInfo {
    /// The model's API, OpenAI-compatible: e.g. `http://localhost:11434/v1`.
    pub url: String,
    /// The model, as the API names it.
    pub model: String,
    /// The end of the API key (`…4f2a`), if there is one.
    pub key_hint: Option<String>,
    /// Whether the model thinks before it answers: slower, more careful.
    pub think: bool,
    /// Set by the environment of the process that runs the assistant
    /// (`MATTER_PEEK_LLM_*`), so not to be changed through the API.
    #[serde(default)]
    pub from_env: bool,
}

/// A change to the assistant's model; `None` keeps a setting as it is.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct AssistantUpdate {
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub think: Option<bool>,
    #[serde(default)]
    pub key: KeyChange,
}

/// What becomes of the assistant's API key.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum KeyChange {
    /// Keep it, while the API stays at the same address: a key only goes
    /// where it was given for.
    #[default]
    Keep,
    Set {
        key: String,
    },
    Remove,
}

impl fmt::Debug for KeyChange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Keep => f.write_str("Keep"),
            Self::Set { .. } => f.write_str("Set { .. }"),
            Self::Remove => f.write_str("Remove"),
        }
    }
}
