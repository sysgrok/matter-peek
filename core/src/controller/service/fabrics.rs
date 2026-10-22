//! Our fabrics: listing, creating, renaming, the default, deleting.

use alloc::format;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;

use core::num::NonZeroU8;

use log::{info, warn};

use rs_matter::crypto::Crypto;
use rs_matter::persist::KvBlobStore;

use crate::api::{FabricDeletion, FabricInfo, FabricRef, PeekError};
use crate::fabrics::OurFabric;
use crate::ota::OtaImageStore;

use super::Ctx;

impl<'a, C: Crypto, S: KvBlobStore, O: OtaImageStore> Ctx<'a, C, S, O> {
    /// Our fabrics, with the devices listed on each.
    pub(super) fn fabric_infos(&self) -> Vec<FabricInfo> {
        let devices = self.listed_devices();
        let fabrics = self.fabrics.borrow();
        let default = fabrics.default_index();

        fabrics
            .iter()
            .map(|fabric| FabricInfo {
                index: fabric.fab_idx.get(),
                name: fabric.name.clone(),
                color: fabric.color,
                fabric_id: fabric.fabric_id,
                compressed_fabric_id: fabric.compressed_fabric_id,
                node_id: fabric.node_id,
                vendor_id: fabric.vendor_id,
                default: fabric.fab_idx == default,
                devices: devices
                    .iter()
                    .filter(|d| d.handle.fabric_index == fabric.fab_idx)
                    .count() as u32,
            })
            .collect()
    }

    /// The one fabric of ours `fabric` names.
    pub(super) fn our_fabric(&self, fabric: &FabricRef) -> Result<Rc<OurFabric>, PeekError> {
        let fabrics = self.fabrics.borrow();

        match fabric {
            FabricRef::Default => fabrics.get(fabrics.default_index()),
            FabricRef::All => {
                return Err(PeekError::failed(
                    "name one fabric for this, not all of them",
                ))
            }
            FabricRef::Index { index } => NonZeroU8::new(*index).and_then(|i| fabrics.get(i)),
            FabricRef::Name { name } => fabrics.named(name),
        }
        .ok_or_else(|| PeekError::failed(format!("there is no fabric {}", describe(fabric))))
    }

    /// The fabrics of ours `fabric` names: every one for [`FabricRef::All`].
    pub(super) fn our_fabrics(&self, fabric: &FabricRef) -> Result<Vec<Rc<OurFabric>>, PeekError> {
        match fabric {
            FabricRef::All => Ok(self.fabrics.borrow().iter().cloned().collect()),
            _ => Ok(alloc::vec![self.our_fabric(fabric)?]),
        }
    }

    /// Our name for fabric `fab_idx`.
    pub(super) fn fabric_name(&self, fab_idx: NonZeroU8) -> String {
        self.fabrics
            .borrow()
            .get(fab_idx)
            .map(|fabric| fabric.name.clone())
            .unwrap_or_else(|| format!("#{fab_idx}"))
    }

    pub(super) fn create_fabric(&self, name: &str) -> Result<Vec<FabricInfo>, PeekError> {
        let fabric = self
            .fabrics
            .borrow_mut()
            .create(self.crypto, self.matter, &self.store, name)
            .map_err(PeekError::failed)?;

        // Devices on it may ask us for updates, as on the others.
        if let Err(e) = crate::node::allow_ota_requestors(self.matter, &self.store, fabric.fab_idx)
        {
            warn!(
                "fabrics: cannot open our OTA Provider to the devices on {}: {e:?}",
                fabric.name
            );
        }

        Ok(self.fabric_infos())
    }

    pub(super) fn rename_fabric(
        &self,
        fabric: &FabricRef,
        name: &str,
    ) -> Result<Vec<FabricInfo>, PeekError> {
        let fab_idx = self.our_fabric(fabric)?.fab_idx;

        self.fabrics
            .borrow_mut()
            .rename(&self.store, fab_idx, name)
            .map_err(PeekError::failed)?;

        Ok(self.fabric_infos())
    }

    pub(super) fn set_default_fabric(
        &self,
        fabric: &FabricRef,
    ) -> Result<Vec<FabricInfo>, PeekError> {
        let fab_idx = self.our_fabric(fabric)?.fab_idx;

        self.fabrics
            .borrow_mut()
            .set_default(&self.store, fab_idx)
            .map_err(PeekError::failed)?;

        Ok(self.fabric_infos())
    }

    /// Delete `fabric` of ours, doing with its devices as `deletion` says.
    pub(super) async fn delete_fabric(
        &self,
        fabric: &FabricRef,
        deletion: FabricDeletion,
    ) -> Result<Vec<FabricInfo>, PeekError> {
        let fabric = self.our_fabric(fabric)?;
        if fabric.fab_idx == self.fabrics.borrow().default_index() {
            return Err(PeekError::failed(
                "the default fabric can't be deleted: make another one the default first",
            ));
        }

        let devices: Vec<u64> = self
            .listed_devices()
            .iter()
            .filter(|d| d.handle.fabric_index == fabric.fab_idx)
            .map(|d| d.handle.node_id)
            .collect();

        match deletion {
            FabricDeletion::Refuse if !devices.is_empty() => {
                return Err(PeekError::failed(format!(
                    "{} has {} device(s): decommission them, or forget them",
                    fabric.name,
                    devices.len()
                )));
            }
            FabricDeletion::Refuse => {}
            FabricDeletion::Decommission => {
                for &node_id in &devices {
                    if let Err(e) = self.decommission(node_id, false).await {
                        warn!("fabrics: decommissioning 0x{node_id:016X} failed: {e:?}");
                    }
                }
            }
            FabricDeletion::Forget => {
                for &node_id in &devices {
                    self.forget_device(node_id)?;
                }
            }
        }

        self.fabrics
            .borrow_mut()
            .delete(self.matter, &self.store, fabric.fab_idx)
            .map_err(PeekError::failed)?;
        info!(
            "fabrics: {} deleted, with {} device(s) ({deletion:?})",
            fabric.name,
            devices.len()
        );

        Ok(self.fabric_infos())
    }
}

/// How `fabric` is named, for a message.
fn describe(fabric: &FabricRef) -> String {
    match fabric {
        FabricRef::Default => "by default".into(),
        FabricRef::All => "all".into(),
        FabricRef::Index { index } => format!("#{index}"),
        FabricRef::Name { name } => format!("named {name}"),
    }
}
