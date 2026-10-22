//! The Thread mesh, as our Thread devices see it (see [`crate::mesh`]).

use alloc::vec::Vec;

use embassy_time::{Duration, Instant};

use rs_matter::crypto::Crypto;
use rs_matter::persist::KvBlobStore;

use crate::api::{BorderRouter, FabricRef, NetworkKind, PeekError, ThreadMesh, UnreadDevice};
use crate::explain::describe;
use crate::mesh::{self, DeviceRead, HostRoute, MeshInput};
use crate::ota::OtaImageStore;

use super::Ctx;

/// How long a border router stays on the map unseen: a browse may miss one.
const BORDER_ROUTERS_KEPT: Duration = Duration::from_secs(600);

impl<C: Crypto, S: KvBlobStore, O: OtaImageStore> Ctx<'_, C, S, O> {
    /// Read the networking of our Thread devices on `fabric`, one after
    /// another, the Wi-Fi channels of our Wi-Fi devices there, and what the LAN
    /// advertises, into the mesh. Devices known to be offline are not tried.
    pub(super) async fn read_thread_mesh(
        &self,
        fabric: &FabricRef,
    ) -> Result<ThreadMesh, PeekError> {
        let devices = self.list_devices_on(fabric, None)?.devices;
        let mut input = MeshInput::default();

        for device in &devices {
            let node_id = device.handle.node_id;
            let thread = match device.network {
                Some(NetworkKind::Thread) => true,
                Some(NetworkKind::WiFi) => false,
                _ => continue,
            };
            if device.online == Some(false) {
                if thread {
                    input.unread.push(UnreadDevice {
                        node_id,
                        title: device.title(),
                        reason: "offline".into(),
                    });
                }
                continue;
            }
            let Ok(stored) = self.lookup_device(node_id) else {
                continue;
            };

            match self.controller.read_network(stored.handle).await {
                Ok(network) => {
                    self.set_online(node_id, true);
                    if thread {
                        input.devices.push(DeviceRead {
                            node_id,
                            title: device.title(),
                            network,
                        });
                    } else if let Some(channel) = network.wifi.and_then(|wifi| wifi.channel) {
                        input.wifi_channels.push((device.title(), channel));
                    }
                }
                Err(e) if thread => input.unread.push(UnreadDevice {
                    node_id,
                    title: device.title(),
                    reason: describe(e.code()),
                }),
                Err(_) => {}
            }
        }

        let fresh = crate::mdns::border_routers(&[]).await;
        input.border_routers = self.recent_border_routers(fresh);
        input.hosts = crate::mdns::matter_hosts().await;
        input.our_fabrics = self
            .fabrics
            .borrow()
            .iter()
            .map(|fabric| fabric.compressed_fabric_id)
            .collect();

        let mut names = self.thread_names.borrow_mut();
        for router in &input.border_routers {
            if let (Some(xpan), Some(name)) = (router.extended_pan_id, &router.network_name) {
                names.insert(xpan, name.clone());
            }
        }
        drop(names);

        for router in &input.border_routers {
            let Some(prefix) = router.omr_prefix.clone() else {
                continue;
            };
            if input.host_routes.iter().any(|route| route.prefix == prefix) {
                continue;
            }
            input.host_routes.push(HostRoute {
                routed: routes_to(&prefix),
                prefix,
                border_router: mesh::border_router_label(router),
                network: router.network_name.clone(),
            });
        }

        Ok(mesh::build(input))
    }

    /// `fresh`, with the border routers seen lately that it lacks.
    fn recent_border_routers(&self, fresh: Vec<BorderRouter>) -> Vec<BorderRouter> {
        let now = Instant::now();
        let mut seen = self.border_routers.borrow_mut();

        for router in fresh {
            seen.insert(router.name.clone(), (router, now));
        }
        seen.retain(|_, (_, at)| now.duration_since(*at) < BORDER_ROUTERS_KEPT);

        seen.values().map(|(router, _)| router.clone()).collect()
    }
}

/// Whether this machine routes to `prefix`, where it can tell.
fn routes_to(prefix: &str) -> Option<bool> {
    #[cfg(feature = "std")]
    {
        crate::host::routes_to(prefix)
    }

    #[cfg(not(feature = "std"))]
    {
        let _ = prefix;
        None
    }
}
