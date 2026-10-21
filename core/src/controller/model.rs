//! The controller's own records of devices, and what it reads off them.

use core::num::NonZeroU8;

pub use crate::api::{DeviceHealth, GeneralDiag, LogIntent, SoftwareDiag};

/// Which diagnostic clusters to read in a health snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiagSet {
    pub general: bool,
    pub software: bool,
    pub thread: bool,
    pub wifi: bool,
    pub ethernet: bool,
}

impl DiagSet {
    /// Read everything the device happens to support.
    pub const ALL: Self = Self {
        general: true,
        software: true,
        thread: true,
        wifi: true,
        ethernet: true,
    };
}

/// A device the controller has commissioned and can address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceHandle {
    /// Our fabric, in the controller's fabric table. Not the device's index of
    /// it, which can differ (e.g. when the device is on other fabrics too).
    pub fabric_index: NonZeroU8,
    /// NodeID we assigned to the device on our fabric.
    pub node_id: u64,
}

/// Result of a successful commissioning, returned to the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommissionOutcome {
    pub handle: DeviceHandle,
}

/// A device on our fabric, as the controller knows it: from the local store
/// when remembered, else from this run only.
///
/// Identity (`unique_id`/`serial_number`) is read from BasicInformation at
/// commission time and is what lets us re-match a device after a factory reset
/// (its NodeID changes, but its serial/unique-id don't). `tags` are local-only
/// labels for filtering/grouping in the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredDevice {
    pub handle: DeviceHandle,
    /// BasicInformation `UniqueID` (stable across resets), if read.
    pub unique_id: alloc::string::String,
    /// BasicInformation `SerialNumber`, if read.
    pub serial_number: alloc::string::String,
    /// BasicInformation `NodeLabel` (user-set name from the primary ecosystem).
    pub node_label: alloc::string::String,
    /// BasicInformation `VendorName` — the vendor's own human-readable name.
    pub vendor_name: alloc::string::String,
    /// BasicInformation `ProductName` — the product's own human-readable name.
    pub product_name: alloc::string::String,
    /// Per-model numeric fingerprint (vendor/product/hw/sw) captured at commission
    /// — the fallback identity when no `unique_id`/`serial_number` is served, and
    /// what surfaces a "same model, maybe same unit?" re-match candidate.
    pub fingerprint: DeviceFingerprint,
    /// Local user tags for filtering/grouping.
    pub tags: alloc::vec::Vec<alloc::string::String>,
}

/// Device identity read from the BasicInformation cluster at commission time.
///
/// The spec-stable string identifiers (`UniqueID`, `SerialNumber`, `NodeLabel`)
/// are **optional** and, additionally, rs-matter's generated client doesn't emit
/// readers for `char_string` attributes — so they're often empty. As a fallback
/// we always capture a composite **fingerprint** from the *mandatory* numeric
/// attributes (`VendorID`, `ProductID`, `Hardware`/`SoftwareVersion`), which
/// every device serves. Re-matching prefers `unique_id` → `serial_number` →
/// `fingerprint`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceIdentity {
    pub unique_id: alloc::string::String,
    pub serial_number: alloc::string::String,
    pub node_label: alloc::string::String,
    /// BasicInformation `VendorName` — the vendor's own human-readable name (the
    /// authoritative source, preferred over decoding `vendor_id`). Empty if the
    /// device leaves it blank.
    pub vendor_name: alloc::string::String,
    /// BasicInformation `ProductName` — the product's own human-readable name.
    /// Empty if the device leaves it blank.
    pub product_name: alloc::string::String,
    /// Composite of the mandatory numeric BasicInformation attributes — not
    /// globally unique, but a stable per-model fingerprint and the fallback when
    /// the string identifiers are absent.
    pub fingerprint: DeviceFingerprint,
}

/// The always-available numeric BasicInformation attributes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeviceFingerprint {
    pub vendor_id: Option<u16>,
    pub product_id: Option<u16>,
    pub hardware_version: Option<u16>,
    pub software_version: Option<u32>,
}

impl DeviceFingerprint {
    /// A fingerprint is usable for *candidate* matching only when it carries
    /// enough to meaningfully pin a model — at minimum a vendor **and** product
    /// id. A device that served none of the numeric attrs (all `None`) must not
    /// match every other blank fingerprint.
    pub fn is_usable(&self) -> bool {
        self.vendor_id.is_some() && self.product_id.is_some()
    }
}

/// Outcome of matching a freshly-read [`DeviceIdentity`] against the stored
/// devices, to decide whether a (re-)commissioned device is one we already know.
///
/// See [`match_device`]. The distinction matters because the numeric fingerprint
/// is per-**model**, not per-**unit**: two identical devices share it, so a
/// fingerprint-only hit is a *suggestion* for the user to confirm — never a
/// silent merge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchResult {
    /// A per-unit identifier (`unique_id` or `serial_number`) matched an existing
    /// record — safe to auto-merge (carry over tags / label, update the handle).
    /// Carries the matched device's current `node_id`.
    Exact { node_id: u64 },
    /// Only the per-model fingerprint matched (no shared `unique_id`/`serial`).
    /// Possibly the same unit, possibly a sibling of the same model — surface it
    /// for the user to confirm; do not auto-merge. Carries the candidate's
    /// `node_id`.
    Candidate { node_id: u64 },
    /// No match — a genuinely new device.
    New,
}

/// Classify `ident` against `stored`, deciding whether it is an existing device.
///
/// Precedence (first hit wins):
/// 1. `unique_id` equal & non-empty → [`MatchResult::Exact`]
/// 2. `serial_number` equal & non-empty → [`MatchResult::Exact`]
/// 3. usable `fingerprint` equal → [`MatchResult::Candidate`]
/// 4. otherwise → [`MatchResult::New`]
///
/// Pure (no I/O) so it is unit-testable in isolation.
pub fn match_device(ident: &DeviceIdentity, stored: &[StoredDevice]) -> MatchResult {
    // 1. unique_id (per-unit, strongest).
    if !ident.unique_id.is_empty() {
        if let Some(d) = stored.iter().find(|d| d.unique_id == ident.unique_id) {
            return MatchResult::Exact {
                node_id: d.handle.node_id,
            };
        }
    }

    // 2. serial_number (per-unit).
    if !ident.serial_number.is_empty() {
        if let Some(d) = stored
            .iter()
            .find(|d| d.serial_number == ident.serial_number)
        {
            return MatchResult::Exact {
                node_id: d.handle.node_id,
            };
        }
    }

    // 3. fingerprint (per-model, suggestion only).
    if ident.fingerprint.is_usable() {
        if let Some(d) = stored.iter().find(|d| d.fingerprint == ident.fingerprint) {
            return MatchResult::Candidate {
                node_id: d.handle.node_id,
            };
        }
    }

    MatchResult::New
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    use alloc::vec;

    const N1: u64 = 1001;
    const N2: u64 = 1002;

    fn fp(vid: u16, pid: u16) -> DeviceFingerprint {
        DeviceFingerprint {
            vendor_id: Some(vid),
            product_id: Some(pid),
            hardware_version: Some(1),
            software_version: Some(1),
        }
    }

    fn stored(
        node_id: u64,
        uid: &str,
        serial: &str,
        fingerprint: DeviceFingerprint,
    ) -> StoredDevice {
        StoredDevice {
            handle: DeviceHandle {
                fabric_index: NonZeroU8::new(1).unwrap(),
                node_id,
            },
            unique_id: uid.to_string(),
            serial_number: serial.to_string(),
            node_label: "".to_string(),
            vendor_name: "".to_string(),
            product_name: "".to_string(),
            fingerprint,
            tags: vec!["keep-me".to_string()],
        }
    }

    fn ident(uid: &str, serial: &str, fingerprint: DeviceFingerprint) -> DeviceIdentity {
        DeviceIdentity {
            unique_id: uid.to_string(),
            serial_number: serial.to_string(),
            node_label: "".to_string(),
            vendor_name: "".to_string(),
            product_name: "".to_string(),
            fingerprint,
        }
    }

    #[test]
    fn empty_store_is_new() {
        let id = ident("uid-1", "sn-1", fp(1, 2));
        assert_eq!(match_device(&id, &[]), MatchResult::New);
    }

    #[test]
    fn unique_id_match_is_exact() {
        let store = vec![stored(N1, "uid-1", "sn-OLD", fp(1, 2))];
        // Same unique_id, *different* serial + fingerprint → still Exact on uid.
        let id = ident("uid-1", "sn-NEW", fp(9, 9));
        assert_eq!(
            match_device(&id, &store),
            MatchResult::Exact { node_id: N1 }
        );
    }

    #[test]
    fn serial_match_is_exact_when_no_unique_id() {
        let store = vec![stored(N1, "", "sn-1", fp(1, 2))];
        let id = ident("", "sn-1", fp(9, 9));
        assert_eq!(
            match_device(&id, &store),
            MatchResult::Exact { node_id: N1 }
        );
    }

    #[test]
    fn unique_id_takes_precedence_over_serial() {
        // uid points at N2, serial points at N1 → uid wins.
        let store = vec![
            stored(N1, "uid-other", "sn-shared", fp(1, 2)),
            stored(N2, "uid-1", "sn-other", fp(3, 4)),
        ];
        let id = ident("uid-1", "sn-shared", fp(5, 6));
        assert_eq!(
            match_device(&id, &store),
            MatchResult::Exact { node_id: N2 }
        );
    }

    #[test]
    fn fingerprint_only_is_candidate_not_exact() {
        let store = vec![stored(N1, "uid-other", "sn-other", fp(1, 2))];
        // No shared uid/serial, but same per-model fingerprint.
        let id = ident("uid-new", "sn-new", fp(1, 2));
        assert_eq!(
            match_device(&id, &store),
            MatchResult::Candidate { node_id: N1 }
        );
    }

    #[test]
    fn blank_fingerprint_never_matches() {
        // A device serving no numeric attrs must not match another blank one.
        let blank = DeviceFingerprint::default();
        let store = vec![stored(N1, "", "", blank)];
        let id = ident("", "", blank);
        assert_eq!(match_device(&id, &store), MatchResult::New);
    }

    #[test]
    fn partial_fingerprint_is_not_usable() {
        // vendor only (no product) → not enough to pin a model.
        let partial = DeviceFingerprint {
            vendor_id: Some(1),
            product_id: None,
            ..DeviceFingerprint::default()
        };
        assert!(!partial.is_usable());
        let store = vec![stored(N1, "", "", partial)];
        let id = ident("", "", partial);
        assert_eq!(match_device(&id, &store), MatchResult::New);
    }

    #[test]
    fn empty_incoming_strings_do_not_match_empty_stored() {
        // An incoming device with empty uid+serial must not match a stored device
        // that also has empty uid/serial — only the (usable) fingerprint may.
        let store = vec![stored(N1, "", "", fp(1, 2))];
        let id = ident("", "", fp(7, 8)); // different fingerprint
        assert_eq!(match_device(&id, &store), MatchResult::New);
    }
}
