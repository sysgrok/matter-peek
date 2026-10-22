//! Changing the networks of a listed device, one fail-safe per change.

use alloc::format;

use embassy_time::{with_timeout, Duration, Instant, Timer};

use log::{info, warn};

use rs_matter::crypto::Crypto;
use rs_matter::dm::clusters::net_comm::WirelessCreds;
use rs_matter::persist::KvBlobStore;

use crate::api::{
    NetworkChange, NetworkConfig, NetworkCredentials, NetworkInfo, NetworkKind, PeekError,
};
use crate::model::DeviceHandle;
use crate::netconfig::Target;
use crate::network::decode_hex;
use crate::ota::OtaImageStore;

use super::{failed, Ctx};

/// The fail-safe of a change that leaves the device on its network.
const FAIL_SAFE_SECS: u16 = 60;

/// The fail-safe of a change that reconnects the device, beyond its own connect
/// time: for its answer, finding it on its network again, and CASE.
const RECONNECT_MARGIN_SECS: u16 = 120;

/// The connect time of a device that does not tell its own.
const DEFAULT_CONNECT_SECS: u16 = 30;

/// The scan time of a device that does not tell its own.
const DEFAULT_SCAN_SECS: u64 = 30;

/// How often to look for the device while it reconnects.
const REACH_RETRY_SECS: u64 = 5;

impl<'a, C: Crypto, S: KvBlobStore, O: OtaImageStore> Ctx<'a, C, S, O> {
    /// Make `change` to the networks of the Network Commissioning instance on
    /// `endpoint` of a listed device, and commit it; answers with its networking
    /// after. A change to its connection (connecting elsewhere, new credentials
    /// for the connected network) is committed only once the device answers on
    /// its network; otherwise its fail-safe reverts it.
    pub(super) async fn change_network(
        &self,
        node_id: u64,
        endpoint: u16,
        change: NetworkChange,
    ) -> Result<NetworkInfo, PeekError> {
        let dev = self.lookup_device(node_id)?;

        let before = self.read_network_of(dev.handle).await?;
        let config = before
            .configs
            .iter()
            .find(|config| config.endpoint == endpoint)
            .ok_or_else(|| {
                PeekError::failed(format!(
                    "the device has no network interface on endpoint {endpoint}"
                ))
            })?;
        let known = |id: &[u8]| config.networks.iter().any(|network| network.id == id);

        let dataset;
        let creds = match &change {
            NetworkChange::AddOrUpdate { credentials } => Some(match credentials {
                NetworkCredentials::Wifi { ssid, password } => {
                    expect_kind(config.kind, NetworkKind::WiFi)?;
                    WirelessCreds::Wifi {
                        ssid: ssid.as_bytes(),
                        pass: password.as_bytes(),
                    }
                }
                NetworkCredentials::Thread { dataset: hex } => {
                    expect_kind(config.kind, NetworkKind::Thread)?;
                    dataset = decode_hex(hex)
                        .map_err(|_| PeekError::failed("the Thread dataset is not hex"))?;
                    WirelessCreds::Thread {
                        dataset_tlv: &dataset,
                    }
                }
            }),
            _ => None,
        };

        // The network the change is about.
        let id = match &change {
            NetworkChange::AddOrUpdate { .. } => {
                creds
                    .as_ref()
                    .and_then(|creds| creds.id().ok())
                    .ok_or_else(|| PeekError::failed("the Thread dataset has no extended PAN ID"))?
            }
            NetworkChange::Remove { network_id }
            | NetworkChange::Connect { network_id }
            | NetworkChange::Reorder { network_id, .. } => {
                if !known(network_id) {
                    return Err(PeekError::failed("the device does not know that network"));
                }
                network_id.as_slice()
            }
        };

        // Connecting elsewhere, or new credentials for a known network (which
        // the device may be on), take reconnecting.
        let may_reconnect = match &change {
            NetworkChange::Connect { .. } => true,
            NetworkChange::AddOrUpdate { .. } => known(id),
            _ => false,
        };

        let connect_secs = config
            .connect_max_time_secs
            .map(u16::from)
            .unwrap_or(DEFAULT_CONNECT_SECS);
        let fail_safe_secs = if may_reconnect {
            connect_secs.saturating_add(RECONNECT_MARGIN_SECS)
        } else {
            FAIL_SAFE_SECS
        };

        let target = Target {
            matter: self.matter,
            crypto: self.crypto,
            fab_idx: dev.handle.fabric_index,
            node_id,
        };

        target.arm_fail_safe(fail_safe_secs).await?;

        // Whether the device is on that network: by what it shows of its
        // network, else by its flag.
        let on = match &change {
            NetworkChange::Reorder { .. } => false,
            _ if known(id) => match self.seen_on(&target, &before, config, id).await {
                Some(seen) => seen,
                None => flagged_on(config, id),
            },
            _ => false,
        };

        let reconnect = match &change {
            NetworkChange::Remove { .. } if on => {
                disarm(&target).await;
                return Err(PeekError::failed(
                    "the device is connected to that network: connect it to another one first",
                ));
            }
            NetworkChange::Connect { .. } if on => {
                disarm(&target).await;
                return Ok(before);
            }
            NetworkChange::Connect { .. } => true,
            // New credentials for the network the device is on are proven by
            // reconnecting with them.
            NetworkChange::AddOrUpdate { .. } => on,
            _ => false,
        };

        let changed = async {
            match &change {
                NetworkChange::AddOrUpdate { .. } => {
                    // Set above for exactly this change.
                    if let Some(creds) = &creds {
                        let index = target.add_or_update(endpoint, creds).await?;

                        // An addition goes last; some devices put it in a free
                        // slot instead.
                        let last = config.networks.len() as u8;
                        if !known(id) && index != Some(last) {
                            target.reorder(endpoint, id, last).await?;
                        }
                    }
                }
                NetworkChange::Remove { network_id } => target.remove(endpoint, network_id).await?,
                NetworkChange::Reorder { network_id, index } => {
                    target.reorder(endpoint, network_id, *index).await?
                }
                NetworkChange::Connect { .. } => {}
            }

            if reconnect {
                self.reconnect(
                    &target,
                    dev.handle,
                    endpoint,
                    id,
                    connect_secs,
                    fail_safe_secs,
                )
                .await?;
            }

            target.commit().await
        }
        .await;

        if let Err(e) = changed {
            // Revert at once where the device still answers; its fail-safe
            // reverts the change in time anyway.
            let disarmed = disarm(&target).await;

            return Err(match (reconnect, disarmed) {
                (true, true) => {
                    PeekError::failed(format!("{e}; the device is back on its previous network"))
                }
                (true, false) => PeekError::failed(format!(
                    "{e}; the device returns to its previous network within {fail_safe_secs} s"
                )),
                (false, _) => e,
            });
        }

        info!("network of node 0x{node_id:016X} changed: {change:?}");

        self.read_network_of(dev.handle).await
    }

    /// Connect the device to network `id`, then wait for it to answer on its
    /// network, connected to `id`.
    async fn reconnect(
        &self,
        target: &Target<'_, '_, C>,
        handle: DeviceHandle,
        endpoint: u16,
        id: &[u8],
        connect_secs: u16,
        fail_safe_secs: u16,
    ) -> Result<(), PeekError> {
        // Leave the fail-safe some time to commit in.
        let deadline = Instant::now() + Duration::from_secs(u64::from(fail_safe_secs) * 3 / 4);

        // An answer of the device's counts; none at all, it may have left its
        // network to join the new one before answering.
        let wait = Duration::from_secs(u64::from(connect_secs) + 15);
        match with_timeout(wait, target.connect(endpoint, id)).await {
            Ok(Ok(answer)) => answer?,
            Ok(Err(e)) => warn!(
                "ConnectNetwork: no answer ({:?}); looking for the device",
                e.code()
            ),
            Err(_) => warn!("ConnectNetwork: no answer in time; looking for the device"),
        }

        // It may have a new address now.
        self.controller.forget_sessions(handle);

        loop {
            match self.controller.read_network(handle).await {
                Ok(info) => {
                    let mut on = false;
                    if let Some(config) = info.configs.iter().find(|c| c.endpoint == endpoint) {
                        on = match self.seen_on(target, &info, config, id).await {
                            Some(seen) => seen,
                            None => flagged_on(config, id),
                        };
                    }

                    return if on {
                        Ok(())
                    } else {
                        Err(PeekError::failed(
                            "the device answers, but is not on the new network",
                        ))
                    };
                }
                Err(e) if Instant::now() < deadline => {
                    info!(
                        "node 0x{:016X} not reachable yet ({:?}); retrying",
                        handle.node_id,
                        e.code()
                    );
                    Timer::after(Duration::from_secs(REACH_RETRY_SECS)).await;
                }
                Err(e) => {
                    return Err(failed("the device is not reachable on the new network", e));
                }
            }
        }
    }

    /// Whether the device is on network `id` of `config`, by the Wi-Fi access
    /// point or Thread network it is on; `None` when it does not tell. Some
    /// devices keep the connected flag on a position in their list rather than
    /// on the network they are on.
    async fn seen_on(
        &self,
        target: &Target<'_, '_, C>,
        info: &NetworkInfo,
        config: &NetworkConfig,
        id: &[u8],
    ) -> Option<bool> {
        let seen = match config.kind? {
            NetworkKind::Thread => {
                let xpan = info.thread.as_ref()?.extended_pan_id?;
                xpan.to_be_bytes().as_slice() == id
            }
            NetworkKind::WiFi => {
                let bssid = info.wifi.as_ref()?.bssid.as_ref()?;
                let scan_secs = config
                    .scan_max_time_secs
                    .map(u64::from)
                    .unwrap_or(DEFAULT_SCAN_SECS);
                let wait = Duration::from_secs(scan_secs + 15);

                match with_timeout(wait, target.scan_wifi(config.endpoint, id)).await {
                    Ok(Ok(bssids)) => bssids.contains(bssid),
                    Ok(Err(e)) => {
                        warn!("{e}; going by the connected flag");
                        return None;
                    }
                    Err(_) => {
                        warn!("ScanNetworks: no answer in time; going by the connected flag");
                        return None;
                    }
                }
            }
            _ => return None,
        };

        if seen != flagged_on(config, id) {
            let name = config
                .networks
                .iter()
                .find(|network| network.id == id)
                .map_or("?", |network| network.name.as_str());
            warn!(
                "node 0x{:016X} flags network {name} {}connected, but is {}on it",
                target.node_id,
                if seen { "not " } else { "" },
                if seen { "" } else { "not " },
            );
        }

        Some(seen)
    }

    async fn read_network_of(&self, handle: DeviceHandle) -> Result<NetworkInfo, PeekError> {
        let mut info = self.controller.read_network(handle).await.map_err(|e| {
            failed(
                &format!("device 0x{:016X} did not answer", handle.node_id),
                e,
            )
        })?;
        self.name_thread_networks(&mut info).await;

        Ok(info)
    }
}

/// Whether the device flags network `id` of `config` connected.
fn flagged_on(config: &NetworkConfig, id: &[u8]) -> bool {
    config
        .networks
        .iter()
        .any(|network| network.connected && network.id == id)
}

/// Disarm the fail-safe, reverting what was not committed: whether the device
/// answered.
async fn disarm<C: Crypto>(target: &Target<'_, '_, C>) -> bool {
    matches!(
        with_timeout(Duration::from_secs(10), target.arm_fail_safe(0)).await,
        Ok(Ok(()))
    )
}

/// Credentials of `kind` are for an interface of that kind only.
fn expect_kind(interface: Option<NetworkKind>, kind: NetworkKind) -> Result<(), PeekError> {
    match interface {
        Some(interface) if interface != kind => Err(PeekError::failed(format!(
            "{} credentials for a {} interface",
            kind.label(),
            interface.label()
        ))),
        _ => Ok(()),
    }
}
