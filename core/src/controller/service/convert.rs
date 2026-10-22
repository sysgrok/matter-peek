//! From the controller's own records to the API types.

use crate::api;
use crate::model::{DeviceHandle, StoredDevice};
use crate::ota::OtaImage;
use crate::subscribe::SubState;

pub(super) fn sub_state(state: SubState) -> api::SubscriptionState {
    match state {
        SubState::Live { max_int_secs } => api::SubscriptionState::Live { max_int_secs },
        SubState::Offline => api::SubscriptionState::Offline,
    }
}

pub(super) fn handle(handle: DeviceHandle) -> api::DeviceHandle {
    api::DeviceHandle {
        fabric_index: handle.fabric_index.get(),
        node_id: handle.node_id,
    }
}

/// A listed device; `online` is whether it is reachable as far as we know
/// (`None` before the first sync), `remembered` whether it has a record.
pub(super) fn device(d: StoredDevice, online: Option<bool>, remembered: bool) -> api::DeviceInfo {
    api::DeviceInfo {
        handle: handle(d.handle),
        unique_id: d.unique_id,
        serial_number: d.serial_number,
        node_label: d.node_label,
        vendor_name: d.vendor_name,
        product_name: d.product_name,
        hardware_version_string: d.hardware_version_string,
        software_version_string: d.software_version_string,
        fingerprint: api::Fingerprint {
            vendor_id: d.fingerprint.vendor_id,
            product_id: d.fingerprint.product_id,
            hardware_version: d.fingerprint.hardware_version,
            software_version: d.fingerprint.software_version,
        },
        tags: d.tags,
        online,
        remembered,
        network: d.network,
    }
}

pub(super) fn ota_image(image: OtaImage) -> api::OtaImageInfo {
    api::OtaImageInfo {
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
