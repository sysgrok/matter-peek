//! Local persistence for the controller (world (a): fabric/creds + device
//! identity + tags).
//!
//! Built on rs-matter's `KvBlobStore` — **no SQL**. Two things persist:
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
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use rs_matter::error::Error;
use rs_matter::fabric::FabricPersist;
use rs_matter::persist::{
    KvBlobStore, KvBlobStoreAccess, Persist, SharedKvBlobStore, VENDOR_KEYS_START,
};
use rs_matter::tlv::{FromTLV, OctetsVec, TLVElement, ToTLV};
use rs_matter::utils::cell::RefCell;
use rs_matter::utils::sync::blocking::Mutex;

use crate::api::agent::{ModelConfig, Think};
use crate::model::{DeviceHandle, NetworkKind, StoredDevice};

/// Scratch buffer size for the KV store (fits a fabric blob or our device list).
const KV_BUF_LEN: usize = 4096;

/// The scratch buffer a [`ControllerStore`] lends to its [`SharedKvBlobStore`].
///
/// rs-matter's `SharedKvBlobStore` borrows its buffer (`&'a Mutex<RefCell<…>>`)
/// rather than owning it, so the buffer must outlive the store. Callers own one
/// of these (e.g. as a `static`/stack value) and hand it to [`ControllerStore`].
pub type KvBuf = Mutex<RefCell<[u8; KV_BUF_LEN]>>;

/// Allocate a fresh, zeroed [`KvBuf`] for a [`ControllerStore`].
pub const fn kv_buf() -> KvBuf {
    Mutex::new(RefCell::new([0u8; KV_BUF_LEN]))
}

/// Our blobs live in the vendor-reserved key range so they never collide with
/// rs-matter's own keys (fabrics, basic-info, …).
const DEVICES_KEY: u16 = VENDOR_KEYS_START;
/// The controller CA's signing key + tier (the one secret rs-matter's fabric
/// persistence doesn't cover).
const CA_KEY: u16 = VENDOR_KEYS_START + 1;
/// The next device node ID to hand out.
const NEXT_NODE_ID_KEY: u16 = VENDOR_KEYS_START + 2;
/// The assistant's model, with its API key.
const ASSISTANT_KEY: u16 = VENDOR_KEYS_START + 3;

/// Owns a shared KV store and exposes the controller's persistence operations:
/// load/save our fabric (via rs-matter) and our device records. Generic over the
/// `KvBlobStore` backend `S` (injected by the binary).
///
/// rs-matter's [`SharedKvBlobStore`] borrows its scratch buffer, so a
/// `ControllerStore` is parameterized by the buffer's lifetime `'a` and holds a
/// borrow of a caller-owned [`KvBuf`] (see [`kv_buf`]).
pub struct ControllerStore<'a, S> {
    kv: SharedKvBlobStore<'a, S, KV_BUF_LEN>,
}

impl<'a, S: KvBlobStore> ControllerStore<'a, S> {
    /// Wrap a concrete `KvBlobStore` backend (e.g. `DirKvBlobStore` on desktop),
    /// borrowing a caller-owned scratch buffer (`buf`, from [`kv_buf`]).
    pub const fn new(store: S, buf: &'a KvBuf) -> Self {
        Self {
            kv: SharedKvBlobStore::new(store, buf),
        }
    }

    /// The shared KV, for `Matter::load_persist` (load our persisted fabric on
    /// startup) and any other rs-matter persistence.
    pub fn kv(&self) -> &SharedKvBlobStore<'a, S, KV_BUF_LEN> {
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

    /// Hand out a node ID for a new device: never one handed out before, nor one
    /// a stored device has, and at least `first`.
    ///
    /// A node ID must not be reused even once its record is gone: the device may
    /// still hold our fabric under it.
    pub fn allocate_node_id(&self, first: u64) -> Result<u64, Error> {
        let stored = self
            .load_devices()?
            .iter()
            .map(|d| d.handle.node_id + 1)
            .max();
        let node_id = self
            .load_next_node_id()?
            .into_iter()
            .chain(stored)
            .fold(first, u64::max);

        self.save_next_node_id(node_id + 1)?;

        Ok(node_id)
    }

    /// Never hand out `node_id` (seen in use on our fabric) as a new one.
    pub fn reserve_node_id(&self, node_id: u64) -> Result<(), Error> {
        if self.load_next_node_id()?.is_some_and(|next| next > node_id) {
            return Ok(());
        }

        self.save_next_node_id(node_id + 1)
    }

    fn load_next_node_id(&self) -> Result<Option<u64>, Error> {
        self.kv
            .access(|kv, buf| match kv.load(NEXT_NODE_ID_KEY, buf)? {
                Some(data) => Ok(Some(NextNodeId::from_tlv(&TLVElement::new(data))?.node_id)),
                None => Ok(None),
            })
    }

    fn save_next_node_id(&self, node_id: u64) -> Result<(), Error> {
        Persist::new(&self.kv).store_tlv(NEXT_NODE_ID_KEY, NextNodeId { node_id })
    }

    /// The assistant's model, if one is set.
    pub fn load_assistant(&self) -> Result<Option<ModelConfig>, Error> {
        self.kv
            .access(|kv, buf| match kv.load(ASSISTANT_KEY, buf)? {
                Some(data) => Ok(Some(
                    AssistantBlob::from_tlv(&TLVElement::new(data))?.into_model(),
                )),
                None => Ok(None),
            })
    }

    /// Keep `model` as the assistant's; `None` forgets it, and its key.
    pub fn save_assistant(&self, model: Option<&ModelConfig>) -> Result<(), Error> {
        match model {
            Some(model) => {
                Persist::new(&self.kv).store_tlv(ASSISTANT_KEY, AssistantBlob::from_model(model))
            }
            None => self.kv.access(|kv, buf| kv.remove(ASSISTANT_KEY, buf)),
        }
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

/// How a [`NetworkKind`] is stored.
fn network_code(kind: NetworkKind) -> u8 {
    match kind {
        NetworkKind::Unspecified => 0,
        NetworkKind::WiFi => 1,
        NetworkKind::Thread => 2,
        NetworkKind::Ethernet => 3,
        NetworkKind::Cellular => 4,
    }
}

/// The [`NetworkKind`] stored as `code`.
fn network_kind(code: u8) -> Option<NetworkKind> {
    match code {
        0 => Some(NetworkKind::Unspecified),
        1 => Some(NetworkKind::WiFi),
        2 => Some(NetworkKind::Thread),
        3 => Some(NetworkKind::Ethernet),
        4 => Some(NetworkKind::Cellular),
        _ => None,
    }
}

/// `None` for an empty string, `Some(clone)` otherwise — so blank device names
/// don't bloat the persisted blob and round-trip cleanly to/from empty.
fn opt_str(s: &str) -> Option<String> {
    (!s.is_empty()).then(|| s.to_string())
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

/// Persisted assistant model.
#[derive(ToTLV, FromTLV)]
struct AssistantBlob {
    url: String,
    model: String,
    api_key: Option<String>,
    think: bool,
}

impl AssistantBlob {
    fn from_model(model: &ModelConfig) -> Self {
        Self {
            url: model.url.clone(),
            model: model.model.clone(),
            api_key: model.api_key.clone(),
            think: model.think == Think::Default,
        }
    }

    fn into_model(self) -> ModelConfig {
        ModelConfig {
            url: self.url,
            model: self.model,
            api_key: self.api_key,
            think: Think::from_bool(self.think),
        }
    }
}

/// Persisted next device node ID.
#[derive(ToTLV, FromTLV)]
struct NextNodeId {
    node_id: u64,
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
    // Numeric fingerprint (per-model), for re-match suggestions + display.
    fp_vendor_id: Option<u16>,
    fp_product_id: Option<u16>,
    fp_hardware_version: Option<u16>,
    fp_software_version: Option<u32>,
    tags: Vec<String>,
    // Device-declared human-readable names (BasicInformation). Appended LAST and
    // `Option` so blobs written before this field existed still deserialize
    // (a missing trailing optional decodes to `None`). `None` ⇒ empty string.
    vendor_name: Option<String>,
    product_name: Option<String>,
    // Set by earlier versions on a device that was unreachable when
    // decommissioned. Never written; such a record is dropped on load, and a
    // sync lists the device again if it still holds our fabric.
    pending_removal: Option<bool>,
    // The kind of network the device is on (see `network_code`); appended last,
    // so earlier blobs decode it as `None`.
    network: Option<u8>,
    // BasicInformation version strings; appended last, as `network`.
    hardware_version_string: Option<String>,
    software_version_string: Option<String>,
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
                    fp_vendor_id: d.fingerprint.vendor_id,
                    fp_product_id: d.fingerprint.product_id,
                    fp_hardware_version: d.fingerprint.hardware_version,
                    fp_software_version: d.fingerprint.software_version,
                    tags: d.tags.clone(),
                    // Persist the name only when non-empty (keeps blobs compact
                    // and round-trips `None` ⇄ empty string).
                    vendor_name: opt_str(&d.vendor_name),
                    product_name: opt_str(&d.product_name),
                    pending_removal: None,
                    network: d.network.map(network_code),
                    hardware_version_string: opt_str(&d.hardware_version_string),
                    software_version_string: opt_str(&d.software_version_string),
                })
                .collect(),
        }
    }

    fn into_model(self) -> Vec<StoredDevice> {
        self.devices
            .into_iter()
            .filter(|w| w.pending_removal != Some(true))
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
                    vendor_name: w.vendor_name.unwrap_or_default(),
                    product_name: w.product_name.unwrap_or_default(),
                    hardware_version_string: w.hardware_version_string.unwrap_or_default(),
                    software_version_string: w.software_version_string.unwrap_or_default(),
                    fingerprint: crate::model::DeviceFingerprint {
                        vendor_id: w.fp_vendor_id,
                        product_id: w.fp_product_id,
                        hardware_version: w.fp_hardware_version,
                        software_version: w.fp_software_version,
                    },
                    tags: w.tags,
                    network: w.network.and_then(network_kind),
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DeviceFingerprint, DeviceHandle, NetworkKind};
    use alloc::collections::BTreeMap;
    use alloc::string::ToString;
    use alloc::vec;
    use core::num::NonZeroU8;

    /// An in-memory [`KvBlobStore`] for tests — a `BTreeMap<u16, Vec<u8>>`. No
    /// filesystem, no device; exercises the real `ControllerStore` persistence
    /// logic (TLV serde + load/save/upsert) in isolation.
    #[derive(Default)]
    struct MemKv {
        blobs: BTreeMap<u16, Vec<u8>>,
    }

    impl KvBlobStore for MemKv {
        fn load<'a>(&mut self, key: u16, buf: &'a mut [u8]) -> Result<Option<&'a [u8]>, Error> {
            match self.blobs.get(&key) {
                Some(data) => {
                    buf[..data.len()].copy_from_slice(data);
                    Ok(Some(&buf[..data.len()]))
                }
                None => Ok(None),
            }
        }

        fn store(&mut self, key: u16, data: &[u8], _buf: &mut [u8]) -> Result<(), Error> {
            self.blobs.insert(key, data.to_vec());
            Ok(())
        }

        fn remove(&mut self, key: u16, _buf: &mut [u8]) -> Result<(), Error> {
            self.blobs.remove(&key);
            Ok(())
        }
    }

    /// A `ControllerStore` over an in-memory KV. The scratch buffer is leaked so
    /// the returned store can own a `'static` borrow of it without the test
    /// having to juggle a separate binding (cheap, test-only).
    fn store() -> ControllerStore<'static, MemKv> {
        let buf: &'static KvBuf = alloc::boxed::Box::leak(alloc::boxed::Box::new(kv_buf()));
        ControllerStore::new(MemKv::default(), buf)
    }

    fn fp(vid: u16, pid: u16) -> DeviceFingerprint {
        DeviceFingerprint {
            vendor_id: Some(vid),
            product_id: Some(pid),
            hardware_version: Some(3),
            software_version: Some(4),
        }
    }

    fn device(node_id: u64, serial: &str, tags: &[&str]) -> StoredDevice {
        StoredDevice {
            handle: DeviceHandle {
                fabric_index: NonZeroU8::new(1).unwrap(),
                node_id,
            },
            unique_id: "uid-".to_string() + serial,
            serial_number: serial.to_string(),
            node_label: "label".to_string(),
            vendor_name: "Acme".to_string(),
            product_name: "Widget".to_string(),
            hardware_version_string: "rev-b".to_string(),
            software_version_string: "1.2.3".to_string(),
            fingerprint: fp(0x1234, 0x5678),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            network: Some(NetworkKind::Thread),
        }
    }

    #[test]
    fn load_devices_empty_when_nothing_stored() {
        let s = store();
        assert!(s.load_devices().unwrap().is_empty());
    }

    #[test]
    fn save_then_load_round_trips_all_fields() {
        let s = store();
        let devices = vec![
            device(101, "sn-a", &["kitchen", "hue"]),
            device(102, "sn-b", &[]),
        ];
        s.save_devices(&devices).unwrap();

        let loaded = s.load_devices().unwrap();
        // Full equality: handle, all identity strings, fingerprint, and tags must
        // survive the TLV round-trip exactly.
        assert_eq!(loaded, devices);
    }

    #[test]
    fn save_replaces_the_whole_list() {
        let s = store();
        s.save_devices(&[device(101, "sn-a", &["t1"])]).unwrap();
        s.save_devices(&[device(202, "sn-z", &["t2"])]).unwrap();

        let loaded = s.load_devices().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].handle.node_id, 202);
    }

    #[test]
    fn set_tags_updates_matching_device() {
        let s = store();
        s.save_devices(&[
            device(101, "sn-a", &["old"]),
            device(102, "sn-b", &["keep"]),
        ])
        .unwrap();

        let found = s
            .set_tags(101, &["new1".to_string(), "new2".to_string()])
            .unwrap();
        assert!(found);

        let loaded = s.load_devices().unwrap();
        let d101 = loaded.iter().find(|d| d.handle.node_id == 101).unwrap();
        let d102 = loaded.iter().find(|d| d.handle.node_id == 102).unwrap();
        assert_eq!(d101.tags, vec!["new1".to_string(), "new2".to_string()]);
        // The other device is untouched.
        assert_eq!(d102.tags, vec!["keep".to_string()]);
    }

    #[test]
    fn set_tags_normalizes_trim_dedup_drop_empty() {
        let s = store();
        s.save_devices(&[device(101, "sn-a", &[])]).unwrap();

        // "  a ", "a" (dup), "" (empty), "b" → ["a", "b"]
        let raw = vec![
            "  a ".to_string(),
            "a".to_string(),
            "".to_string(),
            "b".to_string(),
        ];
        assert!(s.set_tags(101, &raw).unwrap());

        let loaded = s.load_devices().unwrap();
        assert_eq!(loaded[0].tags, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn set_tags_returns_false_for_unknown_node() {
        let s = store();
        s.save_devices(&[device(101, "sn-a", &["t"])]).unwrap();

        assert!(!s.set_tags(999, &["x".to_string()]).unwrap());
        // Nothing changed.
        let loaded = s.load_devices().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].tags, vec!["t".to_string()]);
    }

    #[test]
    fn fingerprint_survives_round_trip_including_none_fields() {
        let s = store();
        let mut d = device(101, "sn-a", &[]);
        // A partial fingerprint (some None) must round-trip faithfully.
        d.fingerprint = DeviceFingerprint {
            vendor_id: Some(9),
            product_id: None,
            hardware_version: None,
            software_version: Some(42),
        };
        s.save_devices(&[d.clone()]).unwrap();

        let loaded = s.load_devices().unwrap();
        assert_eq!(loaded[0].fingerprint, d.fingerprint);
    }

    #[test]
    fn records_pending_removal_are_dropped_on_load() {
        let s = store();
        let mut wire =
            StoredDeviceList::from_model(&[device(101, "sn-a", &[]), device(102, "sn-b", &[])]);
        wire.devices[0].pending_removal = Some(true);
        Persist::new(&s.kv).store_tlv(DEVICES_KEY, wire).unwrap();

        let loaded = s.load_devices().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].handle.node_id, 102);
    }

    #[test]
    fn node_ids_are_never_handed_out_twice() {
        let s = store();
        assert_eq!(s.allocate_node_id(100).unwrap(), 100);
        assert_eq!(s.allocate_node_id(100).unwrap(), 101);

        // Above any stored device, even one not handed out by us.
        s.save_devices(&[device(200, "sn-a", &[])]).unwrap();
        assert_eq!(s.allocate_node_id(100).unwrap(), 201);

        // Not reused once the record is gone.
        s.save_devices(&[]).unwrap();
        assert_eq!(s.allocate_node_id(100).unwrap(), 202);

        // Nor once seen in use on our fabric.
        s.reserve_node_id(300).unwrap();
        s.reserve_node_id(250).unwrap();
        assert_eq!(s.allocate_node_id(100).unwrap(), 301);
    }

    #[test]
    fn assistant_model_round_trips_and_is_forgotten() {
        let store = store();
        assert_eq!(store.load_assistant().unwrap(), None);

        let model = ModelConfig {
            url: "http://localhost:11434/v1".into(),
            model: "qwen3.5:9b".into(),
            api_key: Some("sk-secret".into()),
            think: Think::Off,
        };
        store.save_assistant(Some(&model)).unwrap();
        assert_eq!(store.load_assistant().unwrap(), Some(model));

        store.save_assistant(None).unwrap();
        assert_eq!(store.load_assistant().unwrap(), None);
    }
}
