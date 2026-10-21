//! How the API's values are shown to people, so that every front-end (the web
//! UI, the CLI, the MCP tools, the apps) shows them alike:
//!
//! - IDs in hex: `0x` and uppercase digits, zero-padded (`0x1490`,
//!   `0x000000000001B67B`);
//! - vendors and products by name, with the ID in brackets (`Shelly (0x1490)`),
//!   named by the device, else by the CSA's registry (the DCL);
//! - durations in units, from the largest one present (`2mo 4d 0h 6m 1s`).

use alloc::format;
use alloc::string::{String, ToString};

use super::types::{DeviceInfo, NetworkKind, NetworkStatus, ThreadRole, WifiSecurity, WifiVersion};

mod dcl;

impl DeviceInfo {
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

    /// Whether the device has a test vendor ID (see [`is_test_vendor`]).
    pub fn is_test_vendor(&self) -> bool {
        self.fingerprint.vendor_id.is_some_and(is_test_vendor)
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

/// A 16-bit ID, e.g. `0x1490`.
#[cfg_attr(feature = "uniffi", uniffi::export)]
pub fn hex_u16(id: u16) -> String {
    format!("0x{id:04X}")
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
    fn the_registry_is_sorted() {
        assert!(dcl::VENDORS.windows(2).all(|w| w[0].0 < w[1].0));
        assert!(dcl::PRODUCTS
            .windows(2)
            .all(|w| (w[0].0, w[0].1) < (w[1].0, w[1].1)));
    }
}
