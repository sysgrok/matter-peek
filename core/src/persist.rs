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
//! **`no_std`-clean**: generic over rs-matter's `KvBlobStore` trait — the
//! concrete backend (`DirKvBlobStore` on desktop; NVS/flash on embedded) is
//! injected by the binary, never baked into `core`. So the store layer is
//! portable to esp32 etc., and is a candidate to graduate into rs-matter later.

extern crate alloc;
use alloc::string::String;
use alloc::vec::Vec;

use rs_matter::error::Error;
use rs_matter::fabric::FabricPersist;
use rs_matter::persist::{
    KvBlobStore, KvBlobStoreAccess, Persist, SharedKvBlobStore, VENDOR_KEYS_START,
};
use rs_matter::tlv::{FromTLV, OctetsVec, TLVElement, ToTLV};

use crate::model::{DeviceHandle, StoredDevice};

/// Scratch buffer size for the KV store (fits a fabric blob or our device list).
const KV_BUF_LEN: usize = 4096;

/// Our blobs live in the vendor-reserved key range so they never collide with
/// rs-matter's own keys (fabrics, basic-info, …).
const DEVICES_KEY: u16 = VENDOR_KEYS_START;
/// The controller CA's signing key + tier (the one secret rs-matter's fabric
/// persistence doesn't cover).
const CA_KEY: u16 = VENDOR_KEYS_START + 1;

/// Owns a shared KV store and exposes the controller's persistence operations:
/// load/save our fabric (via rs-matter) and our device records. Generic over the
/// `KvBlobStore` backend `S` (injected by the binary).
pub struct ControllerStore<S> {
    kv: SharedKvBlobStore<S, [u8; KV_BUF_LEN]>,
}

impl<S: KvBlobStore> ControllerStore<S> {
    /// Wrap a concrete `KvBlobStore` backend (e.g. `DirKvBlobStore` on desktop).
    pub const fn new(store: S) -> Self {
        Self {
            kv: SharedKvBlobStore::new(store, [0u8; KV_BUF_LEN]),
        }
    }

    /// The shared KV, for `Matter::load_persist` (load our persisted fabric on
    /// startup) and any other rs-matter persistence.
    pub fn kv(&self) -> &SharedKvBlobStore<S, [u8; KV_BUF_LEN]> {
        &self.kv
    }

    /// Persist a (controller) fabric after commissioning, so CASE re-address
    /// survives a restart.
    pub fn save_fabric(&self, fabric: &rs_matter::fabric::Fabric) -> Result<(), Error> {
        FabricPersist::new(&self.kv).store(fabric)
    }

    /// Persist the controller CA's signing key + tier (TLV). The cert chain is
    /// *not* stored here — on reload it comes from the (rs-matter-persisted)
    /// fabric. The key bytes are exposed only transiently, straight into the TLV
    /// octet-string; they are not held in a long-lived plain buffer.
    pub fn save_ca(&self, ca: &crate::ca::ControllerCa) -> Result<(), Error> {
        let wire = CaBlob {
            icac_tier: matches!(ca.tier(), crate::ca::CaTier::Icac),
            signing_key: OctetsVec::from_vec(ca.signing_key().access().to_vec()),
        };
        Persist::new(&self.kv).store_tlv(CA_KEY, wire)
    }

    /// Load the persisted CA signing key + tier, if present. The key is returned
    /// inside a zero-on-drop [`CanonPkcSecretKey`], never a plain `[u8; N]`.
    pub fn load_ca(
        &self,
    ) -> Result<Option<(crate::ca::CaTier, rs_matter::crypto::CanonPkcSecretKey)>, Error> {
        self.kv.access(|kv, buf| match kv.load(CA_KEY, buf)? {
            Some(data) => {
                let blob = CaBlob::from_tlv(&TLVElement::new(data))?;
                let tier = if blob.icac_tier {
                    crate::ca::CaTier::Icac
                } else {
                    crate::ca::CaTier::RcacDirect
                };
                let mut key = rs_matter::crypto::CanonPkcSecretKey::new();
                key.try_load_from_slice(&blob.signing_key)?;
                Ok(Some((tier, key)))
            }
            None => Ok(None),
        })
    }

    /// Load all stored device records (empty if none persisted yet).
    ///
    /// Serialized as rs-matter TLV (using the `alloc` `String`/`Vec` TLV impls),
    /// loaded via the raw `KvBlobStore`.
    pub fn load_devices(&self) -> Result<Vec<StoredDevice>, Error> {
        self.kv.access(|kv, buf| match kv.load(DEVICES_KEY, buf)? {
            Some(data) => {
                let list = StoredDeviceList::from_tlv(&TLVElement::new(data))?;
                Ok(list.into_model())
            }
            None => Ok(Vec::new()),
        })
    }

    /// Replace the persisted device list (load-all / save-all; trivial at this
    /// scale).
    pub fn save_devices(&self, devices: &[StoredDevice]) -> Result<(), Error> {
        let wire = StoredDeviceList::from_model(devices);
        Persist::new(&self.kv).store_tlv(DEVICES_KEY, wire)
    }

    /// Replace the local tag set of the device with this `node_id`, persisting
    /// the change. Tags are normalized (trimmed, empties dropped, de-duplicated,
    /// order preserved). Returns `true` if a matching device was found and
    /// updated, `false` if no such device is stored (nothing written).
    ///
    /// Tags are controller-local metadata only — they never touch the device.
    pub fn set_tags(&self, node_id: u64, tags: &[String]) -> Result<bool, Error> {
        let mut devices = self.load_devices()?;
        let Some(dev) = devices.iter_mut().find(|d| d.handle.node_id == node_id) else {
            return Ok(false);
        };
        dev.tags = normalize_tags(tags);
        self.save_devices(&devices)?;
        Ok(true)
    }
}

/// Trim whitespace, drop empty tags, and de-duplicate (first occurrence wins),
/// preserving input order.
fn normalize_tags(tags: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in tags {
        let t = t.trim();
        if !t.is_empty() && !out.iter().any(|e| e == t) {
            out.push(String::from(t));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Wire (TLV) representations.
//
// `StoredDevice` (the model) uses `DeviceHandle` (a `NonZeroU8` fabric index);
// the wire form is a flat, TLV-friendly mirror. Kept separate so the model stays
// transport-agnostic and free of TLV derives.
// ---------------------------------------------------------------------------

/// Persisted controller-CA secret: the signing key + which tier it signs as.
#[derive(ToTLV, FromTLV)]
struct CaBlob {
    /// `true` ⇒ ICAC tier (signing key is the ICAC priv); `false` ⇒ RCAC-direct.
    icac_tier: bool,
    /// The RCAC or ICAC private key, as a TLV octet string (not a u8 array).
    signing_key: OctetsVec,
}

#[derive(ToTLV, FromTLV)]
struct StoredDeviceList {
    devices: Vec<StoredDeviceWire>,
}

#[derive(ToTLV, FromTLV)]
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
