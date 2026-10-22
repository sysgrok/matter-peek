//! The network popup of a device: its connection, the networks it knows (and
//! changes to them), its interfaces, and the diagnostics of its network.

use yew::html::Scope;
use yew::prelude::*;

use matter_peek_core::api::{self, display, wire};

use crate::icons::{icon, kind_class, kind_icon, Icon};
use crate::ui;
use crate::{input_value, send_request, textarea_value, App, Msg};

/// The parts of the popup.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Networks,
    Interfaces,
    Neighbors,
}

/// Labelled facts, and labelled counters.
type Details = Vec<(&'static str, String)>;
type Counters = Vec<(&'static str, u64)>;

/// The network popup of one device.
pub struct NetworkView {
    pub node_id: u64,
    pub loading: bool,
    /// Whether a change is being made (else a read, while loading).
    pub changing: bool,
    pub info: Option<api::NetworkInfo>,
    /// The tab picked, if any.
    pub tab: Option<Tab>,
    /// Whether the form adding a network is open.
    pub adding: bool,
    pub ssid: String,
    pub password: String,
    pub show_password: bool,
    pub dataset: String,
    /// A change awaiting confirmation.
    pub confirm: Option<Confirm>,
}

impl NetworkView {
    pub fn new(node_id: u64) -> Self {
        Self {
            node_id,
            loading: true,
            changing: false,
            info: None,
            tab: None,
            adding: false,
            ssid: String::new(),
            password: String::new(),
            show_password: false,
            dataset: String::new(),
            confirm: None,
        }
    }

    /// Make `change` to the networks on `endpoint`.
    pub fn change(&mut self, link: &Scope<App>, endpoint: u16, change: api::NetworkChange) {
        self.loading = true;
        self.changing = true;
        self.confirm = None;
        send_request(
            link.clone(),
            wire::Request::ChangeNetwork {
                node_id: self.node_id,
                endpoint,
                change,
            },
        );
    }

    /// The change made: the form is done with.
    pub fn changed(&mut self) {
        self.adding = false;
        self.ssid.clear();
        self.password.clear();
        self.dataset.clear();
        self.show_password = false;
    }
}

/// A change to confirm first, and what it may cost the device.
pub struct Confirm {
    pub endpoint: u16,
    pub change: api::NetworkChange,
    pub title: String,
    pub text: String,
    pub action: &'static str,
    pub danger: bool,
}

impl Confirm {
    /// The confirmation `change` needs, or `change` back when it leaves the
    /// device's connection alone.
    pub fn of(
        info: Option<&api::NetworkInfo>,
        endpoint: u16,
        change: api::NetworkChange,
    ) -> Result<Self, api::NetworkChange> {
        let config = info.and_then(|info| info.configs.iter().find(|c| c.endpoint == endpoint));
        let name = |id: &[u8]| {
            config
                .and_then(|config| config.networks.iter().find(|network| network.id == id))
                .map_or_else(
                    || "this network".to_string(),
                    |network| network.name.clone(),
                )
        };

        let (title, text, action, danger) = match &change {
            api::NetworkChange::Connect { network_id } => {
                let name = name(network_id);
                (
                    format!("Connect to {name}?"),
                    match config.and_then(|config| config.connect_max_time_secs) {
                        Some(secs) => format!(
                            "The device leaves its network to join {name}, which takes it up to \
                             {secs} s. Should it not be found there in time, it goes back to its \
                             previous network within a few minutes."
                        ),
                        None => format!(
                            "The device leaves its network to join {name}. Should it not be found \
                             there in time, it goes back to its previous network within a few \
                             minutes."
                        ),
                    },
                    "Connect",
                    false,
                )
            }
            api::NetworkChange::Remove { network_id } => (
                format!("Remove {}?", name(network_id)),
                "The device forgets this network, and will no longer join it.".to_string(),
                "Remove",
                true,
            ),
            api::NetworkChange::AddOrUpdate {
                credentials: api::NetworkCredentials::Wifi { ssid, .. },
            } if config.is_some_and(|config| knows(config, ssid)) => (
                format!("Replace the credentials of {ssid}?"),
                "Should the device be on this network, it reconnects with the new credentials, \
                 and keeps them only if that works."
                    .to_string(),
                "Replace",
                false,
            ),
            // The network of a dataset is in the dataset.
            api::NetworkChange::AddOrUpdate {
                credentials: api::NetworkCredentials::Thread { .. },
            } => (
                "Add or update this Thread network?".to_string(),
                "Should the device know this Thread network, its dataset is replaced; should \
                 the device be on it, it rejoins with the new one, and keeps it only if that \
                 works."
                    .to_string(),
                "Continue",
                false,
            ),
            _ => return Err(change),
        };

        Ok(Self {
            endpoint,
            change,
            title,
            text,
            action,
            danger,
        })
    }
}

/// The popup of `view`, for the device titled `title`.
pub fn view_modal(view: &NetworkView, title: &str, error: Option<&str>, link: &Scope<App>) -> Html {
    let busy = if view.changing {
        Some("Changing the networks of the device… When it has to reconnect, this takes up to a few minutes.")
    } else if view.loading && view.info.is_none() {
        Some("Reading the network…")
    } else {
        None
    };

    let body = view.info.as_ref().map(|info| {
        let tabs = tabs(info);
        let tab = view
            .tab
            .filter(|tab| tabs.iter().any(|(t, ..)| t == tab))
            .or_else(|| tabs.first().map(|(tab, ..)| *tab));

        let content = match tab {
            Some(Tab::Networks) => html! {
                { for info.configs.iter().map(|config| known_networks(config, view, link)) }
            },
            Some(Tab::Interfaces) => interfaces(info),
            Some(Tab::Neighbors) => info.thread.as_ref().map(neighbors).unwrap_or_default(),
            None => html! {},
        };

        html! {
            <>
                { hero(info) }
                <div class="tabs net-tabs mt-5">
                    <ul>
                        { for tabs.iter().map(|&(t, label, glyph, count)| html! {
                            <li class={classes!((Some(t) == tab).then_some("is-active"))}>
                                <a onclick={link.callback(move |_| Msg::NetworkTab(t))}>
                                    <span class="icon is-small">{ icon(glyph, 16) }</span>
                                    <span>{ label }</span>
                                    if let Some(count) = count {
                                        <span class="net-count">{ count }</span>
                                    }
                                </a>
                            </li>
                        }) }
                    </ul>
                </div>
                { content }
            </>
        }
    });

    html! {
        <>
            <div class="modal is-active">
                <div class="modal-background"></div>
                <div class="modal-card is-fullscreen">
                    { ui::modal_head("Network", title, html! {
                        <button
                            class={classes!("button", "is-small", view.loading.then_some("is-loading"))}
                            title="Read the network again"
                            disabled={view.loading}
                            onclick={link.callback(|_| Msg::RefreshNetwork)}>
                            <span class="icon is-small">{ icon(Icon::Refresh, 16) }</span>
                            <span>{ "Refresh" }</span>
                        </button>
                    }, link.callback(|_| Msg::CloseNetwork)) }
                    <section class="modal-card-body">
                        <div class="app container">
                            if let Some(e) = error {
                                <div class="notification is-danger is-light">
                                    <button class="delete" onclick={link.callback(|_| Msg::DismissError)}></button>
                                    { e.to_string() }
                                </div>
                            }
                            if let Some(busy) = busy {
                                <div class="mb-4">
                                    <progress class="progress is-small is-link mb-2" max="100"></progress>
                                    <p class="is-size-7 net-weak">{ busy }</p>
                                </div>
                            }
                            { for body }
                        </div>
                    </section>
                </div>
            </div>
            { for view.confirm.as_ref().map(|confirm| confirm_dialog(confirm, link)) }
        </>
    }
}

/// The tabs with something to show: tab, label, icon and count.
fn tabs(info: &api::NetworkInfo) -> Vec<(Tab, &'static str, Icon, Option<usize>)> {
    let mut tabs = Vec::new();

    if !info.configs.is_empty() {
        let count = info
            .configs
            .iter()
            .map(|config| config.networks.len())
            .sum();
        let glyph = kind_icon(primary_kind(info)).unwrap_or(Icon::Wifi);
        tabs.push((Tab::Networks, "Networks", glyph, Some(count)));
    }
    tabs.push((
        Tab::Interfaces,
        "Interfaces",
        Icon::Interfaces,
        Some(info.interfaces.len()),
    ));
    if let Some(thread) = info.thread.as_ref().filter(|t| !t.neighbors.is_empty()) {
        tabs.push((
            Tab::Neighbors,
            "Neighbors",
            Icon::Neighbors,
            Some(thread.neighbors.len()),
        ));
    }

    tabs
}

/// The device's connection at a glance: its network, link quality and
/// addresses.
fn hero(info: &api::NetworkInfo) -> Html {
    let config = primary_config(info);
    let kind = primary_kind(info);
    let iface = info
        .interfaces
        .iter()
        .find(|iface| Some(iface.kind) == kind)
        .or(info.interfaces.first());
    let online = iface.is_some_and(|iface| iface.operational);
    let kind_label = kind.map_or("Network", |kind| kind.label());

    let connected = config
        .and_then(|config| config.networks.iter().find(|network| network.connected))
        .map(|network| network.name.clone());
    let headline = match kind {
        Some(api::NetworkKind::Thread) => info
            .thread
            .as_ref()
            .and_then(|thread| thread.network_name.clone())
            .or(connected),
        _ => connected,
    }
    .unwrap_or_else(|| kind_label.to_string());

    let subline = match iface {
        Some(iface) => format!("{kind_label} · interface {}", iface.name),
        None => kind_label.to_string(),
    };

    let mut chips = Vec::new();
    if let Some(wifi) = &info.wifi {
        if let Some(rssi) = wifi.rssi {
            let (level, quality) = wifi_level(rssi);
            chips.push(html! {
                <span class={classes!("net-chip", level_class(level, 4))} title="Signal strength">
                    { signal_bars(level, 4) }
                    { format!("{rssi} dBm · {quality}") }
                </span>
            });
        }
        if let Some(channel) = wifi.channel {
            chips.push(chip(Icon::Channel, format!("Channel {channel}"), "Channel"));
        }
        if let Some(security) = wifi.security {
            chips.push(chip(Icon::Lock, security.label().to_string(), "Security"));
        }
        if let Some(version) = wifi.version {
            chips.push(chip(Icon::Wifi, version.label().to_string(), "Standard"));
        }
        if let Some(rate) = wifi.current_max_rate {
            chips.push(chip(Icon::Gauge, display::bit_rate(rate), "Max rate"));
        }
        if let Some(bssid) = &wifi.bssid {
            chips.push(chip(Icon::Router, bssid.clone(), "Access point"));
        }
    }
    if let Some(thread) = &info.thread {
        if let Some(role) = thread.role {
            chips.push(chip(Icon::Thread, role.label().to_string(), "Role"));
        }
        if let Some(channel) = thread.channel {
            chips.push(chip(Icon::Channel, format!("Channel {channel}"), "Channel"));
        }
        if let Some(pan_id) = thread.pan_id {
            chips.push(chip(
                Icon::Router,
                format!("PAN {}", display::hex_u16(pan_id)),
                "PAN ID",
            ));
        }
        if !thread.neighbors.is_empty() {
            let count = thread.neighbors.len();
            chips.push(chip(
                Icon::Neighbors,
                format!("{count} neighbor{}", if count == 1 { "" } else { "s" }),
                "Neighbors",
            ));
        }
    }
    if let Some(eth) = &info.ethernet {
        if let Some(mbps) = eth.phy_rate_mbps {
            chips.push(chip(Icon::Gauge, format!("{mbps} Mbit/s"), "Speed"));
        }
        if let Some(full) = eth.full_duplex {
            let duplex = if full { "Full duplex" } else { "Half duplex" };
            chips.push(chip(Icon::ArrowsUpDown, duplex.to_string(), "Duplex"));
        }
        if let Some(carrier) = eth.carrier_detect {
            let link = if carrier { "Link" } else { "No link" };
            chips.push(chip(Icon::Ethernet, link.to_string(), "Carrier"));
        }
    }

    let addresses = iface.map(|iface| {
        let mut addresses = iface.ipv4_addresses.clone();
        // The addresses reachable from afar first.
        let mut ipv6 = iface.ipv6_addresses.clone();
        ipv6.sort_by_key(|addr| address_kind(addr) == "Link-local");
        addresses.extend(ipv6);

        html! {
            <div class="net-hero-addrs">
                { for addresses.iter().map(|addr| html! {
                    <span class="mono">{ addr.clone() }</span>
                }) }
            </div>
        }
    });

    html! {
        <div class="box net-hero">
            <div class={classes!("net-hero-icon", kind_class(kind))}>
                { kind_icon(kind).map(|glyph| icon(glyph, 30)).unwrap_or_default() }
            </div>
            <div class="net-hero-main">
                <div class="net-hero-title">
                    <span class="title is-4 mb-0">{ headline }</span>
                    if online {
                        <span class="pill is-up">{ "Online" }</span>
                    } else {
                        <span class="pill">{ "Offline" }</span>
                    }
                </div>
                <p class="net-weak">{ subline }</p>
                if !chips.is_empty() {
                    <div class="net-chips mt-3">{ for chips }</div>
                }
                { for addresses }
            </div>
        </div>
    }
}

/// The networks an interface knows, in the order it tries them, with their
/// changes and the form adding one.
fn known_networks(config: &api::NetworkConfig, view: &NetworkView, link: &Scope<App>) -> Html {
    let endpoint = config.endpoint;
    let kind = config.kind;
    let count = config.networks.len();
    // Only Wi-Fi and Thread interfaces are given networks.
    let changeable = matches!(
        kind,
        Some(api::NetworkKind::WiFi | api::NetworkKind::Thread)
    );
    let kind_label = kind.map_or("Network", |kind| kind.label());

    let action = |glyph: Icon,
                  title: &str,
                  class: Option<&'static str>,
                  disabled: bool,
                  change: api::NetworkChange| {
        html! {
            <button class={classes!("icon-action", class)} title={title.to_string()}
                disabled={view.loading || disabled}
                onclick={link.callback(move |_| Msg::ChangeNetwork(endpoint, change.clone()))}>
                { icon(glyph, 18) }
            </button>
        }
    };

    // The device's last connection attempt, where it failed.
    let failed = config
        .last_status
        .filter(|status| *status != api::NetworkStatus::Success)
        .zip(config.last_network_id.as_deref());

    let rows = config.networks.iter().enumerate().map(|(index, network)| {
        let id = network.id.clone();
        // A Thread network known by name shows its extended PAN ID too.
        let xpan = (kind == Some(api::NetworkKind::Thread))
            .then(|| network.id.iter().map(|b| format!("{b:02X}")).collect::<String>());
        let named = xpan.as_deref().is_none_or(|xpan| network.name != xpan);
        let failed = failed
            .filter(|(_, last)| *last == network.id.as_slice())
            .map(|(status, _)| status);
        let reorder = |to: usize| api::NetworkChange::Reorder {
            network_id: id.clone(),
            index: to as u8,
        };

        html! {
            <div class="net-row">
                <span class="net-rank" title="Tried in this order">{ index + 1 }</span>
                <span class={classes!("net-row-icon", kind_class(kind))}>
                    { kind_icon(kind).map(|glyph| icon(glyph, 18)).unwrap_or_default() }
                </span>
                <div class="net-name">
                    <span class={classes!("net-name-text", (!named).then_some("mono"))}>
                        { network.name.clone() }
                    </span>
                    if let Some(xpan) = xpan.filter(|_| named) {
                        <span class="net-name-id mono" title="Extended PAN ID">{ xpan }</span>
                    }
                    if network.connected {
                        <span class="pill is-up" title="As the device reports it">{ "Connected" }</span>
                    }
                    if let Some(status) = failed {
                        <span class="pill is-warning"
                            title={match config.last_connect_error {
                                Some(e) => format!("The last attempt to connect failed (error {e})"),
                                None => "The last attempt to connect failed".to_string(),
                            }}>
                            { status.label() }
                        </span>
                    }
                </div>
                if changeable {
                    <div class="net-actions">
                        { action(Icon::ArrowUp, "Try it earlier", None, index == 0, reorder(index.saturating_sub(1))) }
                        { action(Icon::ArrowDown, "Try it later", None, index + 1 >= count, reorder(index + 1)) }
                        { action(Icon::Connect,
                            if network.connected { "Connected to it" } else { "Connect to it" },
                            None, network.connected,
                            api::NetworkChange::Connect { network_id: id.clone() }) }
                        { action(Icon::Trash, "Remove it", Some("is-danger"), false,
                            api::NetworkChange::Remove { network_id: id.clone() }) }
                    </div>
                }
            </div>
        }
    });

    html! {
        <div class="net-section">
            <div class="net-section-head">
                <div>
                    <p class="net-section-title">
                        { format!("{kind_label} networks") }
                        if endpoint != 0 {
                            <span class="tag is-rounded ml-2" title={format!("On endpoint {endpoint}")}>
                                { "secondary interface" }
                            </span>
                        }
                        if config.interface_enabled == Some(false) {
                            <span class="tag is-warning is-light is-rounded ml-2">{ "interface disabled" }</span>
                        }
                    </p>
                    if count > 1 {
                        <p class="is-size-7 net-weak">{ "The device tries them from the top." }</p>
                    }
                </div>
                if let Some(max) = config.max_networks {
                    { slots(count, usize::from(max)) }
                }
            </div>

            <div class="net-list">
                { for rows }
                if count == 0 {
                    <div class="net-row net-weak">{ "The device knows no networks." }</div>
                }
                if changeable {
                    if view.adding {
                        { add_form(config, view, link) }
                    } else {
                        <button class="net-row net-add" disabled={view.loading}
                            onclick={link.callback(|_| Msg::ToggleAddNetwork)}>
                            <span class="icon">{ icon(Icon::Plus, 18) }</span>
                            <span>{ format!("Add a {kind_label} network") }</span>
                        </button>
                    }
                }
            </div>

        </div>
    }
}

/// The form adding a network, or updating a known one.
fn add_form(config: &api::NetworkConfig, view: &NetworkView, link: &Scope<App>) -> Html {
    let endpoint = config.endpoint;
    let Some(kind) = config.kind else {
        return html! {};
    };
    let full = config
        .max_networks
        .is_some_and(|max| config.networks.len() >= usize::from(max));
    let ssid = view.ssid.trim();
    let known = kind == api::NetworkKind::WiFi && knows(config, ssid);
    let ready = match kind {
        api::NetworkKind::Thread => !view.dataset.trim().is_empty(),
        _ => !ssid.is_empty() && (known || !full),
    };

    let fields = match kind {
        api::NetworkKind::Thread => html! {
            <div class="field">
                <label class="label is-small">{ "Operational dataset" }</label>
                <div class="control">
                    <textarea class="textarea mono" rows="3"
                        placeholder="0e080000000000010000000300000f35060004001fffe0…"
                        value={view.dataset.clone()}
                        oninput={link.callback(|e: InputEvent| Msg::NetworkDataset(textarea_value(e)))} />
                </div>
                <p class="help">{ "In hex, as `ot-ctl dataset active -x` prints it." }</p>
            </div>
        },
        _ => html! {
            <div class="columns is-variable is-3 mb-0">
                <div class="column">
                    <div class="field">
                        <label class="label is-small">{ "Network name (SSID)" }</label>
                        <div class="control has-icons-left">
                            <input class="input" type="text" placeholder="My Wi-Fi"
                                value={view.ssid.clone()}
                                oninput={link.callback(|e: InputEvent| Msg::NetworkSsid(input_value(e)))} />
                            <span class="icon is-left">{ icon(Icon::Wifi, 18) }</span>
                        </div>
                    </div>
                </div>
                <div class="column">
                    <div class="field">
                        <label class="label is-small">{ "Password" }</label>
                        <div class="field has-addons">
                            <div class="control has-icons-left is-expanded">
                                <input class="input"
                                    type={if view.show_password { "text" } else { "password" }}
                                    value={view.password.clone()}
                                    oninput={link.callback(|e: InputEvent| Msg::NetworkPassword(input_value(e)))} />
                                <span class="icon is-left">{ icon(Icon::Lock, 18) }</span>
                            </div>
                            <div class="control">
                                <button class="button"
                                    title={if view.show_password { "Hide the password" } else { "Show the password" }}
                                    onclick={link.callback(|_| Msg::ToggleShowPassword)}>
                                    <span class="icon">
                                        { icon(if view.show_password { Icon::EyeOff } else { Icon::Eye }, 18) }
                                    </span>
                                </button>
                            </div>
                        </div>
                    </div>
                </div>
            </div>
        },
    };

    let hint = if known {
        Some((
            "is-warning",
            format!("The device knows {ssid}: its credentials will be replaced."),
        ))
    } else if full {
        Some((
            "is-danger",
            "The device has no room for another network: only known ones can be updated."
                .to_string(),
        ))
    } else {
        None
    };

    html! {
        <div class="net-row net-form">
            { fields }
            if let Some((class, hint)) = hint {
                <p class={classes!("help", class)}>{ hint }</p>
            }
            <div class="buttons mt-3">
                <button class="button is-link" disabled={view.loading || !ready}
                    onclick={link.callback(move |_| Msg::AddNetwork(endpoint, kind))}>
                    <span class="icon">{ icon(if known { Icon::Refresh } else { Icon::Plus }, 18) }</span>
                    <span>{ if known { "Update network" } else { "Add network" } }</span>
                </button>
                <button class="button is-text" onclick={link.callback(|_| Msg::ToggleAddNetwork)}>
                    { "Cancel" }
                </button>
            </div>
        </div>
    }
}

/// The device's network interfaces, each with what the diagnostics of its
/// network tell.
fn interfaces(info: &api::NetworkInfo) -> Html {
    let mut cards = info
        .interfaces
        .iter()
        .enumerate()
        .map(|(index, iface)| {
            // The diagnostics of a kind go to its first interface.
            let first = info
                .interfaces
                .iter()
                .position(|other| other.kind == iface.kind)
                == Some(index);
            let (details, counters) = if first {
                diagnostics(info, iface.kind)
            } else {
                Default::default()
            };
            interface_card(Some(iface), iface.kind, details, counters)
        })
        .collect::<Vec<_>>();

    // Diagnostics of a network no listed interface is on.
    for kind in [
        api::NetworkKind::WiFi,
        api::NetworkKind::Thread,
        api::NetworkKind::Ethernet,
    ] {
        if !info.interfaces.iter().any(|iface| iface.kind == kind) {
            let (details, counters) = diagnostics(info, kind);
            if !details.is_empty() || !counters.is_empty() {
                cards.push(interface_card(None, kind, details, counters));
            }
        }
    }

    if cards.is_empty() {
        return html! { <p class="net-weak">{ "The device reports no interfaces." }</p> };
    }
    let width = if cards.len() == 1 {
        "is-full"
    } else {
        "is-half"
    };

    html! {
        <div class="columns is-multiline">
            { for cards.into_iter().map(|card| html! {
                <div class={classes!("column", width)}>{ card }</div>
            }) }
        </div>
    }
}

fn interface_card(
    iface: Option<&api::NetworkInterfaceInfo>,
    kind: api::NetworkKind,
    details: Details,
    counters: Counters,
) -> Html {
    let off_premise = iface
        .map(|iface| {
            [
                ("IPv4", iface.off_premise_ipv4),
                ("IPv6", iface.off_premise_ipv6),
            ]
            .into_iter()
            .filter_map(|(family, reachable)| reachable.map(|r| (family, r)))
            .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let hw_label = if kind == api::NetworkKind::Thread {
        "Extended address"
    } else {
        "MAC"
    };

    html! {
        <div class="box net-card">
            <div class="net-card-head">
                <span class={classes!("net-row-icon", kind_class(Some(kind)))}>
                    { kind_icon(Some(kind)).map(|glyph| icon(glyph, 18)).unwrap_or_default() }
                </span>
                <div class="net-name">
                    <span class="net-name-text">
                        { iface.map_or_else(|| kind.label().to_string(), |iface| iface.name.clone()) }
                    </span>
                    if iface.is_some() {
                        <span class="is-size-7 net-weak">{ kind.label() }</span>
                    }
                </div>
                if let Some(iface) = iface {
                    if iface.operational {
                        <span class="pill is-up">{ "Up" }</span>
                    } else {
                        <span class="pill">{ "Down" }</span>
                    }
                }
            </div>
            <dl class="net-kv mt-3">
                if let Some(iface) = iface.filter(|iface| !iface.hardware_address.is_empty()) {
                    <dt>{ hw_label }</dt>
                    <dd class="mono">{ iface.hardware_address.clone() }</dd>
                }
                { for off_premise.iter().map(|(family, reachable)| html! {
                    <>
                        <dt>{ format!("Internet ({family})") }</dt>
                        <dd>{ if *reachable { "reachable" } else { "not reachable" } }</dd>
                    </>
                }) }
                { for details.into_iter().map(|(label, value)| html! {
                    <>
                        <dt>{ label }</dt>
                        <dd>{ value }</dd>
                    </>
                }) }
            </dl>
            if let Some(iface) = iface {
                <div class="net-addr-list mt-3">
                    { for iface.ipv4_addresses.iter().map(|addr| address(addr, "IPv4")) }
                    { for iface.ipv6_addresses.iter().map(|addr| address(addr, address_kind(addr))) }
                </div>
            }
            if !counters.is_empty() {
                <div class="net-stats is-compact mt-4">
                    { for counters.into_iter().map(|(label, value)| html! {
                        <div class="net-stat">
                            <p class="net-stat-label">{ label }</p>
                            <p class="net-stat-value">{ count(value) }</p>
                        </div>
                    }) }
                </div>
            }
        </div>
    }
}

fn address(addr: &str, kind: &str) -> Html {
    html! {
        <div class="net-addr">
            <span class="tag is-rounded">{ kind.to_string() }</span>
            <span class="mono">{ addr.to_string() }</span>
        </div>
    }
}

/// What the diagnostics of the device's `kind` network tell beyond the
/// summary: details and counters, as far as the device reports them.
fn diagnostics(info: &api::NetworkInfo, kind: api::NetworkKind) -> (Details, Counters) {
    let (details, counters) = match kind {
        api::NetworkKind::WiFi => {
            let Some(wifi) = &info.wifi else {
                return Default::default();
            };
            (
                vec![],
                vec![
                    ("Beacons received", wifi.beacon_rx_count.map(u64::from)),
                    ("Beacons lost", wifi.beacon_lost_count.map(u64::from)),
                    (
                        "Unicast received",
                        wifi.packet_unicast_rx_count.map(u64::from),
                    ),
                    ("Unicast sent", wifi.packet_unicast_tx_count.map(u64::from)),
                    (
                        "Multicast received",
                        wifi.packet_multicast_rx_count.map(u64::from),
                    ),
                    (
                        "Multicast sent",
                        wifi.packet_multicast_tx_count.map(u64::from),
                    ),
                    ("Overruns", wifi.overrun_count),
                ],
            )
        }
        api::NetworkKind::Thread => {
            let Some(thread) = &info.thread else {
                return Default::default();
            };
            (
                vec![
                    ("Network", thread.network_name.clone()),
                    (
                        "Extended PAN ID",
                        thread.extended_pan_id.map(display::hex_u64),
                    ),
                    ("Mesh-local prefix", thread.mesh_local_prefix.clone()),
                    ("RLOC16", thread.rloc16.map(display::hex_u16)),
                    ("Partition", thread.partition_id.map(|id| id.to_string())),
                    (
                        "Leader router",
                        thread.leader_router_id.map(|id| id.to_string()),
                    ),
                ],
                vec![
                    ("Routes", thread.route_count.map(u64::from)),
                    (
                        "Attach attempts",
                        thread.attach_attempt_count.map(u64::from),
                    ),
                    ("Parent changes", thread.parent_change_count.map(u64::from)),
                    (
                        "Partition changes",
                        thread.partition_id_change_count.map(u64::from),
                    ),
                    ("Times detached", thread.detached_role_count.map(u64::from)),
                ],
            )
        }
        api::NetworkKind::Ethernet => {
            let Some(eth) = &info.ethernet else {
                return Default::default();
            };
            (
                vec![("Up for", eth.time_since_reset_secs.map(display::duration))],
                vec![
                    ("Packets received", eth.packet_rx_count),
                    ("Packets sent", eth.packet_tx_count),
                    ("Send errors", eth.tx_err_count),
                    ("Collisions", eth.collision_count),
                    ("Overruns", eth.overrun_count),
                ],
            )
        }
        _ => return Default::default(),
    };

    (known(details), known(counters))
}

/// The `facts` the device reports.
fn known<T>(facts: Vec<(&'static str, Option<T>)>) -> Vec<(&'static str, T)> {
    facts
        .into_iter()
        .filter_map(|(label, value)| value.map(|value| (label, value)))
        .collect()
}

/// The Thread devices around the device.
fn neighbors(thread: &api::ThreadDiag) -> Html {
    fn dbm(rssi: Option<i8>) -> String {
        rssi.map_or_else(|| "—".to_string(), |rssi| format!("{rssi} dBm"))
    }

    html! {
        <div class="table-container">
            <table class="table is-fullwidth is-hoverable net-table">
                <thead>
                    <tr>
                        <th>{ "Neighbor" }</th>
                        <th>{ "Kind" }</th>
                        <th>{ "Link" }</th>
                        <th>{ "Signal" }</th>
                        <th>{ "Errors" }</th>
                        <th>{ "Last heard" }</th>
                    </tr>
                </thead>
                <tbody>
                    { for thread.neighbors.iter().map(|n| html! {
                        <tr>
                            <td>
                                <div class="mono">{ display::hex_u64(n.ext_address) }</div>
                                <div class="mono is-size-7 net-weak">{ format!("RLOC16 {}", display::hex_u16(n.rloc16)) }</div>
                            </td>
                            <td>
                                <span class="tag is-rounded">{ neighbor_kind(n) }</span>
                                if !n.rx_on_when_idle {
                                    <span class="tag is-rounded is-light ml-1">{ "sleepy" }</span>
                                }
                            </td>
                            <td class={level_class(n.lqi, 3)} title={format!("Link quality {} of 3", n.lqi)}>
                                { signal_bars(n.lqi, 3) }
                            </td>
                            <td class="is-size-7">
                                <div>{ format!("avg {}", dbm(n.average_rssi)) }</div>
                                <div class="net-weak">{ format!("last {}", dbm(n.last_rssi)) }</div>
                            </td>
                            <td class="is-size-7">
                                <div>{ format!("frames {} %", n.frame_error_rate) }</div>
                                <div class="net-weak">{ format!("messages {} %", n.message_error_rate) }</div>
                            </td>
                            <td>{ format!("{} ago", display::duration(u64::from(n.age_secs))) }</td>
                        </tr>
                    }) }
                </tbody>
            </table>
        </div>
    }
}

/// What a Thread neighbor is to the device.
fn neighbor_kind(n: &api::ThreadNeighbor) -> &'static str {
    match (n.is_child, n.full_thread_device) {
        (true, _) => "child",
        (false, true) => "router",
        (false, false) => "parent",
    }
}

fn confirm_dialog(confirm: &Confirm, link: &Scope<App>) -> Html {
    ui::confirm_dialog(
        &confirm.title,
        &confirm.text,
        confirm.action,
        confirm.danger,
        link.callback(|_| Msg::ConfirmNetworkChange),
        link.callback(|_| Msg::CancelNetworkChange),
    )
}

/// How full the device's list of networks is, as dots.
fn slots(used: usize, max: usize) -> Html {
    html! {
        <div class="net-slots" title={format!("{used} of {max} networks")}>
            <span class="net-slot-dots">
                { for (0..max).map(|i| html! {
                    <i class={classes!((i < used).then_some("is-used"))}></i>
                }) }
            </span>
            <span class="is-size-7 net-weak">{ format!("{used} of {max}") }</span>
        </div>
    }
}

fn chip(glyph: Icon, text: String, title: &str) -> Html {
    html! {
        <span class="net-chip" title={title.to_string()}>
            { icon(glyph, 15) }
            { text }
        </span>
    }
}

/// `level` of `max` bars.
fn signal_bars(level: u8, max: u8) -> Html {
    html! {
        <span class="net-signal">
            { for (1..=max).map(|bar| html! {
                <i class={classes!((bar <= level).then_some("is-on"))}
                    style={format!("height: {}%", 100 * u32::from(bar) / u32::from(max))}></i>
            }) }
        </span>
    }
}

/// The bars and word for a Wi-Fi signal.
fn wifi_level(rssi: i8) -> (u8, &'static str) {
    match rssi {
        -55.. => (4, "excellent"),
        -67.. => (3, "good"),
        -75.. => (2, "fair"),
        _ => (1, "weak"),
    }
}

fn level_class(level: u8, max: u8) -> &'static str {
    match level * 4 / max.max(1) {
        3.. => "has-text-success",
        2 => "has-text-warning-dark",
        1 => "has-text-danger",
        _ => "net-weak",
    }
}

/// What kind of IPv6 address `addr` is.
fn address_kind(addr: &str) -> &'static str {
    let addr = addr.to_ascii_lowercase();
    if addr.starts_with("fe8")
        || addr.starts_with("fe9")
        || addr.starts_with("fea")
        || addr.starts_with("feb")
    {
        "Link-local"
    } else if addr.starts_with("fc") || addr.starts_with("fd") {
        "Unique local"
    } else if addr.starts_with("ff") {
        "Multicast"
    } else {
        "Global"
    }
}

/// `n` with its thousands apart.
fn count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push('\u{202F}');
        }
        out.push(digit);
    }
    out
}

/// Whether `config` knows the Wi-Fi network `ssid`.
fn knows(config: &api::NetworkConfig, ssid: &str) -> bool {
    config
        .networks
        .iter()
        .any(|network| network.id == ssid.as_bytes())
}

/// The Network Commissioning instance of the primary interface.
fn primary_config(info: &api::NetworkInfo) -> Option<&api::NetworkConfig> {
    info.configs
        .iter()
        .find(|config| config.endpoint == 0)
        .or(info.configs.first())
}

fn primary_kind(info: &api::NetworkInfo) -> Option<api::NetworkKind> {
    primary_config(info)
        .and_then(|config| config.kind)
        .or_else(|| info.interfaces.first().map(|iface| iface.kind))
}
