//! Our fabrics: the "homes" devices are commissioned into, each with its own
//! root CA and a name we give it, one of them the default.
//!
//! rs-matter persists each fabric (our operational key, NOC, RCAC, IPK); we
//! persist its name, color and CA signing key, and which fabric is the default.
//! A store of the single-fabric versions migrates: its fabric becomes the
//! default, named [`FIRST_NAME`].

extern crate alloc;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use core::num::NonZeroU8;

use log::{info, warn};

use rs_matter::crypto::{CanonPkcSecretKey, Crypto, Rng as _};
use rs_matter::dm::clusters::decl::icd_management::OperatingModeEnum;
use rs_matter::dm::clusters::icd_mgmt::IcdAdvertisement;
use rs_matter::error::{Error, ErrorCode};
use rs_matter::persist::KvBlobStore;
use rs_matter::utils::sync::MaybeSend;
use rs_matter::Matter;

use crate::ca::{CaTier, ControllerCa};
use crate::matter::FabricIdentity;
use crate::persist::{ControllerStore, FabricRecord};

/// The name of the first fabric.
pub const FIRST_NAME: &str = "Home";

/// The longest fabric name.
pub const MAX_NAME_LEN: usize = 32;

/// The colors the front-ends tell fabrics apart by.
pub const COLORS: u8 = 8;

/// A fabric of ours.
pub struct OurFabric {
    pub fab_idx: NonZeroU8,
    pub name: String,
    pub color: u8,
    pub ca: ControllerCa,
    pub fabric_id: u64,
    pub compressed_fabric_id: u64,
    /// Our node ID on it.
    pub node_id: u64,
    pub vendor_id: u16,
    /// The public key of its root CA: a device's fabric with it is ours.
    pub root_key: Vec<u8>,
}

impl OurFabric {
    fn new(
        matter: &Matter<'_>,
        fab_idx: NonZeroU8,
        name: String,
        color: u8,
        ca: ControllerCa,
    ) -> Result<Self, Error> {
        let (fabric_id, compressed_fabric_id, node_id, vendor_id) = matter.with_state(|state| {
            let fabric = state.fabrics.fabric(fab_idx)?;
            Ok::<_, Error>((
                fabric.fabric_id(),
                fabric.compressed_fabric_id(),
                fabric.node_id(),
                fabric.vendor_id(),
            ))
        })?;
        let root_key = ca.rcac_public_key()?;

        Ok(Self {
            fab_idx,
            name,
            color,
            ca,
            fabric_id,
            compressed_fabric_id,
            node_id,
            vendor_id,
            root_key,
        })
    }

    fn record(&self) -> FabricRecord {
        FabricRecord {
            fab_idx: self.fab_idx,
            name: self.name.clone(),
            color: self.color,
            tier: self.ca.tier(),
            signing_key: CanonPkcSecretKey::new_from_ref(self.ca.signing_key()),
        }
    }
}

/// Our fabrics, and which is the default.
pub struct OurFabrics {
    fabrics: Vec<Rc<OurFabric>>,
    default: NonZeroU8,
}

impl OurFabrics {
    /// Our fabrics, as persisted; a single-fabric store's fabric, or a fresh
    /// one, as the default named [`FIRST_NAME`] otherwise.
    ///
    /// Precondition: `matter.startup` has loaded rs-matter's fabrics.
    pub fn load<C: Crypto, S: KvBlobStore + MaybeSend>(
        crypto: &C,
        matter: &Matter<'_>,
        store: &ControllerStore<'_, S>,
    ) -> Result<Self, Error> {
        if let Some((default, records)) = store.load_fabrics()? {
            let mut fabrics = Vec::new();
            for record in records {
                let chain = matter.with_state(|state| {
                    let fabric = state.fabrics.fabric(record.fab_idx).ok()?;
                    Some((fabric.root_ca().to_vec(), fabric.icac().to_vec()))
                });
                let Some((rcac, icac)) = chain else {
                    warn!(
                        "fabrics: {} (index {}) has a record but no fabric; skipped",
                        record.name, record.fab_idx
                    );
                    continue;
                };
                let ca = ControllerCa::reload(record.tier, rcac, icac, record.signing_key);
                fabrics.push(Rc::new(OurFabric::new(
                    matter,
                    record.fab_idx,
                    record.name,
                    record.color,
                    ca,
                )?));
            }

            if let Some(first) = fabrics.first().map(|fabric| fabric.fab_idx) {
                let default = NonZeroU8::new(default)
                    .filter(|default| fabrics.iter().any(|fabric| fabric.fab_idx == *default))
                    .unwrap_or(first);
                info!(
                    "fabrics: {} of ours, default index {default}",
                    fabrics.len()
                );

                return Ok(Self { fabrics, default });
            }
        }

        // A single-fabric store (its CA key on its own), or none yet.
        let (ca, fab_idx) = ControllerCa::ensure(
            crypto,
            matter,
            store,
            FabricIdentity::default(),
            CaTier::RcacDirect,
        )?;
        let fabrics = Self {
            fabrics: alloc::vec![Rc::new(OurFabric::new(
                matter,
                fab_idx,
                FIRST_NAME.to_string(),
                0,
                ca
            )?)],
            default: fab_idx,
        };
        fabrics.save(store)?;
        store.remove_ca()?;
        info!("fabrics: index {fab_idx} is {FIRST_NAME}, the default");

        Ok(fabrics)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Rc<OurFabric>> {
        self.fabrics.iter()
    }

    pub fn default_index(&self) -> NonZeroU8 {
        self.default
    }

    pub fn get(&self, fab_idx: NonZeroU8) -> Option<Rc<OurFabric>> {
        self.fabrics
            .iter()
            .find(|fabric| fabric.fab_idx == fab_idx)
            .cloned()
    }

    /// The fabric named `name`, whatever its case, or of index `name`.
    pub fn named(&self, name: &str) -> Option<Rc<OurFabric>> {
        let name = name.trim();
        let index = name.strip_prefix('#').unwrap_or(name).parse::<u8>().ok();

        self.fabrics
            .iter()
            .find(|fabric| {
                fabric.name.eq_ignore_ascii_case(name) || Some(fabric.fab_idx.get()) == index
            })
            .cloned()
    }

    /// Create a fabric named `name`, with a root CA of its own and a random
    /// fabric ID.
    pub fn create<C: Crypto, S: KvBlobStore + MaybeSend>(
        &mut self,
        crypto: &C,
        matter: &Matter<'_>,
        store: &ControllerStore<'_, S>,
        name: &str,
    ) -> Result<Rc<OurFabric>, String> {
        let name = self.valid_name(name, None)?;

        let fabric_id = loop {
            let id = crypto.rand().map_err(error)?.next_u64();
            if id != 0 {
                break id;
            }
        };
        let identity = FabricIdentity {
            fabric_id,
            ..FabricIdentity::default()
        };
        let (ca, fab_idx) = ControllerCa::bootstrap(crypto, matter, identity, CaTier::RcacDirect)
            .map_err(|e| match e.code() {
            ErrorCode::NoSpace | ErrorCode::ResourceExhausted => {
                "there is no room for another fabric".to_string()
            }
            _ => error(e),
        })?;
        matter
            .with_state(|state| store.save_fabric(state.fabrics.fabric(fab_idx)?))
            .map_err(error)?;

        let color = (0..COLORS)
            .find(|color| self.fabrics.iter().all(|fabric| fabric.color != *color))
            .unwrap_or(self.fabrics.len() as u8 % COLORS);
        let fabric = Rc::new(OurFabric::new(matter, fab_idx, name, color, ca).map_err(error)?);
        self.fabrics.push(fabric.clone());
        self.save(store).map_err(error)?;
        republish_mdns(matter);

        info!(
            "fabrics: created {} (index {fab_idx}, fabric ID 0x{fabric_id:016X})",
            fabric.name
        );

        Ok(fabric)
    }

    /// Rename fabric `fab_idx`.
    pub fn rename<S: KvBlobStore + MaybeSend>(
        &mut self,
        store: &ControllerStore<'_, S>,
        fab_idx: NonZeroU8,
        name: &str,
    ) -> Result<(), String> {
        let name = self.valid_name(name, Some(fab_idx))?;
        let position = self.position(fab_idx)?;

        let old = &self.fabrics[position];
        let renamed = OurFabric {
            name,
            ca: ControllerCa::reload(
                old.ca.tier(),
                old.ca.rcac().to_vec(),
                old.ca.icac().to_vec(),
                CanonPkcSecretKey::new_from_ref(old.ca.signing_key()),
            ),
            root_key: old.root_key.clone(),
            ..**old
        };
        self.fabrics[position] = Rc::new(renamed);

        self.save(store).map_err(error)
    }

    /// Make fabric `fab_idx` the default.
    pub fn set_default<S: KvBlobStore + MaybeSend>(
        &mut self,
        store: &ControllerStore<'_, S>,
        fab_idx: NonZeroU8,
    ) -> Result<(), String> {
        self.position(fab_idx)?;
        self.default = fab_idx;

        self.save(store).map_err(error)
    }

    /// Delete fabric `fab_idx`: our fabric, its sessions and its CA. Not the
    /// default.
    pub fn delete<S: KvBlobStore + MaybeSend>(
        &mut self,
        matter: &Matter<'_>,
        store: &ControllerStore<'_, S>,
        fab_idx: NonZeroU8,
    ) -> Result<(), String> {
        let position = self.position(fab_idx)?;
        if fab_idx == self.default {
            return Err(
                "the default fabric can't be deleted: make another one the default first".into(),
            );
        }

        matter
            .with_state(|state| state.fabrics.remove(fab_idx))
            .map_err(error)?;
        crate::matter::remove_sessions(matter, |session| {
            session.get_local_fabric_idx() == fab_idx.get()
        });
        store.remove_fabric(fab_idx).map_err(error)?;

        let fabric = self.fabrics.remove(position);
        self.save(store).map_err(error)?;
        republish_mdns(matter);

        info!("fabrics: deleted {} (index {fab_idx})", fabric.name);

        Ok(())
    }

    fn position(&self, fab_idx: NonZeroU8) -> Result<usize, String> {
        self.fabrics
            .iter()
            .position(|fabric| fabric.fab_idx == fab_idx)
            .ok_or_else(|| format!("there is no fabric #{fab_idx}"))
    }

    /// `name` trimmed, if no other fabric than `of` has it.
    fn valid_name(&self, name: &str, of: Option<NonZeroU8>) -> Result<String, String> {
        let name = name.trim();

        if name.is_empty() || name.chars().count() > MAX_NAME_LEN {
            return Err(format!("a fabric's name is 1 to {MAX_NAME_LEN} characters"));
        }
        if name == "all" || name.starts_with('#') || name.parse::<u8>().is_ok() {
            return Err(format!(
                "\"{name}\" names a choice of fabrics, not a fabric"
            ));
        }
        if self
            .fabrics
            .iter()
            .any(|fabric| Some(fabric.fab_idx) != of && fabric.name.eq_ignore_ascii_case(name))
        {
            return Err(format!("there is a fabric named {name} already"));
        }

        Ok(name.to_string())
    }

    fn save<S: KvBlobStore + MaybeSend>(
        &self,
        store: &ControllerStore<'_, S>,
    ) -> Result<(), Error> {
        let records: Vec<FabricRecord> =
            self.fabrics.iter().map(|fabric| fabric.record()).collect();

        store.save_fabrics(self.default.get(), &records)
    }
}

/// Have the mDNS responder publish our fabrics again, now that they changed.
///
/// rs-matter signals its responder only on its own changes; one of them is
/// the ICD advertisement. Setting it to another value and back, without an
/// await in between, signals the responder, which finds it as it was.
fn republish_mdns(matter: &Matter<'_>) {
    let advertisement = matter.icd_advertisement();
    let other = match advertisement {
        Some(_) => None,
        None => Some(IcdAdvertisement {
            active_threshold_ms: 0,
            slow_poll_ms: 0,
            operating_mode: Some(OperatingModeEnum::SIT),
            idle_mode_duration_s: 0,
        }),
    };

    matter.set_icd_advertisement(other);
    matter.set_icd_advertisement(advertisement);
}

fn error(e: Error) -> String {
    format!("{e:?}")
}
