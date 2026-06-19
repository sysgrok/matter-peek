//! Conversions from `matter-peek-core` model types to the `matter-peek-proto`
//! wire types. These live here (not in `proto`) so `proto` stays a
//! serde-only, wasm-friendly leaf with no dependency on `core`.

// Free functions (not `From` impls): both types are foreign to this crate, so
// the orphan rule forbids `impl From<core::X> for proto::Y` here. `proto` and
// `core` deliberately don't depend on each other, so this is the natural home.

use matter_peek_core as core;
use matter_peek_proto as proto;

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

pub fn stored_device(d: core::StoredDevice) -> proto::StoredDeviceInfo {
    proto::StoredDeviceInfo {
        handle: handle(d.handle),
        unique_id: d.unique_id,
        serial_number: d.serial_number,
        node_label: d.node_label,
        fingerprint: proto::Fingerprint {
            vendor_id: d.fingerprint.vendor_id,
            product_id: d.fingerprint.product_id,
            hardware_version: d.fingerprint.hardware_version,
            software_version: d.fingerprint.software_version,
        },
        tags: d.tags,
    }
}

pub fn policy(p: proto::FabricPolicy) -> core::FabricPolicy {
    match p {
        proto::FabricPolicy::Persistent => core::FabricPolicy::Persistent,
        proto::FabricPolicy::Ephemeral => core::FabricPolicy::Ephemeral,
    }
}
