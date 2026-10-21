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
            Self::NoDevice { node_id } => write!(f, "no device with node_id 0x{node_id:016x}"),
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

/// A device on our fabric.
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
    /// A ready-to-show vendor label: the device's `VendorName` when present,
    /// else an offline VID→name lookup, else the raw hex `vid:0x…`.
    pub vendor_display: String,
    /// `true` when the vendor id is in the reserved **test** range
    /// (`0xFFF1`–`0xFFF4`): a development / uncertified device.
    pub is_test_vendor: bool,
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
