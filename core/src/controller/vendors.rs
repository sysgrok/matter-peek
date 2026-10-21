//! An **offline** Matter Vendor ID → name lookup.
//!
//! Vendor IDs are centrally assigned by the Connectivity Standards Alliance
//! (CSA) and published in its Distributed Compliance Ledger (DCL). A field /
//! installer tool wants this resolution to work with no connectivity, so we bake
//! in a small curated table of the vendors most likely to be encountered, plus
//! the well-known reserved / test ranges.
//!
//! This is a **fallback only**: the authoritative human-readable vendor name is
//! the device's own `BasicInformation.VendorName` attribute (which
//! [`read_basic_info`](crate::basic_info::read_basic_info) reads). This table is
//! consulted just when a device leaves that attribute blank, and to label the
//! reserved test range so a technician instantly sees a non-production device.
//!
//! Not exhaustive — the full DCL has thousands of entries. Extend
//! [`VENDORS`] as needed; unknown VIDs simply return `None` and the UI falls
//! back to the raw hex.

/// A reserved test Vendor ID (`0xFFF1`–`0xFFF4`): assigned to no real vendor and
/// used for development / certification test devices. A device advertising one
/// of these is **not** a certified production device.
pub fn is_test_vendor(vid: u16) -> bool {
    (0xFFF1..=0xFFF4).contains(&vid)
}

/// Best-effort human-readable name for a Vendor ID, offline.
///
/// Returns the CSA-registered vendor name for the well-known VIDs in [`VENDORS`],
/// a `"Test Vendor N"` label for the reserved test range, and `None` for VIDs not
/// in the table (the caller then shows the raw hex).
pub fn vendor_name(vid: u16) -> Option<&'static str> {
    match vid {
        0x0000 => return Some("Reserved (0x0000)"),
        0xFFF1 => return Some("Test Vendor 1"),
        0xFFF2 => return Some("Test Vendor 2"),
        0xFFF3 => return Some("Test Vendor 3"),
        0xFFF4 => return Some("Test Vendor 4"),
        0xFFFF => return Some("Not specified (0xFFFF)"),
        _ => {}
    }
    VENDORS
        .iter()
        .find(|(id, _)| *id == vid)
        .map(|(_, name)| *name)
}

/// Vendor IDs → names, restricted to values **verified against an authoritative
/// source** (CHIP's `CHIPVendorIdentifiers.hpp`). Deliberately conservative: a
/// diagnostic tool must not show a *wrong* vendor name, so an unverified VID
/// returns `None` and the caller shows the raw hex rather than a guess. Extend
/// from the CSA DCL (`on.dcl.csa-iot.org`) as entries are confirmed.
const VENDORS: &[(u16, &str)] = &[(0x1349, "Apple"), (0x6006, "Google")];
