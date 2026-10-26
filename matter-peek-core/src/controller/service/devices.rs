//! The device list, and the operations on a listed device.

use alloc::collections::BTreeSet;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use embassy_time::{with_timeout, Duration, Instant, Timer};

use log::{info, warn};

use rs_matter::crypto::Crypto;
use rs_matter::dm::clusters::decl::icd_management::FULL_CLUSTER as ICD_MANAGEMENT_CLUSTER;
use rs_matter::error::{Error, ErrorCode};
use rs_matter::persist::KvBlobStore;

use crate::api::query::{Facts, Known, Query};
use crate::api::{
    DeviceHealth, DeviceList, DevicesListener, FabricRef, LogIntent, NetworkInfo, NetworkKind,
    PeekError, SubscriptionState,
};
use crate::model::{
    match_device, DeviceDetails, DeviceHandle, DeviceIdentity, DiagSet, MatchResult, StoredDevice,
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

/// How long the border routers are not asked again for Thread network names
/// they did not have.
const THREAD_NAMES_RETRY: Duration = Duration::from_secs(60);

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
        let clients = self.store.load_icd_clients().unwrap_or_default();

        let devices = remembered
            .into_iter()
            .map(|d| (d, true))
            .chain(transient.iter().cloned().map(|d| (d, false)))
            .map(|(d, remembered)| {
                let node_id = d.handle.node_id;
                let online = online.as_ref().map(|o| o.contains(&node_id));
                let fabric = self.fabric_name(d.handle.fabric_index);
                let check_ins = clients.iter().any(|c| c.handle == d.handle);
                let awake_ms = self.awake_ms(node_id);
                let stay_awake = self.stays_awake(node_id);
                convert::device(
                    d, fabric, online, remembered, check_ins, awake_ms, stay_awake,
                )
            })
            .collect();

        Ok(DeviceList { devices, notice })
    }

    /// The listed devices on `fabric`, with `notice` as
    /// [`list_devices`](Self::list_devices).
    pub(super) fn list_devices_on(
        &self,
        fabric: &FabricRef,
        notice: Option<String>,
    ) -> Result<DeviceList, PeekError> {
        let fabrics: Vec<u8> = self
            .our_fabrics(fabric)?
            .iter()
            .map(|fabric| fabric.fab_idx.get())
            .collect();
        let mut list = self.list_devices(notice)?;

        list.devices
            .retain(|d| fabrics.contains(&d.handle.fabric_index));

        Ok(list)
    }

    /// The listed devices on `fabric` that `filter` matches, with what is known
    /// of them now: the health of those followed, and the updates.
    pub(super) fn list_matching(
        &self,
        fabric: &FabricRef,
        filter: &str,
    ) -> Result<DeviceList, PeekError> {
        let query =
            Query::parse(filter).map_err(|e| PeekError::failed(format!("the filter: {e}")))?;
        let mut list = self.list_devices_on(fabric, None)?;
        if query.is_empty() {
            return Ok(list);
        }

        let images = self.ota_images();
        let updates = self.ota_updates();
        let subs = self.subs.borrow();
        list.devices.retain(|device| {
            let node_id = device.handle.node_id;
            let known = Known {
                online: None,
                health: subs.get(&node_id).map(|watch| watch.health()),
                ota: updates
                    .iter()
                    .find(|update| update.node_id == node_id)
                    .map(|update| &update.progress),
                images: &images,
            };
            query.matches(&Facts::of(device, &known))
        });

        Ok(list)
    }

    /// Follow the device list with `listener`, which gets it now; answers the
    /// watcher's ID.
    pub(super) fn watch_devices(
        &self,
        listener: Arc<dyn DevicesListener>,
    ) -> Result<u64, PeekError> {
        // The others first, so the list told below is news to none of them.
        self.tell_devices();

        let devices = self.list_devices_on(&FabricRef::All, None)?.devices;
        listener.on_devices(devices.clone());

        let id = self.next_watcher.get();
        self.next_watcher.set(id + 1);
        self.devices_listeners.borrow_mut().push((id, listener));
        *self.devices_told.borrow_mut() = Some(devices);

        Ok(id)
    }

    /// Stop telling `watcher` the device list.
    pub(super) fn unwatch_devices(&self, watcher: u64) {
        let mut listeners = self.devices_listeners.borrow_mut();
        listeners.retain(|(id, _)| *id != watcher);
        if listeners.is_empty() {
            *self.devices_told.borrow_mut() = None;
        }
    }

    /// Tell the device list's listeners the list, if it changed since they
    /// were last told.
    pub(super) fn tell_devices(&self) {
        if self.devices_listeners.borrow().is_empty() {
            return;
        }

        let devices = match self.list_devices_on(&FabricRef::All, None) {
            Ok(list) => list.devices,
            Err(e) => {
                warn!("cannot list the devices: {e:?}");
                return;
            }
        };
        if self.devices_told.borrow().as_ref() == Some(&devices) {
            return;
        }

        let listeners = self.devices_listeners.borrow().clone();
        for (_, listener) in &listeners {
            listener.on_devices(devices.clone());
        }
        *self.devices_told.borrow_mut() = Some(devices);
    }

    /// Drop a listed device without contacting it: its subscription, its
    /// sessions and its record.
    pub(super) fn forget_device(&self, node_id: u64) -> Result<(), PeekError> {
        let dev = self.lookup_device(node_id)?;

        self.stop_watch(node_id);
        self.controller.forget_sessions(dev.handle);
        self.set_online(node_id, false);
        self.transient
            .borrow_mut()
            .retain(|d| d.handle.node_id != node_id);
        self.update_devices(
            |devices| devices.retain(|d| d.handle.node_id != node_id),
            None,
        )?;

        Ok(())
    }

    /// Re-address a listed device over CASE and read its health.
    pub(super) async fn peek_device(&self, node_id: u64) -> Result<DeviceHealth, PeekError> {
        let dev = self.lookup_device(node_id)?;

        let mut health = self
            .controller
            .read_health(dev.handle, DiagSet::ALL)
            .await
            .map_err(|e| failed(&format!("device 0x{node_id:016X} did not answer"), e))?;
        self.mark_our_fabric(node_id, &mut health);

        // It answers, so an offline subscription of it can be re-established now.
        self.set_online(node_id, true);
        if let Some(watch) = self.subs.borrow().get(&node_id) {
            if watch.state == Some(SubscriptionState::Offline) {
                watch.retry.notify();
            }
        }

        Ok(health)
    }

    /// Remove another controller's fabric from the device, by the device's
    /// index of it; answers its fabrics then. Our own is refused: that is
    /// decommissioning.
    pub(super) async fn remove_device_fabric(
        &self,
        node_id: u64,
        index: u8,
    ) -> Result<DeviceHealth, PeekError> {
        let dev = self.lookup_device(node_id)?;
        let fabrics = |e| failed("cannot read its fabrics", e);

        let mut before = self
            .controller
            .read_fabrics(dev.handle)
            .await
            .map_err(fabrics)?;
        self.mark_our_fabric(node_id, &mut before);

        let fabric = before
            .fabrics
            .iter()
            .flatten()
            .find(|fabric| fabric.index == Some(index))
            .ok_or_else(|| PeekError::failed(format!("it has no fabric #{index}")))?;
        if fabric.ours {
            return Err(PeekError::failed(
                "that is our own fabric: decommission the device instead",
            ));
        }

        self.controller
            .remove_device_fabric(dev.handle, index)
            .await
            .map_err(|e| failed("cannot remove the fabric", e))?;

        let mut after = self
            .controller
            .read_fabrics(dev.handle)
            .await
            .map_err(fabrics)?;
        self.mark_our_fabric(node_id, &mut after);

        Ok(after)
    }

    /// Mark our fabrics among those `health` has of a device: those of our
    /// root CAs, with our fabric IDs.
    pub(super) fn mark_our_fabric(&self, _node_id: u64, health: &mut DeviceHealth) {
        let Some(fabrics) = health.fabrics.as_mut() else {
            return;
        };
        let ours = self.fabrics.borrow();

        for fabric in fabrics {
            let mine = ours.iter().find(|mine| {
                mine.fabric_id == fabric.fabric_id && hex(&mine.root_key) == fabric.root_key
            });
            fabric.ours = mine.is_some();
            fabric.our_name = mine.map(|mine| mine.name.clone());
        }
    }

    /// Name the Thread networks of `info` the device knows by no name, as the
    /// border routers on the local network advertise them. Names they did not
    /// have are asked for again a minute later at the earliest.
    pub(super) async fn name_thread_networks(&self, info: &mut NetworkInfo) {
        let missing: Vec<u64> = crate::network::unnamed_thread_networks(info)
            .into_iter()
            .filter(|xpan| !self.thread_names.borrow().contains_key(xpan))
            .collect();
        let asked_lately = self
            .thread_names_missed
            .get()
            .is_some_and(|at| at.elapsed() < THREAD_NAMES_RETRY);

        if !missing.is_empty() && !asked_lately {
            let found = crate::mdns::thread_networks(&missing).await;

            let mut names = self.thread_names.borrow_mut();
            names.extend(found);
            if missing.iter().any(|xpan| !names.contains_key(xpan)) {
                self.thread_names_missed.set(Some(Instant::now()));
            }
        }

        let names = self.thread_names.borrow();
        crate::network::name_thread_networks(info, |xpan| names.get(&xpan).cloned());
    }

    /// Read the networking of a listed device, noting the kind of network it is
    /// on in its listing.
    pub(super) async fn read_network(&self, node_id: u64) -> Result<NetworkInfo, PeekError> {
        let dev = self.lookup_device(node_id)?;

        let mut info = self
            .controller
            .read_network(dev.handle)
            .await
            .map_err(|e| failed(&format!("device 0x{node_id:016X} did not answer"), e))?;

        self.set_online(node_id, true);
        self.name_thread_networks(&mut info).await;

        let kind = info
            .configs
            .iter()
            .find(|config| config.endpoint == 0)
            .and_then(|config| config.kind);
        if kind.is_some() && kind != dev.network {
            self.set_network(node_id, kind);
        }

        Ok(info)
    }

    /// Note the kind of network a listed device is on.
    fn set_network(&self, node_id: u64, kind: Option<NetworkKind>) {
        if let Some(device) = self
            .transient
            .borrow_mut()
            .iter_mut()
            .find(|d| d.handle.node_id == node_id)
        {
            device.network = kind;
            return;
        }

        if let Err(e) = self.update_record(node_id, |device| device.network = kind) {
            warn!("cannot note the network of node 0x{node_id:016X}: {e:?}");
        }
    }

    /// Note a fresh read of a listed device's identity.
    pub(super) fn refresh_listed(&self, node_id: u64, ident: DeviceIdentity) {
        if !ident.fingerprint.is_usable() {
            return;
        }

        match self.update_record(node_id, |d| refresh_identity(d, ident.clone())) {
            Ok(true) => {}
            Ok(false) => {
                if let Some(device) = self
                    .transient
                    .borrow_mut()
                    .iter_mut()
                    .find(|d| d.handle.node_id == node_id)
                {
                    refresh_identity(device, ident);
                }
            }
            Err(e) => warn!("cannot note the identity of node 0x{node_id:016X}: {e:?}"),
        }

        // Also outside of any operation: after an OTA update.
        self.tell_devices();
    }

    /// Apply `f` to the record of `node_id`; `false` if it has none.
    fn update_record(
        &self,
        node_id: u64,
        f: impl FnOnce(&mut StoredDevice),
    ) -> Result<bool, Error> {
        let mut devices = self.store.load_devices()?;
        let Some(device) = devices.iter_mut().find(|d| d.handle.node_id == node_id) else {
            return Ok(false);
        };

        f(device);
        self.store.save_devices(&devices)?;

        Ok(true)
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
            .map_err(|e| match e.code() {
                ErrorCode::ClusterNotFound | ErrorCode::EndpointNotFound => {
                    PeekError::failed("the device offers no diagnostic logs")
                }
                _ => failed("retrieving the logs failed", e),
            })
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

        // Its registrations went with our fabric.
        if removed {
            if let Err(e) = self.forget_icd_client(dev.handle) {
                warn!("decommission node 0x{node_id:016X}: {e}");
            }
        }

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
    pub(super) async fn sync(&self, fabric: &FabricRef) -> Result<DeviceList, PeekError> {
        self.our_fabrics(fabric)?;

        let notice = self
            .sync_devices(true)
            .await
            .map_err(|e| failed("sync failed", e))?;

        self.list_devices_on(fabric, notice)
    }

    /// Align the device list with the devices advertising on our fabrics: list
    /// those not listed yet (keeping no record of them), drop those without a
    /// record that are gone and, if `refresh`, re-read the others' identity.
    /// Notes which devices are online. Returns a notice of what changed, if
    /// anything did.
    ///
    /// Other operations run meanwhile, so every change is made on the list as it
    /// is then, and a device listed after the browse is left as it is.
    pub(super) async fn sync_devices(&self, refresh: bool) -> Result<Option<String>, Error> {
        let before: BTreeSet<u64> = self
            .listed_devices()
            .iter()
            .map(|d| d.handle.node_id)
            .collect();

        let fabrics: Vec<_> = self.fabrics.borrow().iter().cloned().collect();
        let mut advertised = Vec::new();
        for fabric in &fabrics {
            let nodes = self.controller.browse_fabric(fabric.fab_idx).await?;
            info!("sync: {} device(s) on {}", nodes.len(), fabric.name);
            advertised.extend(nodes.into_iter().map(|node_id| DeviceHandle {
                fabric_index: fabric.fab_idx,
                node_id,
            }));
        }

        let mut found = 0;

        for &handle in &advertised {
            let node_id = handle.node_id;
            // In use on a fabric of ours, so never to be handed out again.
            self.store.reserve_node_id(node_id)?;

            // Its commission lists it.
            let commissioning = || self.commissioned_node.get() == Some(node_id);

            // A listed device is read again on demand, or while we lack its
            // details.
            let listed = self.lookup_device(node_id);
            let known = listed
                .as_ref()
                .is_ok_and(|device| has_details(&device.details));
            let listed = listed.is_ok();
            if commissioning() || (known && !refresh) {
                continue;
            }
            // One that sleeps long would keep the sync waiting for it to wake up:
            // its subscription reads it once it does.
            if known && self.asleep(handle) {
                info!("sync: node 0x{node_id:016X} sleeps; not reading it again now");
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

            if commissioning()
                || self.update_record(node_id, |d| refresh_identity(d, ident.clone()))?
            {
                continue;
            }

            let mut transient = self.transient.borrow_mut();
            match transient.iter_mut().find(|d| d.handle.node_id == node_id) {
                Some(device) => refresh_identity(device, ident),
                // Removed during the read.
                None if listed => {}
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
            .retain(|d| !before.contains(&d.handle.node_id) || advertised.contains(&d.handle));

        let mut online = self.online.borrow_mut();
        let online = online.get_or_insert_with(BTreeSet::new);
        online.retain(|node_id| {
            !before.contains(node_id) || advertised.iter().any(|d| d.node_id == *node_id)
        });
        online.extend(advertised.iter().map(|d| d.node_id));

        Ok((found > 0)
            .then(|| format!("{found} device(s) found on our fabrics were added to the list.")))
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
            hardware_version_string: device.hardware_version_string,
            software_version_string: device.software_version_string,
            fingerprint: device.fingerprint,
            network: device.network,
            device_types: device.device_types,
            details: device.details,
        };

        self.update_devices(|devices| adopt(devices, device.handle, &ident), None)
    }

    /// Drop the record of a remembered device, and its tags, keeping it on our
    /// fabric: it stays listed, not remembered.
    pub(super) fn forget(&self, node_id: u64) -> Result<DeviceList, PeekError> {
        let mut record = None;
        self.store
            .load_devices()
            .and_then(|mut devices| {
                if let Some(i) = devices.iter().position(|d| d.handle.node_id == node_id) {
                    record = Some(devices.remove(i));
                    self.store.save_devices(&devices)?;
                }
                Ok(())
            })
            .map_err(|e| failed("cannot update the store", e))?;

        let Some(mut device) = record else {
            // Not remembered, or not listed at all.
            self.lookup_device(node_id)?;
            return self.list_devices(None);
        };

        device.tags.clear();
        self.transient.borrow_mut().push(device);
        self.tell_devices();

        self.list_devices(None)
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

/// `bytes` in uppercase hex, as [`DeviceFabric::root_key`](crate::api::DeviceFabric) has them.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect()
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
    device.hardware_version_string = ident.hardware_version_string;
    device.software_version_string = ident.software_version_string;
    device.fingerprint = ident.fingerprint;
    if ident.network.is_some() {
        device.network = ident.network;
    }
    if !ident.device_types.is_empty() {
        device.device_types = ident.device_types;
    }
    if !ident.details.clusters.is_empty() {
        device.details = ident.details;
    }
}

/// Whether `details` are all there: read, and with the ICD details of an ICD
/// (a record from before they were read has only its mode).
fn has_details(details: &DeviceDetails) -> bool {
    !details.clusters.is_empty()
        && (!details.clusters.contains(&ICD_MANAGEMENT_CLUSTER.id)
            || details
                .icd
                .as_ref()
                .is_some_and(|icd| icd.idle_mode_secs.is_some()))
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
        hardware_version_string: ident.hardware_version_string.clone(),
        software_version_string: ident.software_version_string.clone(),
        fingerprint: ident.fingerprint,
        tags: Vec::new(),
        network: ident.network,
        device_types: ident.device_types.clone(),
        details: ident.details.clone(),
    }
}
