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
    /// Over Bluetooth LE: the device at `addr` (e.g. `"E8:F6:0A:12:34:56"`), or
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
    /// The device's index of the fabric whose administrator opened the
    /// commissioning window this commission came in through, when it could be
    /// read: for example Android's own fabric, when Google Play services hands
    /// a device over. That fabric can be removed with
    /// `MatterPeek::remove_device_fabric`.
    #[serde(default)]
    pub window_opener: Option<u8>,
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

/// A device the local network advertises as commissioned (operational mDNS),
/// with the fabrics it advertises on. One on no fabric of ours can only be
/// commissioned by us once its controller opens a commissioning window on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct AdvertisedDevice {
    /// Its IP addresses, best first.
    pub addresses: Vec<String>,
    pub fabrics: Vec<AdvertisedFabric>,
}

impl AdvertisedDevice {
    /// Whether it is on a fabric of ours.
    pub fn ours(&self) -> bool {
        self.fabrics.iter().any(|fabric| fabric.our_name.is_some())
    }
}

/// A fabric a device advertises on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct AdvertisedFabric {
    /// The ID its devices advertise it by.
    pub compressed_fabric_id: u64,
    /// The device's node ID on it.
    pub node_id: u64,
    /// Our name for it, if it is ours.
    pub our_name: Option<String>,
    /// The vendor of the controller that put a device of ours on it too, if
    /// one is, and the fabric's label, as that device has them.
    pub vendor_id: Option<u16>,
    pub label: Option<String>,
}

/// A device's data model as read from it: its endpoints, their device types
/// and clusters, the clusters' attributes (with their values), commands and
/// events; and the events it reported. Names and types come from the Matter
/// IDL, where it has them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct DeviceModel {
    pub node_id: u64,
    pub endpoints: Vec<EndpointModel>,
    /// The enums, bitmaps and structs the values are of, by
    /// [`ValueType::schema`].
    pub types: Vec<TypeInfo>,
    /// The events reported, oldest first.
    #[serde(default)]
    pub events: Vec<EventModel>,
}

/// An event a device reported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct EventModel {
    pub endpoint: u16,
    pub cluster: u32,
    pub event: u32,
    pub cluster_name: Option<String>,
    pub name: Option<String>,
    /// Event numbers count up, also across the device's reboots.
    pub number: u64,
    pub priority: EventPriority,
    /// When it happened: in UTC (ms since 1970), or else since the device
    /// started (ms).
    pub utc_ms: Option<u64>,
    pub uptime_ms: Option<u64>,
    /// Its fields, as JSON.
    pub fields: Option<String>,
    /// Why they could not be read.
    pub error: Option<String>,
}

/// How much an event matters; a device keeps the more important longer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum EventPriority {
    Debug,
    Info,
    Critical,
}

/// An event a cluster may report: its fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct EventTypeModel {
    pub id: u32,
    pub name: String,
    pub priority: EventPriority,
    pub fields: Vec<FieldType>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct EndpointModel {
    pub id: u16,
    pub device_types: Vec<DeviceTypeInfo>,
    pub clusters: Vec<ClusterModel>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct DeviceTypeInfo {
    pub id: u32,
    pub revision: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct ClusterModel {
    pub id: u32,
    pub name: Option<String>,
    pub attributes: Vec<AttributeModel>,
    /// The commands it accepts.
    pub commands: Vec<CommandModel>,
    /// The events it may report.
    #[serde(default)]
    pub events: Vec<EventTypeModel>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct AttributeModel {
    pub id: u32,
    pub name: Option<String>,
    pub writable: bool,
    /// Written with a timed interaction.
    pub timed: bool,
    pub value_type: Option<ValueType>,
    /// Its value, as JSON.
    pub value: Option<String>,
    /// Why it has no value.
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct CommandModel {
    pub id: u32,
    pub name: Option<String>,
    pub fields: Vec<FieldType>,
    /// The name of the command it answers with, unless a status.
    pub response: Option<String>,
    /// Invoked with a timed interaction.
    pub timed: bool,
}

/// The type of a value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct ValueType {
    pub kind: ValueKind,
    /// As the IDL names it: also says the unit (`temperature`: 0.01 °C,
    /// `percent100ths`, `epoch_s`, …).
    pub idl: String,
    /// The bits of an integer.
    pub bits: u8,
    pub list: bool,
    pub nullable: bool,
    pub optional: bool,
    /// An enum's, bitmap's or struct's index in [`DeviceModel::types`].
    pub schema: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum ValueKind {
    Bool,
    UInt,
    Int,
    Float,
    Text,
    Octets,
    Enum,
    Bitmap,
    Struct,
    Unknown,
}

/// An enum (its values), a bitmap (its bits' masks) or a struct (its fields).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct TypeInfo {
    pub name: String,
    pub values: Vec<NamedValue>,
    pub fields: Vec<FieldType>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct NamedValue {
    pub value: u64,
    pub name: String,
}

/// A field of a struct or a command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct FieldType {
    pub tag: u32,
    pub name: String,
    pub value_type: ValueType,
}

/// How much of a device's data model a subscription follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum ModelScope {
    /// The application endpoints: what a device's controls show.
    Application,
    /// Every endpoint, the root's too.
    All,
}

/// What a command answered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct CommandOutcome {
    /// The response command's name, unless the answer is a status.
    pub response: Option<String>,
    /// Its fields, as JSON.
    pub fields: Option<String>,
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

/// A fabric of ours: a "home" devices are commissioned into, with a root CA
/// of its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct FabricInfo {
    /// Its index: how the API names it, and the devices on it.
    pub index: u8,
    /// Its name, unique among ours.
    pub name: String,
    /// Its color: one of the front-ends' 8.
    pub color: u8,
    pub fabric_id: u64,
    pub compressed_fabric_id: u64,
    /// Our node ID on it.
    pub node_id: u64,
    pub vendor_id: u16,
    /// Whether a request naming no fabric gets it. It can't be deleted.
    pub default: bool,
    /// The devices listed on it.
    pub devices: u32,
}

/// Which of our fabrics a request is about.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(tag = "by", rename_all = "snake_case")]
pub enum FabricRef {
    /// The default fabric.
    #[default]
    Default,
    /// Every fabric of ours, where a request can be about several.
    All,
    Index {
        index: u8,
    },
    /// By name, whatever its case.
    Name {
        name: String,
    },
}

impl FabricRef {
    /// From what a user typed: `all`, a name, or an index (`2`, `#2`).
    pub fn parse(text: &str) -> Self {
        let text = text.trim();

        match text.strip_prefix('#').unwrap_or(text).parse::<u8>() {
            Ok(index) => Self::Index { index },
            Err(_) if text.eq_ignore_ascii_case("all") => Self::All,
            Err(_) => Self::Name { name: text.into() },
        }
    }
}

/// What becomes of the devices on a fabric we delete.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum FabricDeletion {
    /// Refuse while there are any.
    #[default]
    Refuse,
    /// Decommission each first: remove the fabric from it (if it is reached)
    /// and drop it from the list.
    Decommission,
    /// Forget them: they keep the fabric, now dead, in a slot of theirs until
    /// another controller removes it.
    Forget,
}

/// A controller's store, to move the controller or to keep a copy of it: its
/// key-value entries (our fabrics and their keys, the devices, the settings,
/// the assistant's model and key), and other files of its directory, which its
/// host adds (the server: the assistant's chats).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct StoreArchive {
    /// What it is: [`StoreArchive::FORMAT`].
    pub format: String,
    /// [`StoreArchive::VERSION`].
    pub version: u32,
    pub entries: Vec<StoreEntry>,
    #[serde(default)]
    pub files: Vec<StoreFile>,
}

impl StoreArchive {
    pub const FORMAT: &'static str = "matter-peek-store";
    pub const VERSION: u32 = 1;
}

/// A key-value entry of a controller's store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct StoreEntry {
    pub key: u16,
    /// Base64.
    pub data: String,
}

/// Another file of a controller's store directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct StoreFile {
    /// Its path in the directory, `/`-separated.
    pub path: String,
    /// Base64.
    pub data: String,
}

/// What a controller can do: as it was built, and now on its host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct Capabilities {
    /// Whether it was built to commission over Bluetooth LE
    /// ([`CommissionVia::Ble`]): on Linux, with BlueZ.
    pub ble: bool,
    /// Why it cannot commission over Bluetooth LE now, though it was built to:
    /// BlueZ does not run, there is no Bluetooth adapter, or it is off.
    #[serde(default)]
    pub ble_unavailable: Option<String>,
}

/// A device on our fabric. See also its [`display`](super::display) helpers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct DeviceInfo {
    pub handle: DeviceHandle,
    /// The name of the fabric of ours it is on.
    #[serde(default)]
    pub fabric: String,
    /// BasicInformation `UniqueID` (stable across resets).
    pub unique_id: String,
    pub serial_number: String,
    /// User-set name (from the primary ecosystem, if it wrote `NodeLabel`).
    pub node_label: String,
    /// BasicInformation `VendorName` (empty if the device didn't serve it).
    pub vendor_name: String,
    /// BasicInformation `ProductName` (empty if the device didn't serve it).
    pub product_name: String,
    /// BasicInformation `HardwareVersionString` (empty if not served).
    #[serde(default)]
    pub hardware_version_string: String,
    /// BasicInformation `SoftwareVersionString` (empty if not served).
    #[serde(default)]
    pub software_version_string: String,
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
    /// Whether we are registered for its Check-Ins (see
    /// [`MatterPeek::register_check_ins`](super::MatterPeek::register_check_ins)).
    #[serde(default)]
    pub check_ins: bool,
    /// For a device that sleeps long: how much longer it is awake, in ms, as
    /// far as we know as of this list (see
    /// [`MatterPeek::wake_device`](super::MatterPeek::wake_device)); `None`
    /// while it sleeps.
    #[serde(default)]
    pub awake_ms: Option<u32>,
    /// Whether it is kept awake (see
    /// [`MatterPeek::stay_awake`](super::MatterPeek::stay_awake)).
    #[serde(default)]
    pub stay_awake: bool,
    /// The kind of network it is on, if known.
    #[serde(default)]
    pub network: Option<NetworkKind>,
    /// The device types of its application endpoints, if known.
    #[serde(default)]
    pub device_types: Vec<u32>,
    /// What it serves, and the network it is on.
    #[serde(default)]
    pub details: DeviceDetails,
}

/// What a device serves and the network it is on, beyond its identity: read
/// with it, and again with each sync.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct DeviceDetails {
    /// The Matter specification version it implements, as BasicInformation
    /// `SpecificationVersion` has it: `0x01040200` is 1.4.2.
    #[serde(default)]
    pub spec_version: Option<u32>,
    /// The clusters it serves, on any endpoint, sorted.
    #[serde(default)]
    pub clusters: Vec<u32>,
    /// How many application endpoints it has (all but the root's).
    #[serde(default)]
    pub endpoints: Option<u16>,
    /// Its kinds of power source, if it says.
    #[serde(default)]
    pub power: Vec<PowerKind>,
    /// How it idles, if it is an intermittently connected device; its `mode`
    /// is always set: short without an OperatingMode.
    #[serde(default)]
    pub icd: Option<IcdInfo>,
    /// The Wi-Fi network (SSID) or Thread network (name) it is on; empty if
    /// unknown.
    #[serde(default)]
    pub network_name: String,
    /// Its role in its Thread network.
    #[serde(default)]
    pub thread_role: Option<ThreadRole>,
}

/// A kind of power source, as the Power Source cluster's features have it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum PowerKind {
    Wired,
    Battery,
    Rechargeable,
    Replaceable,
}

impl PowerKind {
    /// All of them, in the order of their feature bits.
    pub const ALL: [Self; 4] = [
        Self::Wired,
        Self::Battery,
        Self::Rechargeable,
        Self::Replaceable,
    ];

    /// Those of Power Source features `features`.
    pub fn of_features(features: u32) -> Vec<Self> {
        Self::ALL
            .into_iter()
            .enumerate()
            .filter(|(bit, _)| features & (1 << bit) != 0)
            .map(|(_, kind)| kind)
            .collect()
    }

    /// The feature bits of `kinds`.
    pub fn features(kinds: &[Self]) -> u32 {
        Self::ALL
            .into_iter()
            .enumerate()
            .filter(|(_, kind)| kinds.contains(kind))
            .fold(0, |bits, (bit, _)| bits | 1 << bit)
    }
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
    /// The fabrics the device is on: the controllers (ecosystems) it belongs
    /// to.
    #[serde(default)]
    pub fabrics: Option<Vec<DeviceFabric>>,
    /// How many fabrics it supports.
    #[serde(default)]
    pub supported_fabrics: Option<u8>,
    /// Whether `fabrics` adds to those reported before: a device sends a long
    /// list in pieces.
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub fabrics_appended: bool,
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
            if s.thread_metrics.is_some() {
                base.thread_metrics = s.thread_metrics;
            }
        }
        if let Some(fabrics) = delta.fabrics {
            match self.fabrics.as_mut() {
                Some(base) if delta.fabrics_appended => base.extend(fabrics),
                _ => self.fabrics = Some(fabrics),
            }
        }
        self.supported_fabrics = delta.supported_fabrics.or(self.supported_fabrics);
    }
}

/// A fabric a device is on: a controller (an ecosystem) it belongs to, from
/// its Operational Credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct DeviceFabric {
    /// The device's index of the fabric.
    pub index: Option<u8>,
    /// The vendor of the controller that put the device on the fabric.
    pub vendor_id: u16,
    /// The fabric's name, as its controller set it; empty if none.
    pub label: String,
    pub fabric_id: u64,
    /// The device's node ID on the fabric.
    pub node_id: u64,
    /// The public key of its root CA, in hex: who issued it.
    #[serde(default)]
    pub root_key: String,
    /// Whether it is a fabric of ours.
    #[serde(default)]
    pub ours: bool,
    /// Our name for it, if it is ours.
    #[serde(default)]
    pub our_name: Option<String>,
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct SoftwareDiag {
    pub current_heap_free: Option<u64>,
    pub current_heap_used: Option<u64>,
    pub current_heap_high_watermark: Option<u64>,
    /// The threads (tasks) of its software and their stacks, where it says;
    /// only a one-shot read (`peek_device`) asks for them.
    #[serde(default)]
    pub thread_metrics: Option<Vec<ThreadMetric>>,
}

/// A thread (task) of a device's software, as Software Diagnostics' Thread
/// Metrics give it; sizes in bytes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct ThreadMetric {
    pub id: u64,
    pub name: Option<String>,
    pub stack_size: Option<u32>,
    /// Free now.
    pub stack_free_current: Option<u32>,
    /// The least free since it booted.
    pub stack_free_minimum: Option<u32>,
}

impl ThreadMetric {
    /// The most of its stack it used since it booted: its size less the least
    /// free.
    pub fn stack_peak(&self) -> Option<u32> {
        Some(self.stack_size?.saturating_sub(self.stack_free_minimum?))
    }

    /// How it ranks by the stack it has to spare, the least first: by the
    /// share of its stack it never used, else by the bytes it never used.
    pub fn spare_rank(&self) -> (u8, u64) {
        match (
            self.stack_size.filter(|size| *size > 0),
            self.stack_free_minimum,
        ) {
            (Some(size), Some(least)) => (0, least as u64 * 1000 / size as u64),
            (None, Some(least)) => (1, least as u64),
            _ => (2, 0),
        }
    }
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
    /// The device was told to apply an update, but does not run it: `reason`
    /// says what it did.
    Failed { reason: String },
}

/// Whether a device's commissioning window is open, and how.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum WindowStatus {
    Closed,
    /// Open with a passcode of its opener's own.
    Enhanced,
    /// Open with the device's own setup code.
    Basic,
}

/// A device's commissioning window, through which another controller can
/// commission it too: with the code we show, while a window we opened is open.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct CommissioningWindow {
    pub node_id: u64,
    pub status: WindowStatus,
    /// The 11-digit manual pairing code, while a window we opened is open.
    pub manual_code: Option<String>,
    /// The QR code text (`MT:...`), likewise.
    pub qr_code: Option<String>,
    /// The QR code's modules, row by row, `1` dark.
    pub qr_rows: Vec<String>,
    /// The passcode, likewise (both codes hold it too).
    #[serde(default)]
    pub passcode: Option<u32>,
    pub discriminator: Option<u16>,
    /// Seconds until the window we opened closes.
    pub remaining_secs: Option<u64>,
    /// The vendor of the controller that opened the window, as the device says.
    pub opened_by_vendor: Option<u16>,
    /// The fabrics the device is on, and how many it supports.
    pub fabrics: Option<u8>,
    pub supported_fabrics: Option<u8>,
}

/// A device's clock, and how it keeps it, as its Time Synchronization cluster
/// tells.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct DeviceTime {
    pub node_id: u64,
    /// Whether it has the cluster: without it, it keeps no time we can see or
    /// set.
    pub supported: bool,
    pub features: TimeFeatures,
    /// Its UTC time, in milliseconds since the Unix epoch; `None` while it has
    /// none.
    pub utc_unix_ms: Option<u64>,
    /// How far its clock is ahead of ours (behind, if negative), in
    /// milliseconds, as read.
    pub offset_ms: Option<i64>,
    pub granularity: TimeGranularity,
    /// Where its time comes from: a `TimeSourceEnum` value of the spec (see
    /// [`time_source_label`](crate::api::display::time_source_label)).
    pub time_source: Option<u8>,
    /// The node it takes its time from.
    pub trusted_source: Option<TrustedTimeSource>,
    pub default_ntp: Option<String>,
    /// Its time zone; a second one is a scheduled change.
    pub time_zones: Vec<TimeZoneEntry>,
    /// Its daylight saving times, and how many it keeps.
    pub dst_offsets: Vec<DstOffsetEntry>,
    pub max_dst_offsets: Option<u8>,
    /// Its local time, in milliseconds since the Unix epoch.
    pub local_unix_ms: Option<u64>,
}

/// What a device's Time Synchronization cluster can do.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct TimeFeatures {
    /// It keeps a time zone and daylight saving times.
    pub time_zone: bool,
    /// It gets its time over NTP.
    pub ntp_client: bool,
    /// It serves NTP.
    pub ntp_server: bool,
    /// It gets its time from a trusted time source, a Matter node.
    pub time_sync_client: bool,
}

/// How accurate a clock is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum TimeGranularity {
    /// It has no time.
    #[default]
    NoTime,
    /// It had the time, but may have drifted more than 5 s since.
    Minutes,
    /// Within 5 s.
    Seconds,
    /// Within 50 ms.
    Milliseconds,
    /// Within 10 µs.
    Microseconds,
}

/// The node a device takes its time from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct TrustedTimeSource {
    /// The device's index of the fabric of the controller that set it.
    pub fabric_index: u8,
    pub node_id: u64,
    pub endpoint: u16,
    /// Whether it is us.
    pub ours: bool,
}

/// A time zone of a device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct TimeZoneEntry {
    /// Its offset from UTC, in seconds.
    pub offset_secs: i32,
    /// When it applies from, in milliseconds since the Unix epoch; `None` for
    /// the one in force.
    pub valid_at_unix_ms: Option<u64>,
    pub name: Option<String>,
}

/// A daylight saving time of a device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct DstOffsetEntry {
    /// What it adds to the time zone's offset, in seconds.
    pub offset_secs: i32,
    /// When it applies, in milliseconds since the Unix epoch: from (`None`:
    /// from ever) until (`None`: for good).
    pub from_unix_ms: Option<u64>,
    pub until_unix_ms: Option<u64>,
}

/// What setting a device's time did: each step, and the device's clock after.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct TimeSetup {
    pub steps: Vec<TimeSetupStep>,
    pub time: DeviceTime,
}

/// A step of setting a device's time: what was set, or why not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct TimeSetupStep {
    /// What was set, in words: "UTC time", "time zone Europe/Sofia, +02:00"...
    pub what: String,
    /// Why it was not; `None` if it was.
    pub error: Option<String>,
}

/// The latest OTA update of a device that asked us for one since the
/// controller started: the image, and how far it got.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct OtaUpdateInfo {
    pub node_id: u64,
    /// The image offered; `None` if none was.
    pub name: Option<String>,
    pub progress: OtaProgress,
    /// The bytes of the image downloaded so far, and its size.
    pub downloaded: Option<u64>,
    pub size: Option<u64>,
    /// Seconds since the device asked for the update.
    pub elapsed_secs: u64,
    /// Seconds since its last progress: a download with none for minutes has
    /// likely stalled.
    pub idle_secs: u64,
}

impl OtaProgress {
    /// Whether the update is still going: offered, downloading or applying.
    pub fn in_progress(&self) -> bool {
        matches!(
            self,
            Self::Offered { .. } | Self::Downloading { .. } | Self::Applying { .. }
        )
    }
}

impl OtaUpdateInfo {
    /// Whether the update is still going: offered, downloading or applying.
    pub fn in_progress(&self) -> bool {
        self.progress.in_progress()
    }

    /// The time left to download, estimated from the pace so far.
    pub fn remaining_secs(&self) -> Option<u64> {
        let (OtaProgress::Downloading { .. }, Some(downloaded), Some(size)) =
            (&self.progress, self.downloaded, self.size)
        else {
            return None;
        };
        if downloaded == 0 {
            return None;
        }

        let left = size.saturating_sub(downloaded) as u128;

        Some((left * self.elapsed_secs as u128 / downloaded as u128) as u64)
    }
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
    /// How it sleeps, if it is an Intermittently Connected Device.
    #[serde(default)]
    pub icd: Option<IcdInfo>,
}

/// How an Intermittently Connected Device sleeps (ICD Management).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct IcdInfo {
    /// Short or long idle time; `None` where it can only do short.
    pub mode: Option<IcdMode>,
    /// How long it stays idle between active periods, s.
    pub idle_mode_secs: Option<u32>,
    /// How long an active period lasts, ms.
    pub active_mode_ms: Option<u32>,
    /// How long it stays active after its last activity, ms.
    pub active_threshold_ms: Option<u16>,
    /// Whether it can idle long: it does once a client registers for its
    /// Check-Ins.
    #[serde(default)]
    pub long_idle: bool,
    /// Whether it sends Check-Ins to the clients registered with it.
    #[serde(default)]
    pub check_in: bool,
    /// How a user wakes it up, if it says: its UserActiveModeTriggerHint bits.
    #[serde(default)]
    pub user_trigger: Option<u32>,
    /// The instruction that goes with `user_trigger`; empty if none.
    #[serde(default)]
    pub user_trigger_instruction: String,
}

/// An ICD's idle time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum IcdMode {
    /// Short idle time: it polls often, and can be reached within seconds.
    Sit,
    /// Long idle time: it sleeps long, and checks in with its subscribers.
    Lit,
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
    /// Its route table: on a router, the other routers of its partition.
    #[serde(default)]
    pub routes: Vec<ThreadRoute>,
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

/// A route of a Thread router, to another router of its partition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct ThreadRoute {
    pub ext_address: u64,
    pub rloc16: u16,
    pub router_id: u8,
    /// The router ID of the next hop on the way there.
    pub next_hop: u8,
    pub path_cost: u8,
    /// The quality of the link from that router, as this one hears it.
    pub lqi_in: u8,
    /// The quality of the link to that router, as that one hears it.
    pub lqi_out: u8,
    /// Seconds since last heard.
    pub age_secs: u8,
    /// Whether the router ID is allocated: entries that are not stand for no
    /// router.
    pub allocated: bool,
    /// Whether this router links to that one directly.
    pub link_established: bool,
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

/// A Thread border router on the local network, as it advertises itself
/// (MeshCoP, `_meshcop._udp`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct BorderRouter {
    /// As it names itself, e.g. `Google Inc. Google Nest Hub #1234`.
    pub name: String,
    pub vendor: Option<String>,
    pub model: Option<String>,
    pub network_name: Option<String>,
    pub extended_pan_id: Option<u64>,
    /// Its Thread extended address.
    pub ext_address: Option<u64>,
    pub partition_id: Option<u32>,
    pub thread_version: Option<String>,
    /// Its off-mesh routable prefix, through which the LAN reaches the
    /// Thread devices, e.g. `fd12:3456:789a:1::/64`.
    pub omr_prefix: Option<String>,
    /// Its address on the LAN.
    pub address: Option<String>,
}

/// The Thread mesh as our devices see it: their networks, the nodes and links
/// their tables show, the border routers on the LAN, and what is wrong.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct ThreadMesh {
    pub networks: Vec<MeshNetwork>,
    /// Our devices first, then the border routers, then the rest.
    pub nodes: Vec<MeshNode>,
    #[serde(default)]
    pub counts: MeshCounts,
    pub links: Vec<MeshLink>,
    /// Where routers no link is known to sit: how our routers reach them.
    #[serde(default)]
    pub routes: Vec<MeshRoute>,
    /// Matter nodes on Thread the LAN advertises, not on our fabric.
    #[serde(default)]
    pub lan_nodes: Vec<LanNode>,
    pub border_routers: Vec<BorderRouter>,
    /// The warnings first.
    pub findings: Vec<MeshFinding>,
    /// Our Thread devices that were not read, and why.
    pub unread: Vec<UnreadDevice>,
}

/// A Thread network: our devices on it, and its border routers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct MeshNetwork {
    pub extended_pan_id: u64,
    pub name: Option<String>,
    pub channel: Option<u16>,
    pub pan_id: Option<u16>,
    /// The partitions its nodes say they are in: more than one is a split
    /// network.
    pub partitions: Vec<u32>,
    /// Our devices on it, by node ID.
    pub devices: Vec<u64>,
    /// Its border routers, by name.
    pub border_routers: Vec<String>,
}

/// A node of the mesh.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct MeshNode {
    pub ext_address: u64,
    pub label: String,
    pub kind: MeshNodeKind,
    pub role: Option<ThreadRole>,
    pub rloc16: Option<u16>,
    pub extended_pan_id: Option<u64>,
    pub partition_id: Option<u32>,
    /// A child's parent, by extended address.
    pub parent: Option<u64>,
    /// How it sleeps, where it says: its ICD Management, its mDNS advert.
    #[serde(default)]
    pub sleep: Option<MeshSleep>,
}

impl MeshNode {
    /// Whether it sleeps between polls of its parent: by its role, else by
    /// how it says it sleeps.
    pub fn sleepy(&self) -> bool {
        match self.role {
            Some(ThreadRole::SleepyEndDevice) => true,
            Some(
                ThreadRole::Router | ThreadRole::Leader | ThreadRole::EndDevice | ThreadRole::Reed,
            ) => false,
            _ => self.sleep.as_ref().is_some_and(MeshSleep::sleepy),
        }
    }
}

impl MeshSleep {
    /// Whether it says it sleeps: as an ICD, or polling seldom.
    pub fn sleepy(&self) -> bool {
        /// An idle polling interval from which a node sleeps, ms.
        const SLEEPY_IDLE_MS: u32 = 1_000;

        self.icd_mode.is_some() || self.idle_interval_ms.is_some_and(|ms| ms >= SLEEPY_IDLE_MS)
    }
}

/// A Matter node on Thread the LAN advertises (`_matter._tcp`) that is not
/// on our fabric: its host name is not its extended address, so where it sits
/// in the mesh is unknown (it may be one of the nodes our devices' tables
/// show, under that address).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct LanNode {
    /// Its host name, e.g. `36C29F8DB1F0D7F3.local`.
    pub host: String,
    /// Its address, in a border router's prefix.
    pub address: String,
    /// The fabrics it is on, and its node ID on each.
    pub fabrics: Vec<LanFabric>,
    pub sleep: Option<MeshSleep>,
}

impl LanNode {
    pub fn sleepy(&self) -> bool {
        self.sleep.as_ref().is_some_and(MeshSleep::sleepy)
    }
}

/// A fabric a [`LanNode`] is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct LanFabric {
    pub compressed_fabric_id: u64,
    pub node_id: u64,
}

/// How a node sleeps, as far as it says.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct MeshSleep {
    /// Its ICD idle time, if it is an Intermittently Connected Device.
    pub icd_mode: Option<IcdMode>,
    /// How often it polls its parent while idle, ms (as it advertises).
    pub idle_interval_ms: Option<u32>,
    /// How often it polls while active, ms.
    pub active_interval_ms: Option<u32>,
    /// How long it stays active after its last activity, ms.
    pub active_threshold_ms: Option<u32>,
    /// How long it stays idle between active periods, s (ICD).
    pub idle_mode_secs: Option<u32>,
}

/// The nodes of a mesh, counted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct MeshCounts {
    pub nodes: u32,
    pub on_our_fabric: u32,
    pub border_routers: u32,
    /// Routers, the leaders among them.
    pub routers: u32,
    /// End devices that listen all the time.
    pub end_devices: u32,
    pub sleepy_end_devices: u32,
    /// Nodes whose role is unknown.
    pub unknown_role: u32,
    /// Matter nodes on Thread seen on the LAN only, and how many of them
    /// sleep: some may also be among the nodes.
    pub lan_nodes: u32,
    pub lan_sleepy: u32,
}

/// What a node of the mesh is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MeshNodeKind {
    /// A device on our fabric.
    Device {
        node_id: u64,
    },
    BorderRouter,
    /// A Matter node of another fabric.
    OtherFabric {
        node_id: u64,
        compressed_fabric_id: u64,
    },
    /// Our devices' tables show it, but nothing names it.
    Unknown,
}

/// A link between two nodes, as each hears the other.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct MeshLink {
    pub a: u64,
    pub b: u64,
    /// Whether one is the other's parent; else, two routers.
    pub parent_child: bool,
    pub a_hears_b: Option<LinkSignal>,
    pub b_hears_a: Option<LinkSignal>,
}

/// How one of our routers reaches a router it has no link to: through
/// another router first, at a cost. Not a link: the routers in between are
/// unknown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct MeshRoute {
    /// Our router.
    pub from: u64,
    /// The router it reaches.
    pub to: u64,
    /// The router it goes through first, where its route table names it.
    pub via: Option<u64>,
    pub path_cost: u8,
}

/// How well a node hears another.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct LinkSignal {
    /// Link quality, from 0 (none) to 3 (good).
    pub quality: Option<u8>,
    /// Average signal strength, dBm.
    pub rssi: Option<i8>,
}

/// Something wrong with the mesh, or worth knowing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct MeshFinding {
    pub severity: FindingSeverity,
    pub kind: FindingKind,
    /// What it is, and what to do about it, in plain words.
    pub text: String,
    /// The nodes it is about, by extended address.
    pub nodes: Vec<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum FindingSeverity {
    Warning,
    Info,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[serde(rename_all = "snake_case")]
pub enum FindingKind {
    /// Several Thread networks on the LAN.
    CompetingNetworks,
    /// A network split into partitions.
    PartitionSplit,
    WeakLink,
    /// A node hears another that does not hear it.
    OneWayLink,
    /// A router some of our devices reach the border routers only through.
    CriticalRouter,
    SingleBorderRouter,
    /// No border router on the LAN for a network of our devices.
    NoBorderRouter,
    /// A device that keeps changing its parent.
    ParentChurn,
    /// A device that keeps losing the network.
    Detaches,
    /// A Thread channel on top of a Wi-Fi channel in use.
    ChannelOverlap,
    /// A network with OpenThread's well-known default credentials.
    DefaultCredentials,
    /// No route from this machine to a network's prefix.
    NoHostRoute,
    /// Devices that were not read.
    Unread,
}

/// One of our devices that was not read for the mesh, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct UnreadDevice {
    pub node_id: u64,
    pub title: String,
    pub reason: String,
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    fn fabric(index: u8) -> DeviceFabric {
        DeviceFabric {
            index: Some(index),
            vendor_id: 0x6006,
            label: String::new(),
            fabric_id: index.into(),
            node_id: 0x1B669,
            root_key: String::new(),
            ours: false,
            our_name: None,
        }
    }

    #[test]
    fn fabrics_sent_in_pieces_add_up_and_a_whole_list_replaces_them() {
        let mut health = DeviceHealth::default();
        let report = |fabrics, appended, supported| DeviceHealth {
            fabrics: Some(fabrics),
            fabrics_appended: appended,
            supported_fabrics: supported,
            ..Default::default()
        };

        health.merge(report(vec![fabric(1)], false, Some(5)));
        health.merge(report(vec![fabric(2)], true, None));
        assert_eq!(health.fabrics, Some(vec![fabric(1), fabric(2)]));
        assert_eq!(health.supported_fabrics, Some(5));

        health.merge(report(vec![fabric(3)], false, None));
        assert_eq!(health.fabrics, Some(vec![fabric(3)]));
        assert_eq!(health.supported_fabrics, Some(5));

        health.merge(DeviceHealth::default());
        assert_eq!(health.fabrics, Some(vec![fabric(3)]));
    }
}
