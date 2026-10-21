//! The device list, and the operations on a listed device.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use embassy_time::{with_timeout, Duration, Timer};

use log::{info, warn};

use rs_matter::crypto::Crypto;
use rs_matter::error::Error;
use rs_matter::persist::KvBlobStore;

use crate::api::{DeviceHealth, DeviceList, LogIntent, PeekError, SubscriptionState};
use crate::model::{
    match_device, DeviceHandle, DeviceIdentity, DiagSet, MatchResult, StoredDevice,
};
use crate::ota::OtaImageStore;

use super::{convert, failed, Ctx};

/// How many times to try reaching the device for `RemoveFabric` before giving
/// up. A listed device is addressed over CASE, which first needs an
/// *operational mDNS resolve* — a device that is asleep, slow to answer, or
/// re-announcing itself can miss one attempt and answer the next, so a single
/// failure is not proof that it is gone.
const DECOMMISSION_ATTEMPTS: u32 = 3;

/// Pause between those attempts, to give a slow resolve room to land.
const DECOMMISSION_RETRY_DELAY_SECS: u64 = 2;

/// How long those attempts may take in all. Each address of an unreachable
/// device costs a full CASE timeout, and a cached advertisement can outlive the
/// device.
const DECOMMISSION_TIMEOUT_SECS: u64 = 30;

impl<'a, C: Crypto, S: KvBlobStore, O: OtaImageStore> Ctx<'a, C, S, O> {
    /// The listed devices, with `notice` about the operation that produced the
    /// list, if any.
    pub(super) fn list_devices(&self, notice: Option<String>) -> Result<DeviceList, PeekError> {
        let online = self.online.borrow();
        let transient = self.transient.borrow();

        let remembered = self
            .store
            .load_devices()
            .map_err(|e| failed("cannot load the devices", e))?;

        let devices = remembered
            .into_iter()
            .map(|d| (d, true))
            .chain(transient.iter().cloned().map(|d| (d, false)))
            .map(|(d, remembered)| {
                let node_id = d.handle.node_id;
                let online = online.as_ref().map(|o| o.contains(&node_id));
                convert::device(d, online, remembered)
            })
            .collect();

        Ok(DeviceList { devices, notice })
    }

    /// Re-address a listed device over CASE and read its health.
    pub(super) async fn peek_device(&self, node_id: u64) -> Result<DeviceHealth, PeekError> {
        let dev = self.lookup_device(node_id)?;

        let health = self
            .controller
            .read_health(dev.handle, DiagSet::ALL)
            .await
            .map_err(|e| failed(&format!("device 0x{node_id:016X} did not answer"), e))?;

        // It answers, so an offline subscription of it can be re-established now.
        self.set_online(node_id, true);
        if let Some(watch) = self.subs.borrow().get(&node_id) {
            if watch.state == Some(SubscriptionState::Offline) {
                watch.retry.notify();
            }
        }

        Ok(health)
    }

    /// Retrieve a diagnostic log of a listed device.
    pub(super) async fn retrieve_logs(
        &self,
        node_id: u64,
        intent: LogIntent,
    ) -> Result<Vec<u8>, PeekError> {
        let dev = self.lookup_device(node_id)?;

        self.controller
            .retrieve_logs(dev.handle, intent)
            .await
            .map_err(|e| failed("retrieving the logs failed", e))
    }

    /// Blink a listed device (Identify cluster) on its application endpoint.
    pub(super) async fn identify(&self, node_id: u64, secs: u16) -> Result<(), PeekError> {
        let dev = self.lookup_device(node_id)?;

        self.controller
            .identify(dev.handle, 1, secs)
            .await
            .map_err(|e| failed("identify failed", e))
    }

    /// Decommission a listed device: stop its subscription, `RemoveFabric` our
    /// fabric on the device (freeing its fabric slot), then drop it from the
    /// list and its record, if any.
    ///
    /// The device leaves the list even if it cannot be reached, or with `force`
    /// is not contacted at all. Such a device still holds our fabric, so a sync
    /// lists it again once it advertises.
    pub(super) async fn decommission(
        &self,
        node_id: u64,
        force: bool,
    ) -> Result<DeviceList, PeekError> {
        let dev = self.lookup_device(node_id)?;

        // Its CASE session is about to be torn down.
        self.stop_watch(node_id);

        let removed = if force {
            info!("decommission node 0x{node_id:016X}: forced - not contacting the device");
            false
        } else {
            let attempts = self.try_remove_fabric(dev.handle, node_id);
            match with_timeout(Duration::from_secs(DECOMMISSION_TIMEOUT_SECS), attempts).await {
                Ok(result) => result.is_ok(),
                Err(_) => {
                    warn!(
                        "decommission node 0x{node_id:016X}: gave up after {DECOMMISSION_TIMEOUT_SECS}s"
                    );
                    false
                }
            }
        };

        // Drop any secure session we still hold, but only if `RemoveFabric` did
        // not go through: on success, the exchange that carried it still has a
        // trailing close to send on that session.
        if !removed {
            self.controller.forget_sessions(dev.handle);
        }

        self.set_online(node_id, false);
        self.transient
            .borrow_mut()
            .retain(|d| d.handle.node_id != node_id);

        let notice = (!removed && !force).then(|| {
            format!(
                "Device 0x{node_id:016X} could not be reached, so it was only removed from the \
                 list and still holds our fabric. Once it is back online, a sync lists it again \
                 and it can be decommissioned."
            )
        });

        self.update_devices(
            |devices| devices.retain(|d| d.handle.node_id != node_id),
            notice,
        )
    }

    /// `RemoveFabric` on the device, retried up to [`DECOMMISSION_ATTEMPTS`]
    /// times.
    async fn try_remove_fabric(&self, handle: DeviceHandle, node_id: u64) -> Result<(), Error> {
        let mut attempt = 1;

        loop {
            match self.controller.decommission(handle).await {
                Ok(()) => {
                    info!(
                        "decommissioned node 0x{node_id:016X} (RemoveFabric ok, attempt {attempt})"
                    );
                    return Ok(());
                }
                Err(e) => {
                    warn!(
                        "decommission node 0x{node_id:016X}: attempt {attempt}/{DECOMMISSION_ATTEMPTS} failed: {e:?}"
                    );
                    if attempt == DECOMMISSION_ATTEMPTS {
                        return Err(e);
                    }
                }
            }

            attempt += 1;
            Timer::after(Duration::from_secs(DECOMMISSION_RETRY_DELAY_SECS)).await;
        }
    }

    /// Apply `f` to the device records, then answer with the device list.
    pub(super) fn update_devices(
        &self,
        f: impl FnOnce(&mut Vec<StoredDevice>),
        notice: Option<String>,
    ) -> Result<DeviceList, PeekError> {
        self.store
            .load_devices()
            .and_then(|mut devices| {
                f(&mut devices);
                self.store.save_devices(&devices)
            })
            .map_err(|e| failed("cannot update the store", e))?;

        self.list_devices(notice)
    }

    /// Re-sync the device list with our fabric, refreshing the identity of every
    /// reachable device, and answer with the list.
    pub(super) async fn sync(&self) -> Result<DeviceList, PeekError> {
        let notice = self
            .sync_devices(true)
            .await
            .map_err(|e| failed("sync failed", e))?;

        self.list_devices(notice)
    }

    /// Align the device list with the devices advertising on our fabric: list
    /// those not listed yet (keeping no record of them), drop those without a
    /// record that are gone and, if `refresh`, re-read the others' identity.
    /// Notes which devices are online. Returns a notice of what changed, if
    /// anything did.
    pub(super) async fn sync_devices(&self, refresh: bool) -> Result<Option<String>, Error> {
        let advertised = self.controller.browse_fabric(self.fab_idx).await?;
        info!("sync: {} device(s) on our fabric", advertised.len());

        let mut remembered = self.store.load_devices()?;
        let mut found = 0;

        for &node_id in &advertised {
            // In use on our fabric, so never to be handed out again.
            self.store.reserve_node_id(node_id)?;

            let handle = DeviceHandle {
                fabric_index: self.fab_idx,
                node_id,
            };

            let record = remembered.iter().position(|d| d.handle.node_id == node_id);
            let listed = record.is_some()
                || self
                    .transient
                    .borrow()
                    .iter()
                    .any(|d| d.handle.node_id == node_id);

            if listed && !refresh {
                continue;
            }

            let ident = self.controller.read_basic_info(handle).await;

            // An unusable fingerprint means the read failed. Only a device that
            // answers is listed: an advertisement can outlive its device (cached
            // until its TTL runs out).
            if !ident.fingerprint.is_usable() {
                if !listed {
                    warn!(
                        "sync: node 0x{node_id:016X} is advertised on our fabric but does not answer"
                    );
                }
                continue;
            }

            if let Some(i) = record {
                refresh_identity(&mut remembered[i], ident);
                self.store.save_devices(&remembered)?;
                continue;
            }

            let mut transient = self.transient.borrow_mut();
            match transient.iter_mut().find(|d| d.handle.node_id == node_id) {
                Some(device) => refresh_identity(device, ident),
                None => {
                    info!("sync: listing node 0x{node_id:016X}, found on our fabric");
                    transient.push(new_record(handle, &ident));
                    found += 1;
                }
            }
        }

        // Without a record, a device is listed only while it is on our fabric.
        self.transient
            .borrow_mut()
            .retain(|d| advertised.contains(&d.handle.node_id));

        *self.online.borrow_mut() = Some(advertised.into_iter().collect());

        Ok((found > 0)
            .then(|| format!("{found} device(s) found on our fabric were added to the list.")))
    }

    /// Keep a record of a listed device.
    pub(super) fn remember(&self, node_id: u64) -> Result<DeviceList, PeekError> {
        let position = self
            .transient
            .borrow()
            .iter()
            .position(|d| d.handle.node_id == node_id);

        let Some(i) = position else {
            // Remembered already, or not listed at all.
            self.lookup_device(node_id)?;
            return self.list_devices(None);
        };

        let device = self.transient.borrow_mut().remove(i);
        let ident = DeviceIdentity {
            unique_id: device.unique_id,
            serial_number: device.serial_number,
            node_label: device.node_label,
            vendor_name: device.vendor_name,
            product_name: device.product_name,
            fingerprint: device.fingerprint,
        };

        self.update_devices(|devices| adopt(devices, device.handle, &ident), None)
    }

    /// Replace a remembered device's local tags.
    pub(super) fn set_tags(&self, node_id: u64, tags: &[String]) -> Result<DeviceList, PeekError> {
        match self.store.set_tags(node_id, tags) {
            Ok(true) => self.list_devices(None),
            Ok(false) => {
                self.lookup_device(node_id)?;
                Err(PeekError::failed(format!(
                    "device 0x{node_id:016X} is not remembered; remember it to tag it"
                )))
            }
            Err(e) => Err(failed("cannot set the tags", e)),
        }
    }

    /// Keep a record of a freshly-commissioned device, re-matching it against
    /// the store so a re-commissioned unit reuses its existing record (and the
    /// user's tags / label) rather than spawning a duplicate.
    ///
    /// Returns `Some(existing_node_id)` when only the per-model fingerprint
    /// matched (a [`MatchResult::Candidate`]), to offer *linking* the two; a
    /// strong-id ([`MatchResult::Exact`]) merge and a brand-new device both
    /// return `None`.
    pub(super) async fn persist_commissioned(
        &self,
        handle: DeviceHandle,
    ) -> Result<Option<u64>, Error> {
        let ident = self.controller.read_basic_info(handle).await;
        let mut devices = self.store.load_devices().unwrap_or_default();
        let outcome = match_device(&ident, &devices);

        // Drop any record carrying this exact node_id (a clean re-run of the same id).
        devices.retain(|d| d.handle.node_id != handle.node_id);

        let mut candidate = None;
        match outcome {
            // The same physical unit, re-addressed: reuse its record, with the
            // user's tags and label.
            MatchResult::Exact { node_id } => {
                let prior = devices.iter().position(|d| d.handle.node_id == node_id);
                let (tags, label) = prior
                    .map(|i| {
                        let d = devices.remove(i);
                        (d.tags, d.node_label)
                    })
                    .unwrap_or_default();

                let mut record = new_record(handle, &ident);
                record.tags = tags;
                // Keep an existing user label if BasicInformation didn't serve one.
                if record.node_label.is_empty() {
                    record.node_label = label;
                }
                devices.push(record);
            }
            // Possibly the same unit, possibly a sibling of the same model: a new
            // record, and the candidate to link.
            MatchResult::Candidate { node_id } => {
                candidate = Some(node_id);
                devices.push(new_record(handle, &ident));
            }
            MatchResult::New => devices.push(new_record(handle, &ident)),
        }

        self.store.save_devices(&devices)?;
        Ok(candidate)
    }

    /// The NodeIDs of the devices we are commissioned into and mean to stay — the
    /// listed ones: the orphan sweep must not remove our fabric from those.
    pub(super) fn live_node_ids(&self) -> Vec<u64> {
        self.listed_devices()
            .iter()
            .map(|d| d.handle.node_id)
            .collect()
    }
}

/// Update a device's identity from a fresh read, keeping a label the device no
/// longer serves.
fn refresh_identity(device: &mut StoredDevice, ident: DeviceIdentity) {
    if !ident.node_label.is_empty() {
        device.node_label = ident.node_label;
    }
    device.unique_id = ident.unique_id;
    device.serial_number = ident.serial_number;
    device.vendor_name = ident.vendor_name;
    device.product_name = ident.product_name;
    device.fingerprint = ident.fingerprint;
}

/// Add a record for a device on our fabric. A record of the same unit under
/// another node ID is stale (a device holds our fabric only once), so it is
/// replaced, keeping its tags and label.
fn adopt(devices: &mut Vec<StoredDevice>, handle: DeviceHandle, ident: &DeviceIdentity) {
    let mut record = new_record(handle, ident);

    if let MatchResult::Exact { node_id } = match_device(ident, devices) {
        if let Some(i) = devices.iter().position(|d| d.handle.node_id == node_id) {
            let stale = devices.remove(i);

            record.tags = stale.tags;
            if record.node_label.is_empty() {
                record.node_label = stale.node_label;
            }
        }
    }

    devices.push(record);
}

/// A brand-new record (no tags yet) from a fresh identity read.
pub(super) fn new_record(handle: DeviceHandle, ident: &DeviceIdentity) -> StoredDevice {
    StoredDevice {
        handle,
        unique_id: ident.unique_id.clone(),
        serial_number: ident.serial_number.clone(),
        node_label: ident.node_label.clone(),
        vendor_name: ident.vendor_name.clone(),
        product_name: ident.product_name.clone(),
        fingerprint: ident.fingerprint,
        tags: Vec::new(),
    }
}
