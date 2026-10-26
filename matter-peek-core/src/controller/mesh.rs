//! The Thread mesh, from what our devices' Thread Network Diagnostics show
//! (their neighbour and route tables), what the border routers advertise
//! (MeshCoP) and what Matter nodes advertise (`_matter._tcp`): one graph of
//! nodes and links, and what is wrong with it.
//!
//! Only the links our devices report are known; links between nodes that are
//! not ours stay unseen, so the graph is a partial view.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::api::{
    BorderRouter, FindingKind, FindingSeverity, IcdInfo, IcdMode, LanFabric, LanNode, LinkSignal,
    MeshCounts, MeshFinding, MeshLink, MeshNetwork, MeshNode, MeshNodeKind, MeshRoute, MeshSleep,
    NetworkInfo, NetworkKind, ThreadDiag, ThreadMesh, ThreadRole, UnreadDevice,
};
use crate::mdns::MatterHost;

/// From how many parent changes since boot a device's links are suspect.
const CHURN_PARENT_CHANGES: u16 = 3;

/// From how many detaches since boot a device keeps losing the network.
const CHURN_DETACHES: u16 = 3;

/// A link quality up to which a link is weak (of 0 to 3).
const WEAK_QUALITY: u8 = 1;

/// A signal strength up to which a link is weak, dBm.
const WEAK_RSSI: i8 = -85;

/// How many link findings to list; the rest are summed up.
const LINK_FINDINGS_MAX: usize = 6;

/// The router ID a route's next hop has where there is no route.
const NO_NEXT_HOP: u8 = 63;

/// The extended PAN ID of OpenThread's well-known default dataset.
const DEFAULT_XPAN: u64 = 0xDEAD_00BE_EF00_CAFE;

/// The names of OpenThread's well-known default datasets.
const DEFAULT_NAMES: &[&str] = &["OpenThread", "OpenThreadDemo"];

/// What the mesh is built from.
#[derive(Default)]
pub(crate) struct MeshInput {
    /// Our Thread devices that were read.
    pub devices: Vec<DeviceRead>,
    pub unread: Vec<UnreadDevice>,
    pub border_routers: Vec<BorderRouter>,
    pub hosts: Vec<MatterHost>,
    /// Our compressed fabric IDs: the hosts on them are our devices.
    pub our_fabrics: Vec<u64>,
    /// The Wi-Fi channels our Wi-Fi devices are on, by device title.
    pub wifi_channels: Vec<(String, u16)>,
    /// The border routers' OMR prefixes, and whether this machine routes to
    /// them (`None`: cannot tell).
    pub host_routes: Vec<HostRoute>,
}

/// One of our Thread devices, as read.
pub(crate) struct DeviceRead {
    pub node_id: u64,
    pub title: String,
    pub network: NetworkInfo,
}

/// Whether this machine routes to an OMR prefix.
pub(crate) struct HostRoute {
    pub prefix: String,
    pub border_router: String,
    pub network: Option<String>,
    pub routed: Option<bool>,
}

/// The mesh `input` shows, and what is wrong with it.
pub(crate) fn build(input: MeshInput) -> ThreadMesh {
    let mut mesh = Builder::default();

    for device in &input.devices {
        mesh.device(device);
    }
    for router in &input.border_routers {
        mesh.border_router(router);
    }
    let lan_nodes = mesh.hosts(&input);

    let networks = mesh.networks(&input);
    let nodes = mesh.ordered_nodes();
    let counts = counts(&nodes, &lan_nodes);
    let links: Vec<MeshLink> = mesh.links.values().cloned().collect();
    let routes = mesh.unlinked_routes(&links);

    let mut findings = Findings {
        nodes: &mesh.nodes,
        links: &links,
        networks: &networks,
        devices: &input.devices,
        list: Vec::new(),
    };
    findings.all(&input);
    let mut findings = findings.list;
    findings.sort_by_key(|finding| finding.severity);

    ThreadMesh {
        networks,
        nodes,
        counts,
        links,
        routes,
        lan_nodes,
        border_routers: input.border_routers,
        findings,
        unread: input.unread,
    }
}

/// The graph, as it is put together.
#[derive(Default)]
struct Builder {
    nodes: BTreeMap<u64, MeshNode>,
    /// By the lower and the higher extended address.
    links: BTreeMap<(u64, u64), MeshLink>,
    /// Our routers' routes to the routers they have no link to.
    routes: Vec<MeshRoute>,
}

impl Builder {
    fn device(&mut self, device: &DeviceRead) {
        let Some(diag) = device.network.thread.as_ref() else {
            return;
        };
        let Some(ext) = own_address(&device.network, diag) else {
            return;
        };

        let xpan = diag
            .extended_pan_id
            .or_else(|| connected_xpan(&device.network));
        let node = self.node(ext);
        node.label = device.title.clone();
        node.kind = MeshNodeKind::Device {
            node_id: device.node_id,
        };
        node.role = diag.role.or(node.role);
        node.rloc16 = diag.rloc16.or(node.rloc16);
        merge_sleep(&mut node.sleep, device.network.icd.as_ref().map(icd_sleep));
        node.extended_pan_id = xpan.or(node.extended_pan_id);
        node.partition_id = diag.partition_id.or(node.partition_id);

        let is_child = diag.role.is_some_and(child_role);

        for neighbor in &diag.neighbors {
            if neighbor.ext_address == ext {
                continue;
            }

            let other = self.node(neighbor.ext_address);
            other.rloc16 = other.rloc16.or(Some(neighbor.rloc16));
            other.extended_pan_id = other.extended_pan_id.or(xpan);
            other.partition_id = other.partition_id.or(diag.partition_id);

            if neighbor.is_child {
                other.parent = Some(ext);
                other.role = other.role.or(Some(if neighbor.rx_on_when_idle {
                    ThreadRole::EndDevice
                } else {
                    ThreadRole::SleepyEndDevice
                }));
            } else {
                other.role = Some(router_role(
                    neighbor.rloc16,
                    diag.leader_router_id,
                    other.role,
                ));
            }

            let parent_child = neighbor.is_child || is_child;
            if is_child && !neighbor.is_child {
                self.node(ext).parent = Some(neighbor.ext_address);
            }

            self.hears(
                ext,
                neighbor.ext_address,
                LinkSignal {
                    quality: Some(quality(neighbor.lqi)),
                    rssi: neighbor.average_rssi.or(neighbor.last_rssi),
                },
                parent_child,
            );
        }

        let leader = diag.leader_router_id;
        for route in diag.routes.iter().filter(|route| route.allocated) {
            if route.ext_address == ext {
                continue;
            }

            let other = self.node(route.ext_address);
            other.rloc16 = other.rloc16.or(Some(route.rloc16));
            other.extended_pan_id = other.extended_pan_id.or(xpan);
            other.partition_id = other.partition_id.or(diag.partition_id);
            other.role = Some(router_role(route.rloc16, leader, other.role));

            if route.link_established {
                self.hears(
                    ext,
                    route.ext_address,
                    LinkSignal {
                        quality: Some(quality(route.lqi_in)),
                        rssi: None,
                    },
                    false,
                );
                self.hears(
                    route.ext_address,
                    ext,
                    LinkSignal {
                        quality: Some(quality(route.lqi_out)),
                        rssi: None,
                    },
                    false,
                );
            } else if route.next_hop != NO_NEXT_HOP {
                let via = diag
                    .routes
                    .iter()
                    .find(|hop| hop.allocated && hop.router_id == route.next_hop)
                    .map(|hop| hop.ext_address)
                    .filter(|via| *via != route.ext_address);
                self.routes.push(MeshRoute {
                    from: ext,
                    to: route.ext_address,
                    via,
                    path_cost: route.path_cost,
                });
            }
        }
    }

    /// The cheapest route to each router no link is known to.
    fn unlinked_routes(&self, links: &[MeshLink]) -> Vec<MeshRoute> {
        let linked: BTreeSet<u64> = links.iter().flat_map(|link| [link.a, link.b]).collect();

        let mut cheapest: BTreeMap<u64, &MeshRoute> = BTreeMap::new();
        for route in self
            .routes
            .iter()
            .filter(|route| !linked.contains(&route.to))
        {
            let kept = cheapest.entry(route.to).or_insert(route);
            if route.path_cost < kept.path_cost {
                *kept = route;
            }
        }

        cheapest.into_values().cloned().collect()
    }

    fn border_router(&mut self, router: &BorderRouter) {
        let Some(ext) = router.ext_address else {
            return;
        };

        let node = self.node(ext);
        if matches!(node.kind, MeshNodeKind::Device { .. }) {
            return;
        }
        node.kind = MeshNodeKind::BorderRouter;
        node.label = border_router_label(router);
        node.extended_pan_id = router.extended_pan_id.or(node.extended_pan_id);
        node.partition_id = router.partition_id.or(node.partition_id);
    }

    /// The Matter nodes the LAN advertises: ours add to how they sleep; the
    /// others on Thread (at an address in a border router's prefix) are listed
    /// by host, as their host names are not their extended addresses.
    fn hosts(&mut self, input: &MeshInput) -> Vec<LanNode> {
        let prefixes: Vec<(u128, u32)> = input
            .border_routers
            .iter()
            .filter_map(|router| prefix(router.omr_prefix.as_deref()?))
            .collect();
        let mut lan: Vec<LanNode> = Vec::new();

        for host in &input.hosts {
            let sleep = host_sleep(host);

            if input.our_fabrics.contains(&host.compressed_fabric_id) {
                let ours = MeshNodeKind::Device {
                    node_id: host.node_id,
                };
                if let Some(node) = self.nodes.values_mut().find(|node| node.kind == ours) {
                    merge_sleep(&mut node.sleep, sleep);
                }
                continue;
            }
            if !on_thread(&host.address, &prefixes) {
                continue;
            }

            let fabric = LanFabric {
                compressed_fabric_id: host.compressed_fabric_id,
                node_id: host.node_id,
            };
            match lan.iter_mut().find(|node| node.host == host.host) {
                Some(node) => {
                    if !node.fabrics.contains(&fabric) {
                        node.fabrics.push(fabric);
                    }
                    merge_sleep(&mut node.sleep, sleep);
                }
                None => lan.push(LanNode {
                    host: host.host.clone(),
                    address: host.address.clone(),
                    fabrics: alloc::vec![fabric],
                    sleep,
                }),
            }
        }

        lan
    }

    fn node(&mut self, ext: u64) -> &mut MeshNode {
        self.nodes.entry(ext).or_insert_with(|| MeshNode {
            ext_address: ext,
            label: String::new(),
            kind: MeshNodeKind::Unknown,
            role: None,
            rloc16: None,
            extended_pan_id: None,
            partition_id: None,
            parent: None,
            sleep: None,
        })
    }

    /// `from` hears `to` with `signal`.
    fn hears(&mut self, from: u64, to: u64, signal: LinkSignal, parent_child: bool) {
        let key = (from.min(to), from.max(to));
        let link = self.links.entry(key).or_insert_with(|| MeshLink {
            a: key.0,
            b: key.1,
            parent_child,
            a_hears_b: None,
            b_hears_a: None,
        });
        link.parent_child |= parent_child;

        let side = if from == link.a {
            &mut link.a_hears_b
        } else {
            &mut link.b_hears_a
        };
        let held = side.get_or_insert_with(LinkSignal::default);
        held.quality = held.quality.or(signal.quality);
        held.rssi = held.rssi.or(signal.rssi);
    }

    fn networks(&self, input: &MeshInput) -> Vec<MeshNetwork> {
        fn network(networks: &mut BTreeMap<u64, MeshNetwork>, xpan: u64) -> &mut MeshNetwork {
            networks.entry(xpan).or_insert_with(|| MeshNetwork {
                extended_pan_id: xpan,
                ..Default::default()
            })
        }

        let mut networks: BTreeMap<u64, MeshNetwork> = BTreeMap::new();

        for device in &input.devices {
            let Some(diag) = device.network.thread.as_ref() else {
                continue;
            };
            let Some(xpan) = diag
                .extended_pan_id
                .or_else(|| connected_xpan(&device.network))
            else {
                continue;
            };

            let net = network(&mut networks, xpan);
            net.name = net.name.take().or_else(|| diag.network_name.clone());
            net.channel = net.channel.or(diag.channel);
            net.pan_id = net.pan_id.or(diag.pan_id);
            net.devices.push(device.node_id);
            if let Some(partition) = diag.partition_id {
                if !net.partitions.contains(&partition) {
                    net.partitions.push(partition);
                }
            }
        }

        for router in &input.border_routers {
            let Some(xpan) = router.extended_pan_id else {
                continue;
            };

            let net = network(&mut networks, xpan);
            net.name = net.name.take().or_else(|| router.network_name.clone());
            net.border_routers.push(border_router_label(router));
            if let Some(partition) = router.partition_id {
                if !net.partitions.contains(&partition) {
                    net.partitions.push(partition);
                }
            }
        }

        // Ours first.
        let mut networks: Vec<MeshNetwork> = networks.into_values().collect();
        networks.sort_by_key(|net| net.devices.is_empty());

        networks
    }

    /// Our devices first, then the border routers, then the rest.
    fn ordered_nodes(&mut self) -> Vec<MeshNode> {
        for node in self.nodes.values_mut().filter(|node| node.label.is_empty()) {
            node.label = match (node.role, node.rloc16) {
                (Some(ThreadRole::Router | ThreadRole::Leader), Some(rloc16)) => {
                    format!("Router {rloc16:04X}")
                }
                (_, Some(rloc16)) => format!("Thread node {rloc16:04X}"),
                _ => format!("Thread node {:016X}", node.ext_address),
            };
        }

        let rank = |node: &MeshNode| match node.kind {
            MeshNodeKind::Device { .. } => 0,
            MeshNodeKind::BorderRouter => 1,
            MeshNodeKind::OtherFabric { .. } => 2,
            MeshNodeKind::Unknown => 3,
        };
        let mut nodes: Vec<MeshNode> = self.nodes.values().cloned().collect();
        nodes.sort_by(|a, b| rank(a).cmp(&rank(b)).then_with(|| a.label.cmp(&b.label)));

        nodes
    }
}

/// The findings, as they are drawn up.
struct Findings<'a> {
    nodes: &'a BTreeMap<u64, MeshNode>,
    links: &'a [MeshLink],
    networks: &'a [MeshNetwork],
    devices: &'a [DeviceRead],
    list: Vec<MeshFinding>,
}

impl Findings<'_> {
    fn all(&mut self, input: &MeshInput) {
        self.unread(&input.unread);
        self.competing_networks();
        self.partition_splits();
        self.links();
        self.critical_routers();
        self.border_routers(&input.border_routers);
        self.churn();
        self.channel_overlap(&input.wifi_channels);
        self.default_credentials();
        self.host_routes(&input.host_routes);
    }

    fn add(&mut self, severity: FindingSeverity, kind: FindingKind, text: String, nodes: Vec<u64>) {
        self.list.push(MeshFinding {
            severity,
            kind,
            text,
            nodes,
        });
    }

    fn label(&self, ext: u64) -> String {
        self.nodes
            .get(&ext)
            .map(|node| node.label.clone())
            .unwrap_or_else(|| format!("{ext:016X}"))
    }

    fn device_title(&self, node_id: u64) -> String {
        self.devices
            .iter()
            .find(|device| device.node_id == node_id)
            .map(|device| device.title.clone())
            .unwrap_or_else(|| format!("{node_id:X}"))
    }

    fn unread(&mut self, unread: &[UnreadDevice]) {
        if unread.is_empty() {
            return;
        }

        let list = unread
            .iter()
            .map(|device| {
                format!(
                    "{} (node {:X}, {})",
                    device.title, device.node_id, device.reason
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        self.add(
            FindingSeverity::Info,
            FindingKind::Unread,
            format!("Not read, so missing from the mesh: {list}."),
            Vec::new(),
        );
    }

    fn competing_networks(&mut self) {
        if self.networks.len() < 2 {
            return;
        }

        let ours = self
            .networks
            .iter()
            .filter(|net| !net.devices.is_empty())
            .count();
        let describe = |net: &MeshNetwork| {
            let mut parts = Vec::new();
            if !net.devices.is_empty() {
                parts.push(
                    net.devices
                        .iter()
                        .map(|id| self.device_title(*id))
                        .collect::<Vec<_>>()
                        .join(", "),
                );
            }
            if !net.border_routers.is_empty() {
                parts.push(format!("border routers: {}", net.border_routers.join(", ")));
            }
            format!("{} ({})", network_name(net), parts.join("; "))
        };
        let list = self
            .networks
            .iter()
            .map(describe)
            .collect::<Vec<_>>()
            .join(", ");

        if ours > 1 {
            self.add(
                FindingSeverity::Warning,
                FindingKind::CompetingNetworks,
                format!(
                    "The Thread devices on our fabric are split across {} networks: {list}. A device \
                     reaches only the border routers of its own network, and each network \
                     is a weaker mesh than one would be: moving them onto one network helps.",
                    ours
                ),
                Vec::new(),
            );
        } else {
            self.add(
                FindingSeverity::Info,
                FindingKind::CompetingNetworks,
                format!(
                    "There are {} Thread networks on your LAN: {list}. Each ecosystem's \
                     border routers make a network of their own unless they share its \
                     credentials.",
                    self.networks.len()
                ),
                Vec::new(),
            );
        }
    }

    fn partition_splits(&mut self) {
        for net in self.networks {
            if net.partitions.len() < 2 {
                continue;
            }

            let nodes = self
                .nodes
                .values()
                .filter(|node| node.extended_pan_id == Some(net.extended_pan_id))
                .map(|node| node.ext_address)
                .collect();
            let text = format!(
                "{} is split into {} partitions, which can't reach each other: often a \
                 router or border router out of range, or one that restarted. It usually \
                 heals within minutes; if not, restart the border routers.",
                network_name(net),
                net.partitions.len()
            );
            self.add(
                FindingSeverity::Warning,
                FindingKind::PartitionSplit,
                text,
                nodes,
            );
        }
    }

    fn links(&mut self) {
        let mut weak = Vec::new();
        let mut one_way = Vec::new();

        for link in self.links {
            let (a, b) = (self.label(link.a), self.label(link.b));

            let directions = [(&a, &b, link.a_hears_b), (&b, &a, link.b_hears_a)];
            let weakest = directions
                .iter()
                .filter_map(|(hearer, heard, signal)| {
                    let signal = (*signal)?;
                    is_weak(signal)
                        .then(|| format!("{hearer} hears {heard} {}", signal_text(signal)))
                })
                .collect::<Vec<_>>();
            if !weakest.is_empty() {
                weak.push((link, weakest.join(", ")));
            }

            if let Some((hearer, heard)) = self.one_way(link, &a, &b) {
                one_way.push((
                    link,
                    format!("{hearer} hears {heard}, but {heard} not {hearer}"),
                ));
            }
        }

        let advice = "Move them closer, or put a mains-powered Thread device (a router) \
                      between them.";
        for (count, (link, text)) in weak.iter().enumerate() {
            if count == LINK_FINDINGS_MAX {
                self.add(
                    FindingSeverity::Warning,
                    FindingKind::WeakLink,
                    format!("And {} more weak links.", weak.len() - count),
                    Vec::new(),
                );
                break;
            }
            self.add(
                FindingSeverity::Warning,
                FindingKind::WeakLink,
                format!("Weak link: {text}. {advice}"),
                alloc::vec![link.a, link.b],
            );
        }
        for (count, (link, text)) in one_way.iter().enumerate() {
            if count == LINK_FINDINGS_MAX {
                self.add(
                    FindingSeverity::Warning,
                    FindingKind::OneWayLink,
                    format!("And {} more one-way links.", one_way.len() - count),
                    Vec::new(),
                );
                break;
            }
            self.add(
                FindingSeverity::Warning,
                FindingKind::OneWayLink,
                format!(
                    "One-way link: {text}. Frames get through in one direction only, so \
                     acknowledgements are lost. {advice}"
                ),
                alloc::vec![link.a, link.b],
            );
        }
    }

    /// Who hears whom, if only one of `link`'s nodes hears the other.
    fn one_way<'b>(&self, link: &MeshLink, a: &'b str, b: &'b str) -> Option<(&'b str, &'b str)> {
        let heard = |signal: Option<LinkSignal>| signal.and_then(|s| s.quality).map(|q| q > 0);

        match (heard(link.a_hears_b), heard(link.b_hears_a)) {
            (Some(true), Some(false)) => Some((a, b)),
            (Some(false), Some(true)) => Some((b, a)),
            // Each of our devices reports all it hears: one that does not
            // report the other does not hear it.
            (Some(true), None) if self.reported(link.b) => Some((a, b)),
            (None, Some(true)) if self.reported(link.a) => Some((b, a)),
            _ => None,
        }
    }

    /// Whether `ext` is one of our devices whose tables were read.
    fn reported(&self, ext: u64) -> bool {
        self.nodes
            .get(&ext)
            .is_some_and(|node| matches!(node.kind, MeshNodeKind::Device { .. }))
    }

    /// Routers that some of our devices, not their children, reach the
    /// border routers only through, as far as the links seen show.
    fn critical_routers(&mut self) {
        let border_routers: BTreeSet<u64> = self
            .nodes
            .values()
            .filter(|node| node.kind == MeshNodeKind::BorderRouter)
            .map(|node| node.ext_address)
            .collect();
        if border_routers.is_empty() {
            return;
        }

        let reached_without = |gone: Option<u64>| {
            let mut reached: BTreeSet<u64> = border_routers
                .iter()
                .copied()
                .filter(|ext| Some(*ext) != gone)
                .collect();
            let mut queue: Vec<u64> = reached.iter().copied().collect();
            while let Some(ext) = queue.pop() {
                for link in self.links {
                    let next = if link.a == ext {
                        link.b
                    } else if link.b == ext {
                        link.a
                    } else {
                        continue;
                    };
                    if Some(next) != gone && reached.insert(next) {
                        queue.push(next);
                    }
                }
            }
            reached
        };

        let reached = reached_without(None);
        let ours: Vec<u64> = self
            .nodes
            .values()
            .filter(|node| matches!(node.kind, MeshNodeKind::Device { .. }))
            .map(|node| node.ext_address)
            .filter(|ext| reached.contains(ext))
            .collect();

        let routers = self.nodes.values().filter(|node| {
            matches!(node.role, Some(ThreadRole::Router | ThreadRole::Leader))
                && node.kind != MeshNodeKind::BorderRouter
        });
        let mut found = Vec::new();
        for router in routers {
            let still = reached_without(Some(router.ext_address));
            let cut: Vec<u64> = ours
                .iter()
                .copied()
                .filter(|ext| *ext != router.ext_address && !still.contains(ext))
                .filter(|ext| {
                    self.nodes.get(ext).and_then(|node| node.parent) != Some(router.ext_address)
                })
                .collect();
            if !cut.is_empty() {
                found.push((router.ext_address, cut));
            }
        }

        for (router, cut) in found {
            let names = cut
                .iter()
                .map(|ext| self.label(*ext))
                .collect::<Vec<_>>()
                .join(", ");
            let mut nodes = alloc::vec![router];
            nodes.extend(&cut);
            self.add(
                FindingSeverity::Warning,
                FindingKind::CriticalRouter,
                format!(
                    "If {} fails, {names} lose their way to the border routers, as far as \
                     the links seen show: another router in range of them makes the mesh \
                     fail-safe.",
                    self.label(router)
                ),
                nodes,
            );
        }
    }

    fn border_routers(&mut self, routers: &[BorderRouter]) {
        for net in self.networks.iter().filter(|net| !net.devices.is_empty()) {
            match net.border_routers.len() {
                // Without any border router seen, mDNS may just not be
                // looked at here.
                0 if !routers.is_empty() => {
                    let devices = net
                        .devices
                        .iter()
                        .map(|id| self.device_title(*id))
                        .collect::<Vec<_>>()
                        .join(", ");
                    self.add(
                        FindingSeverity::Warning,
                        FindingKind::NoBorderRouter,
                        format!(
                            "No border router on your LAN advertises {}, the network of \
                             {devices}: without one, nothing on the LAN reaches them.",
                            network_name(net)
                        ),
                        Vec::new(),
                    );
                }
                1 => {
                    let nodes = self
                        .nodes
                        .values()
                        .filter(|node| {
                            node.kind == MeshNodeKind::BorderRouter
                                && node.extended_pan_id == Some(net.extended_pan_id)
                        })
                        .map(|node| node.ext_address)
                        .collect();
                    self.add(
                        FindingSeverity::Info,
                        FindingKind::SingleBorderRouter,
                        format!(
                            "{} has one border router, {}: while it is off, the network's \
                             devices can't reach the LAN. A second border router on the \
                             same network makes it redundant.",
                            network_name(net),
                            net.border_routers[0]
                        ),
                        nodes,
                    );
                }
                _ => {}
            }
        }
    }

    fn churn(&mut self) {
        for device in self.devices {
            let Some(diag) = device.network.thread.as_ref() else {
                continue;
            };
            let ext = own_address(&device.network, diag);
            let nodes: Vec<u64> = ext.into_iter().collect();

            if let Some(changes) = diag
                .parent_change_count
                .filter(|count| *count >= CHURN_PARENT_CHANGES)
            {
                self.add(
                    FindingSeverity::Warning,
                    FindingKind::ParentChurn,
                    format!(
                        "{} changed its parent {changes} times since it started: its links \
                         to the routers around it are unstable. A router closer to it helps.",
                        device.title
                    ),
                    nodes.clone(),
                );
            }
            if let Some(detaches) = diag
                .detached_role_count
                .filter(|count| *count >= CHURN_DETACHES)
            {
                self.add(
                    FindingSeverity::Warning,
                    FindingKind::Detaches,
                    format!(
                        "{} lost the network {detaches} times since it started.",
                        device.title
                    ),
                    nodes,
                );
            }
        }
    }

    fn channel_overlap(&mut self, wifi: &[(String, u16)]) {
        for net in self.networks.iter().filter(|net| !net.devices.is_empty()) {
            let Some(channel) = net.channel else { continue };
            let Some(thread_mhz) = thread_mhz(channel) else {
                continue;
            };

            let mut overlapping: BTreeMap<u16, Vec<&str>> = BTreeMap::new();
            for (device, wifi_channel) in wifi {
                let Some(wifi_mhz) = wifi_mhz(*wifi_channel) else {
                    continue;
                };
                if thread_mhz.abs_diff(wifi_mhz) <= 11 {
                    overlapping.entry(*wifi_channel).or_default().push(device);
                }
            }

            for (wifi_channel, devices) in overlapping {
                self.add(
                    FindingSeverity::Info,
                    FindingKind::ChannelOverlap,
                    format!(
                        "{}'s Thread channel {channel} ({thread_mhz} MHz) lies within Wi-Fi \
                         channel {wifi_channel}, which {} use: busy Wi-Fi can drown Thread \
                         frames. Thread channels 15, 20 and 25 sit between the usual Wi-Fi \
                         channels 1, 6 and 11.",
                        network_name(net),
                        devices.join(", ")
                    ),
                    Vec::new(),
                );
            }
        }
    }

    fn default_credentials(&mut self) {
        for net in self.networks {
            let default = net.extended_pan_id == DEFAULT_XPAN
                || net
                    .name
                    .as_deref()
                    .is_some_and(|name| DEFAULT_NAMES.contains(&name));
            if default {
                self.add(
                    FindingSeverity::Warning,
                    FindingKind::DefaultCredentials,
                    format!(
                        "{} uses OpenThread's well-known default credentials: anyone in \
                         range who knows them can join it. Give it a new dataset in your \
                         border router's app.",
                        network_name(net)
                    ),
                    Vec::new(),
                );
            }
        }
    }

    fn host_routes(&mut self, routes: &[HostRoute]) {
        for route in routes.iter().filter(|route| route.routed == Some(false)) {
            let network = route
                .network
                .as_deref()
                .map(|name| format!(" of {name}"))
                .unwrap_or_default();
            self.add(
                FindingSeverity::Warning,
                FindingKind::NoHostRoute,
                format!(
                    "This machine has no route to {}, the prefix through which {} reaches \
                     the Thread devices{network}. On Linux, accept the border routers' \
                     route advertisements for it: sysctl -w \
                     net.ipv6.conf.<interface>.accept_ra_rt_info_max_plen=64.",
                    route.prefix, route.border_router
                ),
                Vec::new(),
            );
        }
    }
}

/// The nodes of the mesh, counted.
fn counts(nodes: &[MeshNode], lan: &[LanNode]) -> MeshCounts {
    let mut counts = MeshCounts {
        nodes: nodes.len() as u32,
        lan_nodes: lan.len() as u32,
        lan_sleepy: lan.iter().filter(|node| node.sleepy()).count() as u32,
        ..Default::default()
    };

    for node in nodes {
        let ours = matches!(node.kind, MeshNodeKind::Device { .. });
        let border_router = node.kind == MeshNodeKind::BorderRouter;
        counts.on_our_fabric += ours as u32;
        counts.border_routers += border_router as u32;

        match node.role {
            Some(ThreadRole::Router | ThreadRole::Leader) => counts.routers += 1,
            Some(ThreadRole::EndDevice | ThreadRole::Reed) => counts.end_devices += 1,
            _ if node.sleepy() => counts.sleepy_end_devices += 1,
            // A border router routes.
            _ if border_router => counts.routers += 1,
            _ => counts.unknown_role += 1,
        }
    }

    counts
}

/// How an ICD sleeps, by its ICD Management: one that does not say its idle
/// time can only do short.
fn icd_sleep(icd: &IcdInfo) -> MeshSleep {
    MeshSleep {
        icd_mode: Some(icd.mode.unwrap_or(IcdMode::Sit)),
        idle_mode_secs: icd.idle_mode_secs,
        active_threshold_ms: icd.active_threshold_ms.map(u32::from),
        ..Default::default()
    }
}

/// How a Matter node sleeps, by its advert; `None` if it says nothing of it.
fn host_sleep(host: &MatterHost) -> Option<MeshSleep> {
    let sleep = MeshSleep {
        icd_mode: host
            .long_idle
            .map(|long| if long { IcdMode::Lit } else { IcdMode::Sit }),
        idle_interval_ms: host.idle_interval_ms,
        active_interval_ms: host.active_interval_ms,
        active_threshold_ms: host.active_threshold_ms,
        idle_mode_secs: None,
    };

    (sleep != MeshSleep::default()).then_some(sleep)
}

/// `into`, with what it lacks of `from`.
fn merge_sleep(into: &mut Option<MeshSleep>, from: Option<MeshSleep>) {
    let Some(from) = from else { return };
    let into = into.get_or_insert_with(MeshSleep::default);

    into.icd_mode = into.icd_mode.or(from.icd_mode);
    into.idle_interval_ms = into.idle_interval_ms.or(from.idle_interval_ms);
    into.active_interval_ms = into.active_interval_ms.or(from.active_interval_ms);
    into.active_threshold_ms = into.active_threshold_ms.or(from.active_threshold_ms);
    into.idle_mode_secs = into.idle_mode_secs.or(from.idle_mode_secs);
}

/// An IPv6 prefix like `fdac:bcdf:a3c9:1::/64`.
fn prefix(text: &str) -> Option<(u128, u32)> {
    let (address, len) = text.split_once('/')?;
    let address: core::net::Ipv6Addr = address.parse().ok()?;
    let len: u32 = len.parse().ok()?;

    (1..=128).contains(&len).then(|| (u128::from(address), len))
}

/// Whether `address` is in one of `prefixes`: on a Thread network.
fn on_thread(address: &str, prefixes: &[(u128, u32)]) -> bool {
    let Ok(address) = address.parse::<core::net::Ipv6Addr>() else {
        return false;
    };
    let address = u128::from(address);

    prefixes.iter().any(|&(prefix, len)| {
        let mask = u128::MAX << (128 - len);
        address & mask == prefix & mask
    })
}

/// A device's own extended address: from its Thread diagnostics, else its
/// Thread interface's hardware address.
fn own_address(network: &NetworkInfo, diag: &ThreadDiag) -> Option<u64> {
    diag.ext_address.or_else(|| {
        network
            .interfaces
            .iter()
            .filter(|iface| iface.kind == NetworkKind::Thread)
            .find_map(|iface| hardware_address(&iface.hardware_address))
    })
}

/// A hardware address like `EA:87:02:65:6C:D6:3E:20`, if it is 8 bytes and not
/// all zeros.
fn hardware_address(text: &str) -> Option<u64> {
    let bytes = text
        .split(':')
        .map(|byte| u8::from_str_radix(byte, 16).ok())
        .collect::<Option<Vec<u8>>>()?;
    let address = u64::from_be_bytes(bytes.try_into().ok()?);

    (address != 0).then_some(address)
}

/// The extended PAN ID of the Thread network a device is connected to.
fn connected_xpan(network: &NetworkInfo) -> Option<u64> {
    network
        .configs
        .iter()
        .filter(|config| config.kind == Some(NetworkKind::Thread))
        .flat_map(|config| &config.networks)
        .find(|known| known.connected)
        .and_then(|known| known.id.as_slice().try_into().ok())
        .map(u64::from_be_bytes)
}

/// The role of a router with `rloc16`: the leader's if its router ID is
/// `leader`'s, else a router's, unless known already.
fn router_role(rloc16: u16, leader: Option<u8>, known: Option<ThreadRole>) -> ThreadRole {
    if leader == Some((rloc16 >> 10) as u8) {
        ThreadRole::Leader
    } else {
        known.unwrap_or(ThreadRole::Router)
    }
}

fn child_role(role: ThreadRole) -> bool {
    matches!(
        role,
        ThreadRole::SleepyEndDevice | ThreadRole::EndDevice | ThreadRole::Reed
    )
}

/// A link quality from 0 to 3: OpenThread devices report it so, the spec
/// scales it to 255.
fn quality(lqi: u8) -> u8 {
    if lqi <= 3 {
        lqi
    } else {
        lqi / 64
    }
}

fn is_weak(signal: LinkSignal) -> bool {
    signal.quality.is_some_and(|q| q <= WEAK_QUALITY) || signal.rssi.is_some_and(|r| r <= WEAK_RSSI)
}

fn signal_text(signal: LinkSignal) -> String {
    match (signal.quality, signal.rssi) {
        (Some(q), Some(rssi)) => format!("at quality {q}/3 ({rssi} dBm)"),
        (Some(q), None) => format!("at quality {q}/3"),
        (None, Some(rssi)) => format!("at {rssi} dBm"),
        (None, None) => "faintly".to_string(),
    }
}

/// A border router by its model, with what tells it from others of the same
/// model (`Google Nest Hub #9206`), else by its name.
pub(crate) fn border_router_label(router: &BorderRouter) -> String {
    match router.model.as_deref() {
        Some(model) => match router.name.find(model) {
            Some(at) => router.name[at..].to_string(),
            None => model.to_string(),
        },
        None => router.name.clone(),
    }
}

fn network_name(net: &MeshNetwork) -> String {
    net.name
        .clone()
        .unwrap_or_else(|| format!("{:016X}", net.extended_pan_id))
}

/// The centre frequency of a 2.4 GHz Thread channel, MHz.
fn thread_mhz(channel: u16) -> Option<u16> {
    (11..=26)
        .contains(&channel)
        .then(|| 2405 + 5 * (channel - 11))
}

/// The centre frequency of a 2.4 GHz Wi-Fi channel, MHz; none for 5 GHz.
fn wifi_mhz(channel: u16) -> Option<u16> {
    match channel {
        1..=13 => Some(2407 + 5 * channel),
        14 => Some(2484),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use crate::api::{KnownNetwork, NetworkConfig, ThreadNeighbor, ThreadRoute};

    use super::*;

    const XPAN: u64 = 0xB566_147D_38E3_8420;
    const HUB: u64 = 0x6A94_7054_0026_9206;
    const ROUTER: u64 = 0xEA87_0265_6CD6_3E20;
    const SLEEPY: u64 = 0x1111_2222_3333_4444;
    const FAR: u64 = 0x5555_6666_7777_8888;

    fn diag(role: ThreadRole, ext: u64) -> ThreadDiag {
        ThreadDiag {
            role: Some(role),
            network_name: Some("NEST-PAN-0FD8".into()),
            channel: Some(25),
            pan_id: Some(0x0FD8),
            extended_pan_id: Some(XPAN),
            ext_address: Some(ext),
            partition_id: Some(614005346),
            leader_router_id: Some(16),
            ..Default::default()
        }
    }

    fn neighbor(ext: u64, lqi: u8, rssi: i8, is_child: bool) -> ThreadNeighbor {
        ThreadNeighbor {
            ext_address: ext,
            rloc16: 0x4000,
            age_secs: 5,
            lqi,
            average_rssi: Some(rssi),
            last_rssi: Some(rssi),
            frame_error_rate: 0,
            message_error_rate: 0,
            is_child,
            rx_on_when_idle: !is_child,
            full_thread_device: !is_child,
        }
    }

    fn route(ext: u64, router_id: u8, lqi_in: u8, lqi_out: u8) -> ThreadRoute {
        ThreadRoute {
            ext_address: ext,
            rloc16: (router_id as u16) << 10,
            router_id,
            next_hop: router_id,
            path_cost: 1,
            lqi_in,
            lqi_out,
            age_secs: 3,
            allocated: true,
            link_established: true,
        }
    }

    fn device(node_id: u64, title: &str, diag: ThreadDiag) -> DeviceRead {
        DeviceRead {
            node_id,
            title: title.into(),
            network: NetworkInfo {
                thread: Some(diag),
                ..Default::default()
            },
        }
    }

    fn hub() -> BorderRouter {
        BorderRouter {
            name: "Google Inc. Google Nest Hub #9206".into(),
            vendor: Some("Google Inc.".into()),
            model: Some("Google Nest Hub".into()),
            network_name: Some("NEST-PAN-0FD8".into()),
            extended_pan_id: Some(XPAN),
            ext_address: Some(HUB),
            partition_id: Some(614005346),
            ..Default::default()
        }
    }

    fn kinds(mesh: &ThreadMesh) -> Vec<FindingKind> {
        mesh.findings.iter().map(|finding| finding.kind).collect()
    }

    /// A router of ours next to the hub, with a sleepy device of ours as its
    /// child.
    fn healthy() -> MeshInput {
        let mut router = diag(ThreadRole::Router, ROUTER);
        router.neighbors = vec![neighbor(HUB, 3, -58, false), neighbor(SLEEPY, 3, -60, true)];
        router.routes = vec![route(HUB, 16, 3, 3), route(ROUTER, 12, 0, 0)];

        let mut sleepy = diag(ThreadRole::SleepyEndDevice, SLEEPY);
        sleepy.neighbors = vec![neighbor(ROUTER, 3, -61, false)];

        MeshInput {
            devices: vec![
                device(1, "ACME Test", router),
                device(2, "Door sensor", sleepy),
            ],
            border_routers: vec![hub()],
            ..Default::default()
        }
    }

    #[test]
    fn devices_and_their_tables_make_one_graph() {
        let mesh = build(healthy());

        let labels: Vec<&str> = mesh.nodes.iter().map(|n| n.label.as_str()).collect();
        assert_eq!(
            labels,
            ["ACME Test", "Door sensor", "Google Nest Hub #9206"]
        );
        assert_eq!(mesh.nodes[0].kind, MeshNodeKind::Device { node_id: 1 });
        assert_eq!(mesh.nodes[1].parent, Some(ROUTER));
        assert_eq!(mesh.nodes[2].kind, MeshNodeKind::BorderRouter);
        assert_eq!(mesh.nodes[2].role, Some(ThreadRole::Leader));

        assert_eq!(mesh.links.len(), 2);
        let child = mesh
            .links
            .iter()
            .find(|link| link.a.min(link.b) == SLEEPY.min(ROUTER))
            .unwrap();
        assert!(child.parent_child);
        assert!(child.a_hears_b.is_some() && child.b_hears_a.is_some());

        assert_eq!(mesh.networks.len(), 1);
        assert_eq!(mesh.networks[0].devices, [1, 2]);
        assert_eq!(mesh.networks[0].border_routers, ["Google Nest Hub #9206"]);

        // Only the one border router is worth knowing.
        assert_eq!(kinds(&mesh), [FindingKind::SingleBorderRouter]);
    }

    #[test]
    fn routers_without_a_link_are_placed_by_their_route() {
        let mut input = healthy();
        let router = input.devices[0].network.thread.as_mut().unwrap();
        let mut far = route(FAR, 20, 0, 0);
        far.link_established = false;
        far.next_hop = 16;
        far.path_cost = 3;
        let mut nowhere = route(0x9999, 21, 0, 0);
        nowhere.link_established = false;
        nowhere.next_hop = NO_NEXT_HOP;
        router.routes.extend([far, nowhere]);

        let mesh = build(input);
        assert_eq!(
            mesh.routes,
            [MeshRoute {
                from: ROUTER,
                to: FAR,
                via: Some(HUB),
                path_cost: 3,
            }]
        );
        let far = mesh.nodes.iter().find(|n| n.ext_address == FAR).unwrap();
        assert_eq!(far.role, Some(ThreadRole::Router));
        assert!(!mesh.links.iter().any(|l| l.a == FAR || l.b == FAR));
    }

    #[test]
    fn sleepy_devices_from_their_role_their_icd_and_their_advert() {
        const OURS: u64 = 0xEADD;

        let mut input = healthy();
        input.our_fabrics = alloc::vec![OURS];
        input.border_routers[0].omr_prefix = Some("fdac:bcdf:a3c9:1::/64".into());
        input.devices[1].network.icd = Some(IcdInfo {
            mode: Some(IcdMode::Lit),
            idle_mode_secs: Some(3600),
            active_mode_ms: Some(300),
            active_threshold_ms: Some(5000),
            ..Default::default()
        });
        let host =
            |fabric: u64, node_id: u64, name: &str, address: &str, sii: Option<u32>| MatterHost {
                compressed_fabric_id: fabric,
                node_id,
                host: name.into(),
                address: address.into(),
                idle_interval_ms: sii,
                active_interval_ms: sii.map(|_| 300),
                active_threshold_ms: None,
                long_idle: None,
            };
        input.hosts = vec![
            // Ours: its advert adds to how it sleeps.
            host(
                OURS,
                2,
                "430500CDAE446BE3.local",
                "fdac:bcdf:a3c9:1::2",
                Some(30_000),
            ),
            // On Thread, on two other fabrics, polling every 5 s.
            host(
                0xAB,
                0x77,
                "36C29F8DB1F0D7F3.local",
                "fdac:bcdf:a3c9:1::77",
                Some(5000),
            ),
            host(
                0xCD,
                0x5,
                "36C29F8DB1F0D7F3.local",
                "fdac:bcdf:a3c9:1::77",
                None,
            ),
            // On the LAN, not on Thread: same node ID as ours, other fabric.
            host(0xEF, 2, "DCA6322885670000.local", "192.168.10.50", None),
        ];

        let mesh = build(input);

        let sleepy = mesh
            .nodes
            .iter()
            .find(|n| n.ext_address == SLEEPY)
            .and_then(|n| n.sleep.clone())
            .unwrap();
        assert_eq!(sleepy.icd_mode, Some(IcdMode::Lit));
        assert_eq!(sleepy.idle_mode_secs, Some(3600));
        assert_eq!(sleepy.idle_interval_ms, Some(30_000));

        assert_eq!(mesh.lan_nodes.len(), 1);
        let lan = &mesh.lan_nodes[0];
        assert_eq!(lan.host, "36C29F8DB1F0D7F3.local");
        assert_eq!(lan.fabrics.len(), 2);
        assert!(lan.sleepy());

        assert_eq!(
            mesh.counts,
            MeshCounts {
                nodes: 3,
                on_our_fabric: 2,
                border_routers: 1,
                routers: 2,
                end_devices: 0,
                sleepy_end_devices: 1,
                unknown_role: 0,
                lan_nodes: 1,
                lan_sleepy: 1,
            }
        );
    }

    #[test]
    fn weak_and_one_way_links() {
        let mut input = healthy();
        let router = input.devices[0].network.thread.as_mut().unwrap();
        router.neighbors[1] = neighbor(SLEEPY, 1, -88, true);
        // The child does not report hearing its parent.
        input.devices[1]
            .network
            .thread
            .as_mut()
            .unwrap()
            .neighbors
            .clear();

        let mesh = build(input);
        let kinds = kinds(&mesh);
        assert!(kinds.contains(&FindingKind::WeakLink), "{kinds:?}");
        assert!(kinds.contains(&FindingKind::OneWayLink), "{kinds:?}");
        let weak = mesh
            .findings
            .iter()
            .find(|f| f.kind == FindingKind::WeakLink)
            .unwrap();
        assert!(
            weak.text
                .contains("ACME Test hears Door sensor at quality 1/3 (-88 dBm)"),
            "{}",
            weak.text
        );
        assert_eq!(mesh.findings[0].severity, FindingSeverity::Warning);
    }

    #[test]
    fn a_router_in_the_middle_is_critical() {
        // hub - ACME Test - far router of ours, which links to nothing else.
        let mut input = healthy();
        let mut far = diag(ThreadRole::Router, FAR);
        far.neighbors = vec![neighbor(ROUTER, 2, -75, false)];
        far.routes = vec![route(ROUTER, 12, 2, 2)];
        input.devices.push(device(3, "Far plug", far));

        let mesh = build(input);
        let critical: Vec<&MeshFinding> = mesh
            .findings
            .iter()
            .filter(|f| f.kind == FindingKind::CriticalRouter)
            .collect();
        assert_eq!(critical.len(), 1, "{:?}", mesh.findings);
        // Not for its own child, the sleepy device.
        assert!(
            critical[0]
                .text
                .starts_with("If ACME Test fails, Far plug lose"),
            "{}",
            critical[0].text
        );
    }

    #[test]
    fn networks_partitions_and_credentials() {
        let mut input = healthy();
        input.devices[1]
            .network
            .thread
            .as_mut()
            .unwrap()
            .partition_id = Some(1);
        input.border_routers.push(BorderRouter {
            name: "Apple HomePod".into(),
            network_name: Some("OpenThread".into()),
            extended_pan_id: Some(DEFAULT_XPAN),
            ..Default::default()
        });

        let mesh = build(input);
        let kinds = kinds(&mesh);
        assert!(kinds.contains(&FindingKind::PartitionSplit), "{kinds:?}");
        assert!(kinds.contains(&FindingKind::CompetingNetworks), "{kinds:?}");
        assert!(
            kinds.contains(&FindingKind::DefaultCredentials),
            "{kinds:?}"
        );
        // Ours first.
        assert_eq!(mesh.networks[0].extended_pan_id, XPAN);
        let competing = mesh
            .findings
            .iter()
            .find(|f| f.kind == FindingKind::CompetingNetworks)
            .unwrap();
        assert_eq!(competing.severity, FindingSeverity::Info);
    }

    #[test]
    fn churn_overlap_routes_and_unread() {
        let mut input = healthy();
        let sleepy = input.devices[1].network.thread.as_mut().unwrap();
        sleepy.parent_change_count = Some(7);
        sleepy.detached_role_count = Some(1);
        input.wifi_channels = vec![("Shelly 2PM".into(), 13), ("Shelly 1PM".into(), 6)];
        input.host_routes = vec![HostRoute {
            prefix: "fdac:bcdf:a3c9:1::/64".into(),
            border_router: "Google Nest Hub".into(),
            network: Some("NEST-PAN-0FD8".into()),
            routed: Some(false),
        }];
        input.unread = vec![UnreadDevice {
            node_id: 9,
            title: "Old sensor".into(),
            reason: "offline".into(),
        }];

        let mesh = build(input);
        let kinds = kinds(&mesh);
        assert!(kinds.contains(&FindingKind::ParentChurn), "{kinds:?}");
        assert!(!kinds.contains(&FindingKind::Detaches), "{kinds:?}");
        assert!(kinds.contains(&FindingKind::NoHostRoute), "{kinds:?}");
        assert!(kinds.contains(&FindingKind::Unread), "{kinds:?}");
        let overlap: Vec<&MeshFinding> = mesh
            .findings
            .iter()
            .filter(|f| f.kind == FindingKind::ChannelOverlap)
            .collect();
        // Channel 25 (2475 MHz) is within Wi-Fi 13 (2472 MHz), not 6.
        assert_eq!(overlap.len(), 1);
        assert!(
            overlap[0]
                .text
                .contains("Wi-Fi channel 13, which Shelly 2PM use"),
            "{}",
            overlap[0].text
        );
    }

    #[test]
    fn identity_from_the_interface_and_the_connected_network() {
        let mut thread = diag(ThreadRole::Router, ROUTER);
        thread.ext_address = None;
        thread.extended_pan_id = None;
        let mut read = device(1, "ACME Test", thread);
        read.network.interfaces = vec![crate::api::NetworkInterfaceInfo {
            name: "ot1".into(),
            kind: NetworkKind::Thread,
            operational: true,
            hardware_address: "EA:87:02:65:6C:D6:3E:20".into(),
            ipv4_addresses: Vec::new(),
            ipv6_addresses: Vec::new(),
            off_premise_ipv4: None,
            off_premise_ipv6: None,
        }];
        read.network.configs = vec![NetworkConfig {
            endpoint: 0,
            kind: Some(NetworkKind::Thread),
            interface_enabled: Some(true),
            max_networks: Some(1),
            networks: vec![KnownNetwork {
                id: XPAN.to_be_bytes().to_vec(),
                name: String::new(),
                connected: true,
            }],
            last_status: None,
            last_network_id: None,
            last_network: None,
            last_connect_error: None,
            connect_max_time_secs: None,
            scan_max_time_secs: None,
        }];

        let mesh = build(MeshInput {
            devices: vec![read],
            ..Default::default()
        });
        assert_eq!(mesh.nodes[0].ext_address, ROUTER);
        assert_eq!(mesh.networks[0].extended_pan_id, XPAN);

        assert_eq!(hardware_address("00:00:00:00:00:00:00:00"), None);
        assert_eq!(quality(200), 3);
        assert_eq!(thread_mhz(15), Some(2425));
    }
}
