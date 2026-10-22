//! The Thread page: the mesh our Thread devices see (see
//! `matter_peek_core::api::ThreadMesh`): what is wrong with it, its networks,
//! a map of its nodes and links, and the lists behind the map.

use std::collections::BTreeMap;
use std::f64::consts::{FRAC_PI_2, TAU};

use yew::html::Scope;
use yew::prelude::*;

use matter_peek_core::api::{
    wire, FindingSeverity, IcdMode, LanNode, LinkSignal, MeshLink, MeshNetwork, MeshNode,
    MeshNodeKind, MeshRoute, MeshSleep, ThreadMesh, ThreadRole,
};

use crate::icons::{icon, Icon};
use crate::{request_loop, ui, App, Msg};

/// How far from the middle of the map the routers are, and the children.
const ROUTER_RING: f64 = 105.0;
const CHILD_RING: f64 = 165.0;

/// How far apart the children of one router are on the map, radians.
const CHILD_SPREAD: f64 = 0.32;

/// How many characters of a label the map shows.
const MAP_LABEL_MAX: usize = 18;

/// The Thread page.
#[derive(Default)]
pub struct ThreadPage {
    pub loading: bool,
    pub mesh: Option<ThreadMesh>,
    pub error: Option<String>,
    /// When the mesh was read, in ms since the Unix epoch.
    pub read_at: Option<f64>,
}

/// What happens on the Thread page.
pub enum ThreadMsg {
    Read,
    Loaded(wire::Response),
    Failed(String),
}

impl App {
    pub(crate) fn update_thread(&mut self, ctx: &Context<Self>, msg: ThreadMsg) -> bool {
        let page = &mut self.thread;

        match msg {
            ThreadMsg::Read => {
                if page.loading {
                    return false;
                }
                page.loading = true;
                page.error = None;
                request_loop(
                    ctx.link().clone(),
                    wire::Request::ReadThreadMesh,
                    |response| Msg::Thread(ThreadMsg::Loaded(response)),
                    |e| Msg::Thread(ThreadMsg::Failed(e)),
                );
            }
            ThreadMsg::Loaded(wire::Response::ThreadMesh { mesh }) => {
                page.loading = false;
                page.mesh = Some(*mesh);
                page.read_at = Some(js_sys::Date::now());
            }
            ThreadMsg::Loaded(wire::Response::Error { message }) | ThreadMsg::Failed(message) => {
                page.loading = false;
                page.error = Some(message);
            }
            ThreadMsg::Loaded(_) => page.loading = false,
        }

        true
    }

    pub(crate) fn view_thread_tab(&self, link: &Scope<Self>) -> Html {
        let page = &self.thread;

        let subtitle = match &page.mesh {
            Some(mesh) => {
                let networks = mesh.networks.len();
                let mut parts = vec![
                    format!("{networks} network{}", if networks == 1 { "" } else { "s" }),
                    format!("{} nodes", mesh.nodes.len()),
                    format!(
                        "{} routers, {} sleepy",
                        mesh.counts.routers, mesh.counts.sleepy_end_devices
                    ),
                ];
                if !mesh.lan_nodes.is_empty() {
                    parts.push(format!("{} on the LAN", mesh.lan_nodes.len()));
                }
                if let Some(at) = page.read_at {
                    parts.push(format!("read {}", ago(at)));
                }
                parts.join(" · ")
            }
            None => "The mesh the Thread devices on our fabric see".to_string(),
        };
        let actions = html! {
            <button class={classes!("button", "is-tinted", page.loading.then_some("is-loading"))}
                title="Read the Thread devices again" disabled={page.loading}
                onclick={link.callback(|_| Msg::Thread(ThreadMsg::Read))}>
                <span class="icon">{ icon(Icon::Refresh, 18) }</span>
                <span>{ "Read again" }</span>
            </button>
        };

        html! {
            <div class="thread-page">
                { ui::page_header("Thread", html! { subtitle }, actions) }
                if let Some(error) = &page.error {
                    <p class="thread-error">{ icon(Icon::Alert, 16) }<span>{ error.clone() }</span></p>
                }
                { match &page.mesh {
                    None if page.loading => html! {
                        <div class="empty-state">
                            <span class="loader is-large"></span>
                            <p class="empty-state-text">{ "Reading the Thread devices on our fabric, one after another…" }</p>
                        </div>
                    },
                    None => html! {},
                    Some(mesh) if mesh.nodes.is_empty() && mesh.networks.is_empty() => ui::empty_state(
                        Icon::Thread,
                        "No Thread mesh",
                        "No Thread device of ours was read, and no border router seen on the LAN.",
                        html! {},
                    ),
                    Some(mesh) => html! {
                        <>
                            { view_findings(mesh) }
                            { view_networks(mesh) }
                            { view_map(mesh) }
                            { view_nodes(mesh) }
                            { view_lan_nodes(mesh) }
                            { view_links(mesh) }
                        </>
                    },
                } }
            </div>
        }
    }
}

fn view_findings(mesh: &ThreadMesh) -> Html {
    html! {
        <section class="thread-section">
            if mesh.findings.is_empty() {
                <div class="finding is-ok">
                    <span class="finding-icon">{ icon(Icon::CheckCircle, 18) }</span>
                    <span>{ "Nothing wrong found." }</span>
                </div>
            }
            { for mesh.findings.iter().map(|finding| {
                let (class, glyph) = match finding.severity {
                    FindingSeverity::Warning => ("is-warning", Icon::Alert),
                    FindingSeverity::Info => ("is-info", Icon::Info),
                };
                html! {
                    <div class={classes!("finding", class)}>
                        <span class="finding-icon">{ icon(glyph, 18) }</span>
                        <span>{ finding.text.clone() }</span>
                    </div>
                }
            }) }
        </section>
    }
}

fn view_networks(mesh: &ThreadMesh) -> Html {
    let label = |node_id: u64| {
        mesh.nodes
            .iter()
            .find(|node| node.kind == MeshNodeKind::Device { node_id })
            .map(|node| node.label.clone())
            .unwrap_or_else(|| format!("{node_id:X}"))
    };

    html! {
        <section class="thread-section">
            <p class="thread-section-title">{ "Networks" }</p>
            <div class="thread-networks">
                { for mesh.networks.iter().map(|net| html! {
                    <div class="thread-network">
                        <div class="thread-network-head">
                            <span class="tile-icon is-thread">{ icon(Icon::Thread, 20) }</span>
                            <div class="thread-network-title">
                                <span class="thread-network-name">{ network_name(net) }</span>
                                <span class="thread-network-id mono">{ format!("{:016X}", net.extended_pan_id) }</span>
                            </div>
                        </div>
                        <div class="thread-facts">
                            if let Some(channel) = net.channel {
                                { fact("Channel", channel.to_string()) }
                            }
                            if let Some(pan) = net.pan_id {
                                { fact("PAN ID", format!("0x{pan:04X}")) }
                            }
                            if !net.partitions.is_empty() {
                                { fact(if net.partitions.len() == 1 { "Partition" } else { "Partitions" },
                                    net.partitions.iter().map(u32::to_string).collect::<Vec<_>>().join(", ")) }
                            }
                        </div>
                        <div class="thread-chips">
                            { for net.devices.iter().map(|id| html! {
                                <span class="thread-chip is-device">{ icon(Icon::Devices, 13) }{ label(*id) }</span>
                            }) }
                            { for net.border_routers.iter().map(|router| html! {
                                <span class="thread-chip is-router">{ icon(Icon::Router, 13) }{ router.clone() }</span>
                            }) }
                            if net.devices.is_empty() {
                                <span class="thread-chip is-none">{ "none on our fabric" }</span>
                            }
                        </div>
                    </div>
                }) }
            </div>
        </section>
    }
}

fn fact(label: &str, value: String) -> Html {
    html! {
        <div class="thread-fact">
            <span class="health-label">{ label.to_string() }</span>
            <span class="health-value">{ value }</span>
        </div>
    }
}

/// The map: the border routers in the middle, the routers around them, each
/// child beyond its parent.
fn view_map(mesh: &ThreadMesh) -> Html {
    if mesh.nodes.is_empty() {
        return html! {};
    }

    let places = layout(mesh);
    let at = |ext: u64| places.get(&ext).copied();
    let view_box = view_box(&places);

    html! {
        <section class="thread-section">
            <p class="thread-section-title">{ "Map" }</p>
            <div class="mesh-map-card">
                <svg class="mesh-map" viewBox={view_box} role="img" aria-label="The Thread mesh">
                    { for mesh.links.iter().filter_map(|link| {
                        let ((x1, y1), (x2, y2)) = (at(link.a)?, at(link.b)?);
                        Some(html! {
                            <line class={classes!("mesh-link", link_class(link), link.parent_child.then_some("is-child"),
                                    one_way(link).then_some("is-one-way"))}
                                x1={x1.to_string()} y1={y1.to_string()} x2={x2.to_string()} y2={y2.to_string()}>
                                <title>{ link_title(mesh, link) }</title>
                            </line>
                        })
                    }) }
                    { for mesh.nodes.iter().filter_map(|node| {
                        // A parent and child whose link no one reported.
                        let parent = node.parent?;
                        let linked = mesh.links.iter().any(|link| {
                            (link.a, link.b) == (parent, node.ext_address)
                                || (link.a, link.b) == (node.ext_address, parent)
                        });
                        if linked {
                            return None;
                        }
                        let ((x1, y1), (x2, y2)) = (at(node.ext_address)?, at(parent)?);
                        Some(html! {
                            <line class="mesh-link is-implied"
                                x1={x1.to_string()} y1={y1.to_string()} x2={x2.to_string()} y2={y2.to_string()}>
                                <title>{ "Its parent; their link is not reported" }</title>
                            </line>
                        })
                    }) }
                    { for mesh.routes.iter().filter_map(|route| {
                        // Where a router no link is known to sits: by the router
                        // our router reaches it through.
                        let near = route.via.unwrap_or(route.from);
                        let ((x1, y1), (x2, y2)) = (at(route.to)?, at(near)?);
                        Some(html! {
                            <line class="mesh-link is-route"
                                x1={x1.to_string()} y1={y1.to_string()} x2={x2.to_string()} y2={y2.to_string()}>
                                <title>{ route_text(mesh, route) }</title>
                            </line>
                        })
                    }) }
                    { for mesh.nodes.iter().filter_map(|node| {
                        let (x, y) = at(node.ext_address)?;
                        Some(html! {
                            <g class={classes!("mesh-node", node_class(node))}
                                transform={format!("translate({x} {y})")}>
                                <circle r="17"></circle>
                                <text class="mesh-node-mark" dy="5">{ role_mark(node) }</text>
                                <text class="mesh-node-label" dy="34">{ short(&node.label) }</text>
                                <title>{ format!("{} — {} · {:016X}", node.label, role_name(node), node.ext_address) }</title>
                            </g>
                        })
                    }) }
                </svg>
                <div class="mesh-legend">
                    <span><i class="mesh-swatch is-device"></i>{ "on our fabric" }</span>
                    <span><i class="mesh-swatch is-router"></i>{ "border router" }</span>
                    <span><i class="mesh-swatch is-other"></i>{ "other" }</span>
                    <span><i class="mesh-line q3"></i>{ "good link" }</span>
                    <span><i class="mesh-line q1"></i>{ "weak link" }</span>
                    <span><i class="mesh-line is-one-way"></i>{ "one way" }</span>
                    <span><i class="mesh-line is-route"></i>{ "route, links unknown" }</span>
                </div>
            </div>
        </section>
    }
}

fn view_nodes(mesh: &ThreadMesh) -> Html {
    let label = |ext: u64| {
        mesh.nodes
            .iter()
            .find(|node| node.ext_address == ext)
            .map(|node| node.label.clone())
            .unwrap_or_else(|| format!("{ext:016X}"))
    };

    html! {
        <section class="thread-section">
            <p class="thread-section-title">
                { "Nodes" }<span class="nav-count">{ mesh.nodes.len() }</span>
            </p>
            <div class="thread-list">
                { for mesh.nodes.iter().map(|node| html! {
                    <div class="thread-row">
                        <span class={classes!("mesh-dot", node_class(node))}>{ role_mark(node) }</span>
                        <div class="thread-row-main">
                            <span class="thread-row-title">{ node.label.clone() }</span>
                            <span class="thread-row-sub">
                                { kind_name(node) }{ " · " }{ role_name(node) }
                                if let Some(parent) = node.parent {
                                    { format!(" · child of {}", label(parent)) }
                                }
                                if let Some(route) = mesh.routes.iter().find(|route| route.to == node.ext_address) {
                                    { format!(" · {}", route_text(mesh, route)) }
                                }
                                if let Some(sleep) = node.sleep.as_ref().map(sleep_text).filter(|text| !text.is_empty()) {
                                    { format!(" · {sleep}") }
                                }
                            </span>
                        </div>
                        <div class="thread-row-ids mono">
                            if let Some(rloc16) = node.rloc16 {
                                <span>{ format!("{rloc16:04X}") }</span>
                            }
                            <span>{ format!("{:016X}", node.ext_address) }</span>
                        </div>
                    </div>
                }) }
            </div>
        </section>
    }
}

/// Matter nodes on Thread of other fabrics: the LAN advertises them, but no
/// table we read places them.
fn view_lan_nodes(mesh: &ThreadMesh) -> Html {
    if mesh.lan_nodes.is_empty() {
        return html! {};
    }

    html! {
        <section class="thread-section">
            <p class="thread-section-title">
                { "Seen on the LAN" }<span class="nav-count">{ mesh.lan_nodes.len() }</span>
            </p>
            <p class="thread-section-note">
                { "Matter nodes on Thread of other fabrics. Where they sit in the mesh is unknown, and some may be among the nodes above." }
            </p>
            <div class="thread-list">
                { for mesh.lan_nodes.iter().map(view_lan_node) }
            </div>
        </section>
    }
}

fn view_lan_node(node: &LanNode) -> Html {
    let fabrics = node
        .fabrics
        .iter()
        .map(|f| {
            format!(
                "node {:X} on fabric {:016X}",
                f.node_id, f.compressed_fabric_id
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let sleep = node
        .sleep
        .as_ref()
        .map(sleep_text)
        .filter(|text| !text.is_empty());

    html! {
        <div class="thread-row">
            <span class="mesh-dot is-other">{ if node.sleepy() { "S" } else { "?" } }</span>
            <div class="thread-row-main">
                <span class="thread-row-title mono">{ node.host.clone() }</span>
                <span class="thread-row-sub">
                    { fabrics }
                    if let Some(sleep) = sleep {
                        { format!(" · {sleep}") }
                    }
                </span>
            </div>
            <div class="thread-row-ids mono">
                <span>{ node.address.clone() }</span>
            </div>
        </div>
    }
}

fn view_links(mesh: &ThreadMesh) -> Html {
    if mesh.links.is_empty() {
        return html! {};
    }

    let label = |ext: u64| {
        mesh.nodes
            .iter()
            .find(|node| node.ext_address == ext)
            .map(|node| node.label.clone())
            .unwrap_or_else(|| format!("{ext:016X}"))
    };

    html! {
        <section class="thread-section">
            <p class="thread-section-title">
                { "Links" }<span class="nav-count">{ mesh.links.len() }</span>
            </p>
            <div class="thread-list">
                { for mesh.links.iter().map(|link| {
                    let (a, b) = (label(link.a), label(link.b));
                    html! {
                        <div class="thread-row">
                            <span class={classes!("mesh-link-mark", link_class(link))}></span>
                            <div class="thread-row-main">
                                <span class="thread-row-title">{ format!("{a} ⇄ {b}") }</span>
                                <span class="thread-row-sub">
                                    { if link.parent_child { "parent and child" } else { "routers" } }
                                    if one_way(link) {
                                        { " · one way" }
                                    }
                                </span>
                            </div>
                            <div class="thread-signals">
                                { signal_chip(&a, &b, link.a_hears_b) }
                                { signal_chip(&b, &a, link.b_hears_a) }
                            </div>
                        </div>
                    }
                }) }
            </div>
        </section>
    }
}

fn signal_chip(hearer: &str, heard: &str, signal: Option<LinkSignal>) -> Html {
    let text = match signal {
        Some(signal) => signal_text(signal),
        None => "not reported".into(),
    };
    let class = signal.and_then(|s| s.quality).map(quality_class);

    html! {
        <span class={classes!("signal-chip", class)} title={format!("How {hearer} hears {heard}")}>
            <span class="signal-chip-who">{ format!("{} hears", short(hearer)) }</span>
            <span>{ text }</span>
        </span>
    }
}

/// Where each node goes on the map.
fn layout(mesh: &ThreadMesh) -> BTreeMap<u64, (f64, f64)> {
    let mut places = BTreeMap::new();

    let border_routers: Vec<&MeshNode> = mesh
        .nodes
        .iter()
        .filter(|node| node.kind == MeshNodeKind::BorderRouter)
        .collect();
    let children: Vec<&MeshNode> = mesh
        .nodes
        .iter()
        .filter(|node| node.parent.is_some() && node.kind != MeshNodeKind::BorderRouter)
        .collect();
    let mut middle: Vec<&MeshNode> = border_routers;
    let mut ring: Vec<&MeshNode> = mesh
        .nodes
        .iter()
        .filter(|node| node.kind != MeshNodeKind::BorderRouter && node.parent.is_none())
        .collect();
    // Without a border router, the leader takes the middle.
    if middle.is_empty() {
        if let Some(at) = ring
            .iter()
            .position(|node| node.role == Some(ThreadRole::Leader))
        {
            middle.push(ring.remove(at));
        }
    }

    match middle.len() {
        1 => {
            places.insert(middle[0].ext_address, (0.0, 0.0));
        }
        count => {
            for (i, node) in middle.iter().enumerate() {
                let angle = -FRAC_PI_2 + TAU * i as f64 / count as f64;
                places.insert(node.ext_address, (45.0 * angle.cos(), 45.0 * angle.sin()));
            }
        }
    }
    for (i, node) in ring.iter().enumerate() {
        let angle = -FRAC_PI_2 + TAU * i as f64 / ring.len().max(1) as f64;
        places.insert(
            node.ext_address,
            (ROUTER_RING * angle.cos(), ROUTER_RING * angle.sin()),
        );
    }

    let mut by_parent: BTreeMap<u64, Vec<&MeshNode>> = BTreeMap::new();
    for child in children {
        by_parent
            .entry(child.parent.unwrap_or_default())
            .or_default()
            .push(child);
    }
    for (parent, children) in by_parent {
        let base = places
            .get(&parent)
            .map(|(x, y)| {
                if *x == 0.0 && *y == 0.0 {
                    FRAC_PI_2
                } else {
                    y.atan2(*x)
                }
            })
            .unwrap_or(FRAC_PI_2);
        let count = children.len() as f64;
        for (i, child) in children.iter().enumerate() {
            let angle = base + CHILD_SPREAD * (i as f64 - (count - 1.0) / 2.0);
            places.insert(
                child.ext_address,
                (CHILD_RING * angle.cos(), CHILD_RING * angle.sin()),
            );
        }
    }

    relax(mesh, &mut places);

    places
}

/// The map's view: around the nodes, with room for their labels.
fn view_box(places: &BTreeMap<u64, (f64, f64)>) -> String {
    const MARGIN: f64 = 75.0;

    let (mut left, mut top, mut right, mut bottom) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    for (x, y) in places.values() {
        left = left.min(*x);
        right = right.max(*x);
        top = top.min(*y);
        bottom = bottom.max(*y);
    }
    /// The least the view shows, so that a small mesh is not blown up.
    const MIN_WIDTH: f64 = 440.0;
    const MIN_HEIGHT: f64 = 280.0;

    let (left, top) = (left - MARGIN, top - 35.0);
    let (right, bottom) = (right + MARGIN, bottom + 60.0);
    let (width, height) = (
        (right - left).max(MIN_WIDTH),
        (bottom - top).max(MIN_HEIGHT),
    );
    let (centre_x, centre_y) = ((left + right) / 2.0, (top + bottom) / 2.0);

    format!(
        "{} {} {width} {height}",
        centre_x - width / 2.0,
        centre_y - height / 2.0
    )
}

/// Spread the nodes out from where `layout` put them: they push each other
/// apart, links pull theirs together (a child closer to its parent), and the
/// border routers keep to the middle.
fn relax(mesh: &ThreadMesh, places: &mut BTreeMap<u64, (f64, f64)>) {
    const ITERATIONS: usize = 300;
    /// The length a link settles at.
    const LINK: f64 = 95.0;

    let ids: Vec<u64> = places.keys().copied().collect();
    let index: BTreeMap<u64, usize> = ids.iter().enumerate().map(|(i, id)| (*id, i)).collect();
    let mut at: Vec<(f64, f64)> = ids.iter().map(|id| places[id]).collect();
    let middle: Vec<bool> = ids
        .iter()
        .map(|id| {
            mesh.nodes
                .iter()
                .any(|node| node.ext_address == *id && node.kind == MeshNodeKind::BorderRouter)
        })
        .collect();
    let mut links: Vec<(usize, usize, f64)> = mesh
        .links
        .iter()
        .filter_map(|link| {
            let length = if link.parent_child { LINK * 0.6 } else { LINK };
            Some((*index.get(&link.a)?, *index.get(&link.b)?, length))
        })
        .collect();
    // A router no link is known to stays by the router it is reached through.
    for route in &mesh.routes {
        let near = route.via.unwrap_or(route.from);
        if let (Some(&to), Some(&near)) = (index.get(&route.to), index.get(&near)) {
            links.push((to, near, LINK));
        }
    }
    // A child stays by its parent even where no one reported their link.
    for node in &mesh.nodes {
        let (Some(parent), Some(&child)) = (node.parent, index.get(&node.ext_address)) else {
            continue;
        };
        let Some(&parent) = index.get(&parent) else {
            continue;
        };
        if !links
            .iter()
            .any(|&(a, b, _)| (a, b) == (child, parent) || (a, b) == (parent, child))
        {
            links.push((child, parent, LINK * 0.6));
        }
    }

    let count = at.len();
    for step in 0..ITERATIONS {
        let room = 25.0 * (1.0 - step as f64 / ITERATIONS as f64) + 0.5;
        let mut push = vec![(0.0, 0.0); count];

        for i in 0..count {
            for j in i + 1..count {
                let (dx, dy) = (at[i].0 - at[j].0, at[i].1 - at[j].1);
                let distance = dx.hypot(dy).max(1.0);
                let force = LINK * LINK * 0.7 / distance;
                let (ux, uy) = (dx / distance, dy / distance);
                push[i].0 += ux * force;
                push[i].1 += uy * force;
                push[j].0 -= ux * force;
                push[j].1 -= uy * force;
            }
        }
        for &(i, j, length) in &links {
            let (dx, dy) = (at[i].0 - at[j].0, at[i].1 - at[j].1);
            let distance = dx.hypot(dy).max(0.01);
            let force = distance * distance / length;
            let (ux, uy) = (dx / distance, dy / distance);
            push[i].0 -= ux * force;
            push[i].1 -= uy * force;
            push[j].0 += ux * force;
            push[j].1 += uy * force;
        }
        for (i, (x, y)) in at.iter().enumerate() {
            let pull = if middle[i] { 2.5 } else { 0.6 };
            push[i].0 -= x * pull;
            push[i].1 -= y * pull;
        }

        for (i, (dx, dy)) in push.into_iter().enumerate() {
            let length = dx.hypot(dy);
            if length > 0.01 {
                let step = length.min(room);
                at[i].0 += dx / length * step;
                at[i].1 += dy / length * step;
            }
        }
    }

    for (id, place) in ids.into_iter().zip(at) {
        places.insert(id, place);
    }
}

/// How our router reaches a router no link is known to.
fn route_text(mesh: &ThreadMesh, route: &MeshRoute) -> String {
    let label = |ext: u64| {
        mesh.nodes
            .iter()
            .find(|node| node.ext_address == ext)
            .map(|node| node.label.clone())
            .unwrap_or_else(|| format!("{ext:016X}"))
    };
    let via = route
        .via
        .map(|via| format!(" through {}", label(via)))
        .unwrap_or_default();

    format!(
        "{} reaches it{via}, path cost {}",
        label(route.from),
        route.path_cost
    )
}

/// The weaker direction of `link`, as a class.
fn link_class(link: &MeshLink) -> &'static str {
    let worst = [link.a_hears_b, link.b_hears_a]
        .into_iter()
        .flatten()
        .filter_map(|signal| signal.quality)
        .min();

    worst.map(quality_class).unwrap_or("q-unknown")
}

fn quality_class(quality: u8) -> &'static str {
    match quality {
        3.. => "q3",
        2 => "q2",
        1 => "q1",
        0 => "q0",
    }
}

fn one_way(link: &MeshLink) -> bool {
    let heard = |signal: Option<LinkSignal>| signal.and_then(|s| s.quality).map(|q| q > 0);

    matches!(
        (heard(link.a_hears_b), heard(link.b_hears_a)),
        (Some(true), Some(false)) | (Some(false), Some(true))
    )
}

fn link_title(mesh: &ThreadMesh, link: &MeshLink) -> String {
    let label = |ext: u64| {
        mesh.nodes
            .iter()
            .find(|node| node.ext_address == ext)
            .map(|node| node.label.clone())
            .unwrap_or_default()
    };
    let (a, b) = (label(link.a), label(link.b));
    let side =
        |signal: Option<LinkSignal>| signal.map(signal_text).unwrap_or("not reported".into());

    format!(
        "{a} hears {b}: {}\n{b} hears {a}: {}",
        side(link.a_hears_b),
        side(link.b_hears_a)
    )
}

fn signal_text(signal: LinkSignal) -> String {
    match (signal.quality, signal.rssi) {
        (Some(q), Some(rssi)) => format!("{q}/3 · {rssi} dBm"),
        (Some(q), None) => format!("{q}/3"),
        (None, Some(rssi)) => format!("{rssi} dBm"),
        (None, None) => "heard".into(),
    }
}

fn node_class(node: &MeshNode) -> &'static str {
    match node.kind {
        MeshNodeKind::Device { .. } => "is-device",
        MeshNodeKind::BorderRouter => "is-router",
        MeshNodeKind::OtherFabric { .. } | MeshNodeKind::Unknown => "is-other",
    }
}

/// A letter for its role, on the map.
fn role_mark(node: &MeshNode) -> &'static str {
    match node.role {
        Some(ThreadRole::Leader) => "L",
        Some(ThreadRole::Router) => "R",
        Some(ThreadRole::Reed) => "E",
        Some(ThreadRole::EndDevice) => "E",
        Some(ThreadRole::SleepyEndDevice) => "S",
        _ if node.sleepy() => "S",
        _ => "?",
    }
}

/// How a node sleeps: its ICD idle time, and how often it polls its parent.
fn sleep_text(sleep: &MeshSleep) -> String {
    let mut parts = Vec::new();
    match sleep.icd_mode {
        Some(IcdMode::Lit) => parts.push("long idle time".to_string()),
        Some(IcdMode::Sit) => parts.push("short idle time".to_string()),
        None => {}
    }
    match (sleep.idle_interval_ms, sleep.active_interval_ms) {
        (Some(idle), Some(active)) => parts.push(format!(
            "polls every {} idle, {} active",
            duration_ms(idle),
            duration_ms(active)
        )),
        (Some(idle), None) => parts.push(format!("polls every {} idle", duration_ms(idle))),
        _ => {}
    }
    if let Some(secs) = sleep.idle_mode_secs {
        parts.push(format!(
            "idle for {} at a time",
            duration_ms(secs.saturating_mul(1000))
        ));
    }

    parts.join(", ")
}

/// A span of milliseconds, in the largest unit that fits.
fn duration_ms(ms: u32) -> String {
    match ms {
        0..1_000 => format!("{ms} ms"),
        1_000..60_000 if ms.is_multiple_of(1_000) => format!("{} s", ms / 1_000),
        1_000..60_000 => format!("{:.1} s", ms as f32 / 1_000.0),
        60_000..3_600_000 => format!("{} min", ms / 60_000),
        _ => format!("{} h", ms / 3_600_000),
    }
}

fn role_name(node: &MeshNode) -> &'static str {
    match node.role {
        Some(ThreadRole::Leader) => "leader",
        Some(ThreadRole::Router) => "router",
        Some(ThreadRole::Reed) => "end device (may route)",
        Some(ThreadRole::EndDevice) => "end device",
        Some(ThreadRole::SleepyEndDevice) => "sleepy end device",
        Some(ThreadRole::Unassigned) => "detached",
        Some(ThreadRole::Unspecified) | None => "role unknown",
    }
}

fn kind_name(node: &MeshNode) -> &'static str {
    match node.kind {
        MeshNodeKind::Device { .. } => "on our fabric",
        MeshNodeKind::BorderRouter => "border router",
        MeshNodeKind::OtherFabric { .. } => "Matter, another fabric",
        MeshNodeKind::Unknown => "not named",
    }
}

fn network_name(net: &MeshNetwork) -> String {
    net.name
        .clone()
        .unwrap_or_else(|| format!("{:016X}", net.extended_pan_id))
}

/// At most [`MAP_LABEL_MAX`] characters of `label`.
fn short(label: &str) -> String {
    if label.chars().count() <= MAP_LABEL_MAX {
        return label.to_string();
    }

    let cut: String = label.chars().take(MAP_LABEL_MAX - 1).collect();
    format!("{}…", cut.trim_end())
}

/// How long ago `at` (ms since the Unix epoch) was.
fn ago(at: f64) -> String {
    match ((js_sys::Date::now() - at) / 1000.0) as u64 {
        0..60 => "just now".into(),
        secs @ 60..3600 => format!("{} min ago", secs / 60),
        secs => format!("{} h ago", secs / 3600),
    }
}
