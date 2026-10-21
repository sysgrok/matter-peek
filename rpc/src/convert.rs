//! Conversions from `matter-peek-core` model types to the `matter-peek-proto`
//! wire types. These live here (not in `proto`) so `proto` stays a
//! serde-only, wasm-friendly leaf with no dependency on `core`.

// Free functions (not `From` impls): both types are foreign to this crate, so
// the orphan rule forbids `impl From<core::X> for proto::Y` here. `proto` and
// `core` deliberately don't depend on each other, so this is the natural home.

use matter_peek_core as core;
use matter_peek_proto as proto;

/// Core subscription lifecycle → wire.
pub fn sub_state(s: core::subscribe::SubState) -> proto::SubscriptionState {
    match s {
        core::subscribe::SubState::Live { max_int_secs } => {
            proto::SubscriptionState::Live { max_int_secs }
        }
        core::subscribe::SubState::Offline => proto::SubscriptionState::Offline,
    }
}

fn general(g: core::GeneralDiag) -> proto::GeneralDiag {
    proto::GeneralDiag {
        up_time_secs: g.up_time_secs,
        reboot_count: g.reboot_count,
        total_operational_hours: g.total_operational_hours,
    }
}

fn software(s: core::SoftwareDiag) -> proto::SoftwareDiag {
    proto::SoftwareDiag {
        current_heap_free: s.current_heap_free,
        current_heap_used: s.current_heap_used,
        current_heap_high_watermark: s.current_heap_high_watermark,
    }
}

pub fn health(h: core::DeviceHealth) -> proto::DeviceHealth {
    proto::DeviceHealth {
        general: h.general.map(general),
        software: h.software.map(software),
    }
}

pub fn handle(h: core::DeviceHandle) -> proto::DeviceHandle {
    proto::DeviceHandle {
        fabric_index: h.fabric_index.get(),
        node_id: h.node_id,
    }
}

/// A device for the wire; `online` is whether it advertised on our fabric at the
/// last sync (`None` before the first one), `remembered` whether it has a record.
pub fn device(d: core::StoredDevice, online: Option<bool>, remembered: bool) -> proto::DeviceInfo {
    let vid = d.fingerprint.vendor_id;

    // Resolve a ready-to-show vendor label once, server-side, so the UI needs no
    // vendor table: the device's own VendorName wins; else the offline VID→name
    // lookup; else the raw hex.
    let vendor_display = if !d.vendor_name.is_empty() {
        d.vendor_name.clone()
    } else if let Some(name) = vid.and_then(core::vendors::vendor_name) {
        name.to_string()
    } else if let Some(v) = vid {
        format!("vid:0x{v:04X}")
    } else {
        "vid:—".to_string()
    };
    let is_test_vendor = vid.map(core::vendors::is_test_vendor).unwrap_or(false);

    proto::DeviceInfo {
        handle: handle(d.handle),
        unique_id: d.unique_id,
        serial_number: d.serial_number,
        node_label: d.node_label,
        vendor_name: d.vendor_name,
        product_name: d.product_name,
        vendor_display,
        is_test_vendor,
        fingerprint: proto::Fingerprint {
            vendor_id: d.fingerprint.vendor_id,
            product_id: d.fingerprint.product_id,
            hardware_version: d.fingerprint.hardware_version,
            software_version: d.fingerprint.software_version,
        },
        tags: d.tags,
        online,
        remembered,
    }
}

pub fn log_intent(intent: proto::LogIntent) -> core::LogIntent {
    match intent {
        proto::LogIntent::EndUserSupport => core::LogIntent::EndUserSupport,
        proto::LogIntent::NetworkDiag => core::LogIntent::NetworkDiag,
        proto::LogIntent::CrashLogs => core::LogIntent::CrashLogs,
    }
}

pub fn ota_image(image: core::ota::OtaImage) -> proto::OtaImageInfo {
    proto::OtaImageInfo {
        name: image.name,
        vendor_id: image.vendor_id,
        product_id: image.product_id,
        version: image.version,
        version_str: image.version_str,
        min_applicable_version: image.min_applicable_version,
        max_applicable_version: image.max_applicable_version,
        release_notes_url: image.release_notes_url,
        size: image.size,
    }
}
