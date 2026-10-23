//! The commissioned devices the network advertises, on any fabric.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use embassy_time::{with_timeout, Duration};

use futures_util::future::join_all;

use log::info;

use rs_matter::crypto::{CanonPkcPublicKeyRef, Crypto};
use rs_matter::fabric::Fabric;
use rs_matter::persist::KvBlobStore;
use rs_matter::transport::network::Address;

use crate::api::{AdvertisedDevice, AdvertisedFabric, DeviceFabric, PeekError};
use crate::ota::OtaImageStore;

use super::{failed, Ctx};

/// How long reading the fabrics of a listed device may take.
const FABRICS_READ_TIMEOUT: Duration = Duration::from_secs(10);

impl<'a, C: Crypto, S: KvBlobStore, O: OtaImageStore> Ctx<'a, C, S, O> {
    /// The commissioned devices the network advertises, on our fabrics or
    /// others', but ourselves. A device on several fabrics advertises on each
    /// from the same addresses, so its nodes are grouped by them.
    pub(super) async fn advertised_devices(&self) -> Result<Vec<AdvertisedDevice>, PeekError> {
        let nodes = self
            .controller
            .browse_operational()
            .await
            .map_err(|e| failed("cannot browse the network", e))?;

        let ours = self.our_compressed_ids();
        let known = self.known_fabrics().await;

        let mut devices: Vec<AdvertisedDevice> = Vec::new();

        for (compressed_fabric_id, node_id) in nodes {
            let our = ours.get(&compressed_fabric_id);
            if our.is_some_and(|(_, us)| *us == node_id) {
                continue;
            }

            let addresses = match self
                .controller
                .resolve_operational(compressed_fabric_id, node_id)
                .await
            {
                Ok(addrs) => ips(&addrs),
                Err(e) => {
                    // Its advertisement outlived it.
                    info!(
                        "advertised: {compressed_fabric_id:016X}-{node_id:016X} does not resolve: {e:?}"
                    );
                    continue;
                }
            };
            if addresses.is_empty() {
                continue;
            }

            let known = known.get(&compressed_fabric_id);
            let fabric = AdvertisedFabric {
                compressed_fabric_id,
                node_id,
                our_name: our.map(|(name, _)| name.clone()),
                vendor_id: known.map(|(vendor_id, _)| *vendor_id),
                label: known
                    .map(|(_, label)| label.clone())
                    .filter(|label| !label.is_empty()),
            };

            match devices
                .iter_mut()
                .find(|d| d.addresses.iter().any(|a| addresses.contains(a)))
            {
                Some(device) => {
                    for address in addresses {
                        if !device.addresses.contains(&address) {
                            device.addresses.push(address);
                        }
                    }
                    device.fabrics.push(fabric);
                }
                None => devices.push(AdvertisedDevice {
                    addresses,
                    fabrics: alloc::vec![fabric],
                }),
            }
        }

        Ok(devices)
    }

    /// Our fabrics by compressed fabric ID: our name for each, and our node
    /// ID on it.
    fn our_compressed_ids(&self) -> BTreeMap<u64, (String, u64)> {
        let fabrics = self.fabrics.borrow();

        self.matter.with_state(|state| {
            fabrics
                .iter()
                .filter_map(|ours| {
                    let fabric = state.fabrics.fabric(ours.fab_idx).ok()?;
                    Some((
                        fabric.compressed_fabric_id(),
                        (ours.name.clone(), fabric.node_id()),
                    ))
                })
                .collect()
        })
    }

    /// The fabrics our devices are on, by compressed fabric ID: the vendor of
    /// the controller that put each device there, and the fabric's label. As
    /// their live subscriptions report them, or else read from those online.
    async fn known_fabrics(&self) -> BTreeMap<u64, (u16, String)> {
        let mut lists: Vec<Vec<DeviceFabric>> = Vec::new();
        let mut reported = BTreeSet::new();

        for (node_id, watch) in self.subs.borrow().iter() {
            if !watch.fabrics().is_empty() {
                lists.push(watch.fabrics().to_vec());
                reported.insert(*node_id);
            }
        }

        let unreported: Vec<_> = {
            let online = self.online.borrow();
            self.listed_devices()
                .into_iter()
                .filter(|d| !reported.contains(&d.handle.node_id))
                .filter(|d| {
                    online
                        .as_ref()
                        .is_none_or(|online| online.contains(&d.handle.node_id))
                })
                .collect()
        };
        let reads = unreported
            .iter()
            .map(|d| with_timeout(FABRICS_READ_TIMEOUT, self.controller.read_fabrics(d.handle)));
        for health in join_all(reads).await.into_iter().flatten().flatten() {
            lists.extend(health.fabrics);
        }

        let mut known = BTreeMap::new();
        for fabric in lists.iter().flatten() {
            if let Some(compressed_fabric_id) = self.compressed_id_of(fabric) {
                known
                    .entry(compressed_fabric_id)
                    .or_insert_with(|| (fabric.vendor_id, fabric.label.clone()));
            }
        }

        known
    }

    /// The ID the devices on `fabric` advertise it by.
    fn compressed_id_of(&self, fabric: &DeviceFabric) -> Option<u64> {
        let root_key = crate::network::decode_hex(&fabric.root_key).ok()?;
        let root_key = CanonPkcPublicKeyRef::try_new(&root_key).ok()?;

        Some(Fabric::compute_compressed_fabric_id(
            self.crypto,
            root_key,
            fabric.fabric_id,
        ))
    }
}

/// The IP addresses of `addrs`, each once, in order.
fn ips(addrs: &[Address]) -> Vec<String> {
    let mut ips = Vec::new();

    for addr in addrs {
        if let Address::Udp(addr) = addr {
            let ip = addr.ip().to_string();
            if !ips.contains(&ip) {
                ips.push(ip);
            }
        }
    }

    ips
}
