//! Local persistence for the controller (world (a): fabric/creds + device
//! identity + tags).
//!
//! Built on rs-matter's `KvBlobStore` — **no SQL** (see
//! `docs/phase1-persistence.md`). Two things persist:
//!
//! - **Our controller fabric** (RCAC/NOC/IPK/operational key): saved via
//!   rs-matter's [`FabricPersist`] after commission, reloaded on startup via
//!   [`Matter::load_persist`]. A controller fabric is just a `Fabric`, so this
//!   reuses the device-side machinery wholesale — nothing bespoke.
//! - **Device records + tags**: our own [`StoredDevice`] list, serialized as a
//!   single TLV blob under a vendor-reserved key. At 50–200 devices, load-all /
//!   filter-in-memory is trivial and avoids a database entirely.
//!
//! Desktop-only for now (`DirKvBlobStore` is `std`; the rest of `core` already
//! uses `std`); the abstraction is `no_std`-friendly so it can graduate into
//! rs-matter later.

extern crate alloc;
use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use rs_matter::error::{Error, ErrorCode};
use rs_matter::fabric::FabricPersist;
use rs_matter::persist::{DirKvBlobStore, KvBlobStoreAccess, SharedKvBlobStore, VENDOR_KEYS_START};

use crate::model::{DeviceHandle, StoredDevice};

/// Scratch buffer size for the KV store (fits a fabric blob or our device list).
const KV_BUF_LEN: usize = 4096;

/// Our device-list blob lives in the vendor-reserved key range so it never
/// collides with rs-matter's own keys (fabrics, basic-info, …).
const DEVICES_KEY: u16 = VENDOR_KEYS_START;

/// The concrete shared KV store type backing the controller's persistence.
type Kv = SharedKvBlobStore<DirKvBlobStore, [u8; KV_BUF_LEN]>;

/// Owns the on-disk KV store and exposes the controller's persistence
/// operations: load/save our fabric (via rs-matter) and our device records.
pub struct ControllerStore {
    kv: Kv,
}

impl ControllerStore {
    /// Open the default on-disk store (`<tmp-dir>/rs-matter`).
    pub fn new_default() -> Self {
        Self {
            kv: SharedKvBlobStore::new(DirKvBlobStore::new_default(), [0u8; KV_BUF_LEN]),
        }
    }

    /// The shared KV, for `Matter::load_persist` (load our persisted fabric on
    /// startup) and any other rs-matter persistence.
    pub fn kv(&self) -> &Kv {
        &self.kv
    }

    /// Persist a (controller) fabric after commissioning, so CASE re-address
    /// survives a restart.
    pub fn save_fabric(&self, fabric: &rs_matter::fabric::Fabric) -> Result<(), Error> {
        FabricPersist::new(&self.kv).store(fabric)
    }

    /// Load all stored device records (empty if none persisted yet).
    ///
    /// Our device list is our own data, so we serialize it with serde/JSON
    /// rather than rs-matter's `no_std`/heapless TLV (which doesn't cover std
    /// `String`/`Vec`). Loaded via the raw `KvBlobStore`.
    pub fn load_devices(&self) -> Result<Vec<StoredDevice>, Error> {
        self.kv.access(|kv, buf| match kv.load(DEVICES_KEY, buf)? {
            Some(data) => {
                let list: StoredDeviceList =
                    serde_json::from_slice(data).map_err(|_| Error::from(ErrorCode::Invalid))?;
                Ok(list.into_model())
            }
            None => Ok(Vec::new()),
        })
    }

    /// Replace the persisted device list (load-all / save-all; trivial at this
    /// scale).
    pub fn save_devices(&self, devices: &[StoredDevice]) -> Result<(), Error> {
        let wire = StoredDeviceList::from_model(devices);
        let json = serde_json::to_vec(&wire).map_err(|_| Error::from(ErrorCode::Invalid))?;
        self.kv.access(|kv, buf| kv.store(DEVICES_KEY, &json, buf))
    }
}

// ---------------------------------------------------------------------------
// Wire (JSON) representation of the device list.
//
// `StoredDevice` (the model) uses `DeviceHandle` (a `NonZeroU8` fabric index);
// the wire form is a flat, serde-friendly mirror. Kept separate so the model
// stays transport-agnostic and free of serde derives.
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct StoredDeviceList {
    devices: Vec<StoredDeviceWire>,
}

#[derive(Serialize, Deserialize)]
struct StoredDeviceWire {
    fabric_index: u8,
    node_id: u64,
    unique_id: String,
    serial_number: String,
    node_label: String,
    tags: Vec<String>,
}

impl StoredDeviceList {
    fn from_model(devices: &[StoredDevice]) -> Self {
        Self {
            devices: devices
                .iter()
                .map(|d| StoredDeviceWire {
                    fabric_index: d.handle.fabric_index.get(),
                    node_id: d.handle.node_id,
                    unique_id: d.unique_id.clone(),
                    serial_number: d.serial_number.clone(),
                    node_label: d.node_label.clone(),
                    tags: d.tags.clone(),
                })
                .collect(),
        }
    }

    fn into_model(self) -> Vec<StoredDevice> {
        self.devices
            .into_iter()
            .filter_map(|w| {
                Some(StoredDevice {
                    handle: DeviceHandle {
                        // A persisted device always has a non-zero fabric slot;
                        // drop a corrupt 0 rather than panic.
                        fabric_index: core::num::NonZeroU8::new(w.fabric_index)?,
                        node_id: w.node_id,
                    },
                    unique_id: w.unique_id,
                    serial_number: w.serial_number,
                    node_label: w.node_label,
                    tags: w.tags,
                })
            })
            .collect()
    }
}
