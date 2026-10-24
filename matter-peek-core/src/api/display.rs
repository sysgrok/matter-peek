//! How the API's values are shown to people, so that every front-end (the web
//! UI, the CLI, the MCP tools, the apps) shows them alike:
//!
//! - IDs in hex: `0x` and uppercase digits, zero-padded (`0x1490`,
//!   `0x000000000001B67B`);
//! - vendors and products by name, with the ID in brackets (`Shelly (0x1490)`),
//!   named by the device, else by the CSA's registry (the DCL);
//! - versions by the device's string, with the number in hex in brackets
//!   (`1.3.0-s1 (0x01030041)`);
//! - durations in units, from the largest one present (`2mo 4d 0h 6m 1s`);
//! - moments as `2026-10-23 14:05:09 UTC`, UTC offsets as `+02:00`.

use alloc::format;
use alloc::string::{String, ToString};

use super::types::{
    DeviceFabric, DeviceInfo, DeviceTime, NetworkKind, NetworkStatus, ThreadRole, TimeGranularity,
    WifiSecurity, WifiVersion,
};

mod clusters;
mod dcl;
mod device_types;

impl DeviceInfo {
    /// What the device is: the first device type of its application
    /// endpoints that is not the node's plumbing.
    pub fn device_type(&self) -> Option<u32> {
        self.device_types
            .iter()
            .copied()
            .find(|id| !is_utility_device_type(*id))
    }

    /// What to call the device: its label, else its product name, else its
    /// stable IDs, else its node ID.
    pub fn title(&self) -> String {
        let product = Some(self.product_name.as_str())
            .filter(|name| !name.is_empty())
            .or_else(|| {
                let fp = &self.fingerprint;
                static_product_name(fp.vendor_id?, fp.product_id?)
            });

        if !self.node_label.is_empty() {
            self.node_label.clone()
        } else if let Some(product) = product {
            product.to_string()
        } else if !self.unique_id.is_empty() {
            format!("uid {}", self.unique_id)
        } else if !self.serial_number.is_empty() {
            format!("sn {}", self.serial_number)
        } else {
            format!("node {}", hex_u64(self.handle.node_id))
        }
    }

    /// The device's vendor, as [`vendor_label`] shows it; `None` if the device
    /// served neither its vendor ID nor its name.
    pub fn vendor(&self) -> Option<String> {
        let name = Some(self.vendor_name.clone()).filter(|name| !name.is_empty());

        match self.fingerprint.vendor_id {
            Some(vid) => Some(vendor_label(vid, name)),
            None => name,
        }
    }

    /// The device's product, as [`product_label`] shows it; `None` if the device
    /// served neither its product ID nor its name.
    pub fn product(&self) -> Option<String> {
        let name = Some(self.product_name.clone()).filter(|name| !name.is_empty());

        match (self.fingerprint.vendor_id, self.fingerprint.product_id) {
            (Some(vid), Some(pid)) => Some(product_label(vid, pid, name)),
            _ => name,
        }
    }

    /// The device's hardware version, as [`hardware_version_label`] shows it;
    /// `None` if the device served neither its number nor its string.
    pub fn hardware_version(&self) -> Option<String> {
        let name = non_empty(&self.hardware_version_string);

        match self.fingerprint.hardware_version {
            Some(version) => Some(hardware_version_label(version, name)),
            None => name,
        }
    }

    /// The device's software version, as [`software_version_label`] shows it;
    /// `None` if the device served neither its number nor its string.
    pub fn software_version(&self) -> Option<String> {
        let name = non_empty(&self.software_version_string);

        match self.fingerprint.software_version {
            Some(version) => Some(software_version_label(version, name)),
            None => name,
        }
    }

    /// Whether the device has a test vendor ID (see [`is_test_vendor`]).
    pub fn is_test_vendor(&self) -> bool {
        self.fingerprint.vendor_id.is_some_and(is_test_vendor)
    }
}

impl DeviceFabric {
    /// What to call the fabric: ours `matter-peek · Home`; another by its
    /// label, else by the name of its controller's vendor (`Google LLC`), else
    /// by the vendor ID.
    pub fn name(&self) -> String {
        if self.ours {
            match &self.our_name {
                Some(name) => format!("matter-peek · {name}"),
                None => "matter-peek".to_string(),
            }
        } else if !self.label.is_empty() {
            self.label.clone()
        } else {
            static_vendor_name(self.vendor_id)
                .map(ToString::to_string)
                .unwrap_or_else(|| hex_u16(self.vendor_id))
        }
    }
}

impl DeviceTime {
    /// Where the device takes its time from, in words; `None` if from no node.
    pub fn trusted_source_label(&self) -> Option<String> {
        let source = self.trusted_source.as_ref()?;

        Some(if source.ours {
            "matter-peek (us)".to_string()
        } else {
            format!(
                "node {} on fabric {}, endpoint {}",
                hex_u64(source.node_id),
                source.fabric_index,
                source.endpoint
            )
        })
    }
}

impl TimeGranularity {
    pub fn label(&self) -> &'static str {
        match self {
            Self::NoTime => "no time",
            Self::Minutes => "drifted (over 5 s off)",
            Self::Seconds => "within 5 s",
            Self::Milliseconds => "within 50 ms",
            Self::Microseconds => "within 10 µs",
        }
    }
}

impl NetworkKind {
    pub fn label(&self) -> &'static str {
        match self {
            Self::WiFi => "Wi-Fi",
            Self::Thread => "Thread",
            Self::Ethernet => "Ethernet",
            Self::Cellular => "Cellular",
            Self::Unspecified => "Unspecified",
        }
    }
}

impl NetworkStatus {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Success => "Success",
            Self::OutOfRange => "Out of range",
            Self::BoundsExceeded => "Too many networks",
            Self::NetworkIdNotFound => "Unknown network",
            Self::DuplicateNetworkId => "Network known already",
            Self::NetworkNotFound => "Network not found",
            Self::RegulatoryError => "Regulatory error",
            Self::AuthFailure => "Authentication failure",
            Self::UnsupportedSecurity => "Unsupported security",
            Self::OtherConnectionFailure => "Connection failure",
            Self::Ipv6Failed => "IPv6 failure",
            Self::IpBindFailed => "IP bind failure",
            Self::UnknownError => "Unknown error",
        }
    }
}

impl WifiSecurity {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Unspecified => "Unspecified",
            Self::None => "Open",
            Self::Wep => "WEP",
            Self::Wpa => "WPA",
            Self::Wpa2 => "WPA2",
            Self::Wpa3 => "WPA3",
        }
    }
}

impl WifiVersion {
    pub fn label(&self) -> &'static str {
        match self {
            Self::A => "802.11a",
            Self::B => "802.11b",
            Self::G => "802.11g",
            Self::N => "802.11n (Wi-Fi 4)",
            Self::Ac => "802.11ac (Wi-Fi 5)",
            Self::Ax => "802.11ax (Wi-Fi 6)",
            Self::Ah => "802.11ah (HaLow)",
        }
    }
}

impl ThreadRole {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Unspecified => "Unspecified",
            Self::Unassigned => "Detached",
            Self::SleepyEndDevice => "Sleepy end device",
            Self::EndDevice => "End device",
            Self::Reed => "Router-eligible end device",
            Self::Router => "Router",
            Self::Leader => "Leader",
        }
    }
}

/// The name of vendor `vid`, as the CSA's registry has it.
#[cfg_attr(feature = "uniffi", uniffi::export)]
pub fn vendor_name(vid: u16) -> Option<String> {
    static_vendor_name(vid).map(ToString::to_string)
}

/// The name of product `pid` of vendor `vid`, as the CSA's registry has it.
#[cfg_attr(feature = "uniffi", uniffi::export)]
pub fn product_name(vid: u16, pid: u16) -> Option<String> {
    static_product_name(vid, pid).map(ToString::to_string)
}

/// Whether `vid` is a reserved test vendor ID (`0xFFF1`–`0xFFF4`): assigned to
/// no vendor, used by development and certification devices.
#[cfg_attr(feature = "uniffi", uniffi::export)]
pub fn is_test_vendor(vid: u16) -> bool {
    (0xFFF1..=0xFFF4).contains(&vid)
}

/// Vendor `vid`, e.g. `Shelly (0x1490)`: named `name` (the device's own), else
/// as the registry has it, else just `0x1490`.
#[cfg_attr(feature = "uniffi", uniffi::export)]
pub fn vendor_label(vid: u16, name: Option<String>) -> String {
    let name = name.filter(|name| !name.is_empty());

    labelled(name.as_deref().or(static_vendor_name(vid)), hex_u16(vid))
}

/// Product `pid` of vendor `vid`, e.g. `Shelly 2PM Gen4 (0x1005)`: named `name`
/// (the device's own), else as the registry has it, else just `0x1005`.
#[cfg_attr(feature = "uniffi", uniffi::export)]
pub fn product_label(vid: u16, pid: u16, name: Option<String>) -> String {
    let name = name.filter(|name| !name.is_empty());

    labelled(
        name.as_deref().or(static_product_name(vid, pid)),
        hex_u16(pid),
    )
}

/// The name of device type `id`, as the Device Library has it.
#[cfg_attr(feature = "uniffi", uniffi::export)]
pub fn device_type_name(id: u32) -> Option<String> {
    device_type(id).map(|(_, name, _)| name.to_string())
}

/// The name of cluster `id`, as the Matter IDL has it: `OnOff`.
#[cfg_attr(feature = "uniffi", uniffi::export)]
pub fn cluster_name(id: u32) -> Option<String> {
    clusters::CLUSTERS
        .binary_search_by_key(&id, |(i, _)| *i)
        .ok()
        .map(|i| clusters::CLUSTERS[i].1.to_string())
}

/// Device type `id`, e.g. `On/Off Light (0x0100)`.
#[cfg_attr(feature = "uniffi", uniffi::export)]
pub fn device_type_label(id: u32) -> String {
    labelled(
        device_type(id).map(|(_, name, _)| *name),
        hex_u16(id as u16),
    )
}

/// Whether device type `id` is the node's plumbing (Root Node, OTA Requestor,
/// Power Source, …) rather than what the device is.
#[cfg_attr(feature = "uniffi", uniffi::export)]
pub fn is_utility_device_type(id: u32) -> bool {
    device_type(id).is_some_and(|(_, _, utility)| *utility)
}

/// Hardware version `version`, e.g. `rev-b (0x0002)`: named `name` (the
/// device's version string), else, or when the name is just the number,
/// `0x0002`.
#[cfg_attr(feature = "uniffi", uniffi::export)]
pub fn hardware_version_label(version: u16, name: Option<String>) -> String {
    versioned(version.into(), hex_u16(version), name)
}

/// Software version `version`, e.g. `1.3.0-s1 (0x01030041)`: named `name`
/// (the device's version string), else, or when the name is just the number,
/// `0x01030041`.
#[cfg_attr(feature = "uniffi", uniffi::export)]
pub fn software_version_label(version: u32, name: Option<String>) -> String {
    versioned(version, hex_u32(version), name)
}

/// An 11-digit manual pairing code in its groups, e.g. `3497-011-2332`; any
/// other code as it is.
#[cfg_attr(feature = "uniffi", uniffi::export)]
pub fn pairing_code_label(code: &str) -> String {
    if code.len() == 11 && code.bytes().all(|b| b.is_ascii_digit()) {
        format!("{}-{}-{}", &code[..4], &code[4..7], &code[7..])
    } else {
        code.to_string()
    }
}

/// A 16-bit ID, e.g. `0x1490`.
#[cfg_attr(feature = "uniffi", uniffi::export)]
pub fn hex_u16(id: u16) -> String {
    format!("0x{id:04X}")
}

/// A 32-bit number (a software version, say), e.g. `0x01030041`.
#[cfg_attr(feature = "uniffi", uniffi::export)]
pub fn hex_u32(id: u32) -> String {
    format!("0x{id:08X}")
}

/// A 64-bit ID (a node ID, say), e.g. `0x000000000001B67B`.
#[cfg_attr(feature = "uniffi", uniffi::export)]
pub fn hex_u64(id: u64) -> String {
    format!("0x{id:016X}")
}

/// A duration, e.g. `5y 2mo 4d 5h 6m 1s`, from its largest unit present down
/// to seconds (`6m 1s`, `0s`). A year is 365 days, a month 30.
#[cfg_attr(feature = "uniffi", uniffi::export)]
pub fn duration(secs: u64) -> String {
    const UNITS: [(u64, &str); 6] = [
        (365 * 24 * 3600, "y"),
        (30 * 24 * 3600, "mo"),
        (24 * 3600, "d"),
        (3600, "h"),
        (60, "m"),
        (1, "s"),
    ];

    let mut out = String::new();
    let mut rest = secs;

    for (unit, suffix) in UNITS {
        let count = rest / unit;
        rest %= unit;

        if count > 0 || !out.is_empty() || unit == 1 {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(&format!("{count}{suffix}"));
        }
    }

    out
}

/// A moment, `2026-10-23 14:05:09 UTC`, from milliseconds since the Unix
/// epoch.
#[cfg_attr(feature = "uniffi", uniffi::export)]
pub fn utc_label(unix_ms: u64) -> String {
    format!("{} UTC", date_time(unix_ms))
}

/// A local time, `2026-10-23 17:05:09`, from what a device reports for it:
/// milliseconds since the Unix epoch, as if it were UTC.
#[cfg_attr(feature = "uniffi", uniffi::export)]
pub fn local_time_label(unix_ms: u64) -> String {
    date_time(unix_ms)
}

/// An offset from UTC, e.g. `+02:00`, `-03:30`, `+00:00`.
#[cfg_attr(feature = "uniffi", uniffi::export)]
pub fn utc_offset_label(secs: i32) -> String {
    let sign = if secs < 0 { '-' } else { '+' };
    let secs = secs.unsigned_abs();

    format!("{sign}{:02}:{:02}", secs / 3600, secs / 60 % 60)
}

/// How far a clock is from ours, e.g. `in step (12 ms ahead)`, `3.2 s behind`,
/// `2h 5m 0s ahead`.
#[cfg_attr(feature = "uniffi", uniffi::export)]
pub fn clock_offset_label(offset_ms: i64) -> String {
    let side = if offset_ms < 0 { "behind" } else { "ahead" };
    let ms = offset_ms.unsigned_abs();

    if ms < 1000 {
        format!("in step ({ms} ms {side})")
    } else if ms < 120_000 {
        format!("{}.{} s {side}", ms / 1000, ms / 100 % 10)
    } else {
        format!("{} {side}", duration(ms / 1000))
    }
}

/// Where a clock takes its time from: a `TimeSourceEnum` value of the spec.
#[cfg_attr(feature = "uniffi", uniffi::export)]
pub fn time_source_label(source: u8) -> String {
    match source {
        0 => "none",
        1 => "unknown",
        2 => "set by a controller",
        3 => "another Matter node",
        4 => "SNTP, outside Matter",
        5 => "NTP, outside Matter",
        6 => "SNTP, in Matter",
        7 => "NTP, in Matter",
        8 => "NTP, in and outside Matter",
        9 => "SNTP with NTS, outside Matter",
        10 => "NTP with NTS, outside Matter",
        11 => "SNTP with NTS, in Matter",
        12 => "NTP with NTS, in Matter",
        13 => "NTP with NTS, in and outside Matter",
        14 => "a vendor cloud",
        15 => "PTP",
        16 => "GNSS",
        _ => return format!("source {source}"),
    }
    .to_string()
}

/// A bit rate, e.g. `72.2 Mbit/s`.
#[cfg_attr(feature = "uniffi", uniffi::export)]
pub fn bit_rate(bits_per_sec: u64) -> String {
    const UNITS: [(u64, &str); 3] = [
        (1_000_000_000, "Gbit/s"),
        (1_000_000, "Mbit/s"),
        (1_000, "kbit/s"),
    ];

    for (unit, suffix) in UNITS {
        if bits_per_sec >= unit {
            let tenths = bits_per_sec * 10 / unit;
            return if tenths.is_multiple_of(10) {
                format!("{} {suffix}", tenths / 10)
            } else {
                format!("{}.{} {suffix}", tenths / 10, tenths % 10)
            };
        }
    }

    format!("{bits_per_sec} bit/s")
}

/// `2026-10-23 14:05:09`, from milliseconds since the Unix epoch.
fn date_time(unix_ms: u64) -> String {
    let secs = unix_ms / 1000;
    let (year, month, day) = civil_date((secs / 86_400) as i64);
    let secs = secs % 86_400;

    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}",
        secs / 3600,
        secs / 60 % 60,
        secs % 60
    )
}

/// The date `days` after 1970-01-01 (proleptic Gregorian).
fn civil_date(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    // Months from March.
    let mp = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;

    (year_of_era + era * 400 + i64::from(month <= 2), month, day)
}

/// `name (hex)`, leaving out a name that is just the number.
fn versioned(version: u32, hex: String, name: Option<String>) -> String {
    let name = name.filter(|name| {
        !name.is_empty() && *name != version.to_string() && !name.eq_ignore_ascii_case(&hex)
    });

    labelled(name.as_deref(), hex)
}

fn non_empty(name: &str) -> Option<String> {
    Some(name.to_string()).filter(|name| !name.is_empty())
}

/// `name (id)`, or just `id`.
fn labelled(name: Option<&str>, id: String) -> String {
    match name {
        Some(name) => format!("{name} ({id})"),
        None => id,
    }
}

fn static_vendor_name(vid: u16) -> Option<&'static str> {
    if is_test_vendor(vid) {
        return Some("Test vendor");
    }

    dcl::VENDORS
        .binary_search_by_key(&vid, |(id, _)| *id)
        .ok()
        .map(|i| dcl::VENDORS[i].1)
}

fn device_type(id: u32) -> Option<&'static (u32, &'static str, bool)> {
    device_types::DEVICE_TYPES
        .binary_search_by_key(&id, |(i, _, _)| *i)
        .ok()
        .map(|i| &device_types::DEVICE_TYPES[i])
}

fn static_product_name(vid: u16, pid: u16) -> Option<&'static str> {
    dcl::PRODUCTS
        .binary_search_by_key(&(vid, pid), |(v, p, _)| (*v, *p))
        .ok()
        .map(|i| dcl::PRODUCTS[i].2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moments_and_offsets() {
        assert_eq!(utc_label(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(utc_label(946_684_800_000), "2000-01-01 00:00:00 UTC");
        assert_eq!(utc_label(1_709_210_096_999), "2024-02-29 12:34:56 UTC");
        assert_eq!(local_time_label(1_793_000_000_000), "2026-10-26 07:33:20");
        assert_eq!(utc_offset_label(7200), "+02:00");
        assert_eq!(utc_offset_label(-12_600), "-03:30");
        assert_eq!(utc_offset_label(0), "+00:00");
        assert_eq!(clock_offset_label(-12), "in step (12 ms behind)");
        assert_eq!(clock_offset_label(3_250), "3.2 s ahead");
        assert_eq!(clock_offset_label(-7_500_000), "2h 5m 0s behind");
        assert_eq!(time_source_label(2), "set by a controller");
        assert_eq!(time_source_label(99), "source 99");
    }

    #[cfg(feature = "controller")]
    #[test]
    fn cluster_names_are_the_schemas() {
        let schema: alloc::vec::Vec<_> = crate::schema::clusters()
            .iter()
            .map(|c| (c.id, c.name))
            .collect();
        assert_eq!(clusters::CLUSTERS, schema.as_slice());
    }

    #[test]
    fn durations_start_at_the_largest_unit() {
        assert_eq!(duration(0), "0s");
        assert_eq!(duration(61), "1m 1s");
        assert_eq!(duration(3600), "1h 0m 0s");
        assert_eq!(duration(90_061), "1d 1h 1m 1s");
        let secs = 5 * 365 * 86_400 + 2 * 30 * 86_400 + 4 * 86_400 + 5 * 3600 + 6 * 60 + 1;
        assert_eq!(duration(secs), "5y 2mo 4d 5h 6m 1s");
    }

    #[test]
    fn ids_are_uppercase_hex() {
        assert_eq!(hex_u16(0x1b6), "0x01B6");
        assert_eq!(hex_u64(0x1b67b), "0x000000000001B67B");
    }

    #[test]
    fn labels_prefer_the_devices_name() {
        assert_eq!(vendor_label(0x1490, None), "Shelly (0x1490)");
        assert_eq!(vendor_label(0x1490, Some("Mine".into())), "Mine (0x1490)");
        assert_eq!(vendor_label(0x1490, Some(String::new())), "Shelly (0x1490)");
        assert_eq!(vendor_label(0xFFF1, None), "Test vendor (0xFFF1)");
        assert_eq!(
            product_label(0x1490, 0x1005, None),
            "Shelly 2PM Gen3 (0x1005)"
        );
        assert_eq!(product_label(0xFFF1, 0x8001, None), "0x8001");
    }

    #[test]
    fn versions_show_the_string_then_the_number_in_hex() {
        assert_eq!(
            software_version_label(16_973_889, Some("1.3.0-s1".into())),
            "1.3.0-s1 (0x01030041)"
        );
        assert_eq!(
            software_version_label(256, Some(String::new())),
            "0x00000100"
        );
        assert_eq!(software_version_label(1, Some("1".into())), "0x00000001");
        assert_eq!(hardware_version_label(4, Some("v4".into())), "v4 (0x0004)");
        assert_eq!(hardware_version_label(1, Some("0x0001".into())), "0x0001");
    }

    #[test]
    fn pairing_codes_are_grouped() {
        assert_eq!(pairing_code_label("34970112332"), "3497-011-2332");
        assert_eq!(pairing_code_label("MT:Y.K90"), "MT:Y.K90");
    }

    #[test]
    fn the_registry_is_sorted() {
        assert!(dcl::VENDORS.windows(2).all(|w| w[0].0 < w[1].0));
        assert!(clusters::CLUSTERS.windows(2).all(|w| w[0].0 < w[1].0));
        assert!(dcl::PRODUCTS
            .windows(2)
            .all(|w| (w[0].0, w[0].1) < (w[1].0, w[1].1)));
    }
}
