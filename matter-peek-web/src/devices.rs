//! The Devices page: the devices on our fabric, each with its live health,
//! tags, update and actions; and the logs popup.

use yew::html::Scope;
use yew::prelude::*;

use matter_peek_core::api::{self, display};

use crate::icons::{icon, kind_class, kind_icon, Icon};
use crate::ui;
use crate::{
    input_value, log_intent_file, log_intent_name, network, select_value, App, Ask, LiveSub,
    Loading, LogsView, Msg, SubConn, Tab,
};

impl App {
    /// The devices on our fabric.
    pub(crate) fn view_devices_tab(&self, link: &Scope<Self>) -> Html {
        let filter = self.tag_filter.trim().to_lowercase();
        let visible = self
            .stored
            .iter()
            .filter(|d| {
                filter.is_empty() || d.tags.iter().any(|t| t.to_lowercase().contains(&filter))
            })
            .collect::<Vec<_>>();

        let count = self.stored.len();
        let live = self
            .stored
            .iter()
            .filter(|d| {
                matches!(
                    self.subs.get(&d.handle.node_id).map(|sub| sub.conn),
                    Some(SubConn::Live { .. })
                )
            })
            .count();
        let fabric = self
            .visible_fabric()
            .map(|fabric| fabric.name.clone())
            .unwrap_or_else(|| "our fabric".into());
        let subtitle = if count == 0 {
            html! { format!("The devices on {fabric}, live") }
        } else {
            html! {
                <>
                    { format!("{count} device{} on {fabric}", if count == 1 { "" } else { "s" }) }
                    <span class="dot-sep"></span>
                    <span class="live-count">{ format!("{live} live") }</span>
                </>
            }
        };

        let actions = html! {
            <>
                if count > 0 {
                    <div class="control has-icons-left search-box">
                        <input class="input" type="search" placeholder="Filter by tag"
                            value={self.tag_filter.clone()}
                            oninput={link.callback(|e: InputEvent| Msg::TagFilter(input_value(e)))} />
                        <span class="icon is-left">{ icon(Icon::Search, 18) }</span>
                    </div>
                }
                <button
                    class={classes!("button", "is-icon",
                        (self.loading == Some(Loading::List)).then_some("is-loading"))}
                    title="Read the list again"
                    disabled={self.busy_stored}
                    onclick={link.callback(|_| Msg::ListDevices)}>
                    { icon(Icon::Refresh, 18) }
                </button>
                { ui::icon_button(Icon::Sync, "Sync", classes!("is-tinted"), self.busy_stored,
                    Some("Add the devices on our fabric missing from the list, and refresh the others"),
                    link.callback(|_| Msg::Sync)) }
            </>
        };

        let body = if self.stored.is_empty() {
            if self.loading == Some(Loading::List) {
                html! {
                    <div class="empty-state">
                        <span class="loader is-large"></span>
                        <p class="empty-state-text">{ "Reading the devices…" }</p>
                    </div>
                }
            } else {
                ui::empty_state(
                    Icon::Devices,
                    "No devices yet",
                    "Commission a device, and it shows up here with its live health.",
                    ui::icon_button(
                        Icon::Commission,
                        "Commission a device",
                        classes!("is-link"),
                        false,
                        None,
                        link.callback(|_| Msg::SelectTab(Tab::Commission)),
                    ),
                )
            }
        } else if visible.is_empty() {
            ui::empty_state(
                Icon::Search,
                "No matches",
                &format!("No device is tagged like “{}”.", self.tag_filter.trim()),
                html! {
                    <button class="button is-soft"
                        onclick={link.callback(|_| Msg::TagFilter(String::new()))}>
                        { "Clear the filter" }
                    </button>
                },
            )
        } else {
            html! {
                <div class="device-grid">
                    { for visible.into_iter().map(|d| self.view_device_card(d, link)) }
                </div>
            }
        };

        html! {
            <>
                { ui::page_header("Devices", subtitle, actions) }
                { body }
            </>
        }
    }

    fn view_device_card(&self, d: &api::DeviceInfo, link: &Scope<Self>) -> Html {
        let node_id = d.handle.node_id;
        let busy = self.busy_stored;
        let live = self.subs.get(&node_id);

        // Offline per its subscription; until that is live or offline, per the
        // last sync. Only Refresh and the actions not needing the device remain.
        let offline = match live.map(|sub| sub.conn) {
            Some(SubConn::Live { .. }) => false,
            Some(SubConn::Offline) => true,
            _ => d.online == Some(false),
        };

        let (state, class, why) = match live.map(|sub| sub.conn) {
            Some(SubConn::Live { max_int_secs }) => (
                "Live",
                Some("is-up"),
                format!(
                    "Subscribed: it reports at least every {max_int_secs} s ({} reports so far)",
                    live.map_or(0, |sub| sub.updates)
                ),
            ),
            Some(SubConn::Reconnecting) => (
                "Reconnecting",
                Some("is-warning"),
                "Reconnecting to the server".to_string(),
            ),
            _ if offline => (
                "Offline",
                Some("is-danger"),
                "Not answering; retrying. Refresh checks it now".to_string(),
            ),
            _ => ("Connecting", None, "Subscribing to its health".to_string()),
        };

        let hardware = d.hardware_version().unwrap_or_else(|| "—".into());
        let software = d.software_version().unwrap_or_else(|| "—".into());
        let refreshing = self.loading == Some(Loading::Device(node_id));

        html! {
            <article class={classes!("device-card", offline.then_some("is-offline"))}>
                <header class="device-head">
                    <span class={classes!("tile-icon", kind_class(d.network))}
                        title={d.network.map(|kind| kind.label())}>
                        { icon(kind_icon(d.network).unwrap_or(Icon::Chip), 22) }
                    </span>
                    <div class="device-title">
                        <h2>{ d.title() }</h2>
                        <p>
                            { d.vendor().unwrap_or_else(|| "Unknown vendor".into()) }
                            if d.is_test_vendor() {
                                { ui::pill("Test vendor", Some("is-warning"), None) }
                            }
                        </p>
                    </div>
                    <div class="device-head-end">
                        { ui::pill(state, class, Some(why)) }
                        <button class={classes!("icon-action", refreshing.then_some("is-loading"))}
                            title="Read its health now" disabled={busy}
                            onclick={link.callback(move |_| Msg::Refresh(node_id))}>
                            { icon(Icon::Refresh, 18) }
                        </button>
                    </div>
                </header>

                <dl class="device-facts">
                    <div>
                        <dt>{ "Product" }</dt>
                        <dd>{ d.product().unwrap_or_else(|| "—".into()) }</dd>
                    </div>
                    <div>
                        <dt>{ "Node" }</dt>
                        <dd class="mono" title={format!("Fabric {}", d.handle.fabric_index)}>
                            { display::hex_u64(node_id) }
                        </dd>
                    </div>
                    <div>
                        <dt>{ "Versions" }</dt>
                        <dd class="device-versions">
                            <span>{ format!("HW {hardware}") }</span>
                            <span>{ format!("SW {software}") }</span>
                        </dd>
                    </div>
                </dl>

                { self.view_fabrics_row(node_id, live, link) }
                { self.view_health(live, offline) }
                { self.view_tags(d, link) }
                { self.view_device_update(d, offline, link) }

                <footer class="device-actions">
                    <div class="device-actions-main">
                        { ui::icon_button(Icon::Identify, "Identify", classes!("is-small", "is-soft"),
                            busy || offline, Some("Make it blink for 3 s"),
                            link.callback(move |_| Msg::Identify(node_id))) }
                        { ui::icon_button(kind_icon(d.network).unwrap_or(Icon::Globe), "Network",
                            classes!("is-small", "is-soft"), offline, Some("Its networks and interfaces"),
                            link.callback(move |_| Msg::OpenNetwork(node_id))) }
                        { ui::icon_button(Icon::Logs, "Logs", classes!("is-small", "is-soft"), offline,
                            Some("Retrieve its diagnostic logs"),
                            link.callback(move |_| Msg::OpenLogs(node_id))) }
                        { ui::icon_button(Icon::Chip, "Memory", classes!("is-small", "is-soft"), offline,
                            Some("Its heap, and the stacks of its tasks"),
                            link.callback(move |_| Msg::Memory(crate::memory::MemoryMsg::Open(node_id)))) }
                        { ui::icon_button(Icon::Clock, "Clock", classes!("is-small", "is-soft"), offline,
                            Some("Its clock; set it from ours"),
                            link.callback(move |_| Msg::Clock(crate::clock::ClockMsg::Open(node_id)))) }
                        { ui::icon_button(Icon::Share, "Share", classes!("is-small", "is-soft"), offline,
                            Some("Let another app (Google Home, Home Assistant, …) commission it too"),
                            link.callback(move |_| Msg::Share(crate::share::ShareMsg::Open(node_id)))) }
                    </div>
                    <div class="device-actions-end">
                        if !d.remembered {
                            <button class="icon-action" disabled={busy}
                                title="Remember it: keep a record, so it can be tagged and stays listed while offline"
                                onclick={link.callback(move |_| Msg::Remember(node_id))}>
                                { icon(Icon::Remember, 18) }
                            </button>
                        }
                        <button class="icon-action is-danger" disabled={busy}
                            title="Decommission it: remove it from our fabric"
                            onclick={link.callback(move |_| Msg::Ask(Ask::Decommission(node_id)))}>
                            { icon(Icon::Trash, 18) }
                        </button>
                    </div>
                </footer>
            </article>
        }
    }

    /// A device's live health: what it reports, as tiles.
    fn view_health(&self, sub: Option<&LiveSub>, offline: bool) -> Html {
        let default = LiveSub::default();
        let health = &sub.unwrap_or(&default).health;
        let g = health.general.unwrap_or_default();

        let tiles = [
            (Icon::Clock, "Uptime", g.up_time_secs.map(display::duration)),
            (
                Icon::Reboot,
                "Reboots",
                g.reboot_count.map(|count| count.to_string()),
            ),
            (
                Icon::Timer,
                "Operating",
                g.total_operational_hours.map(|hours| format!("{hours} h")),
            ),
        ]
        .into_iter()
        .filter_map(|(glyph, label, value)| value.map(|value| (glyph, label, value)))
        .collect::<Vec<_>>();

        if tiles.is_empty() {
            let text = if offline {
                "No health while it is offline."
            } else {
                "Waiting for its first report…"
            };
            return html! { <p class="device-health-empty">{ text }</p> };
        }

        html! {
            <div class="health-tiles">
                { for tiles.into_iter().map(|(glyph, label, value)| html! {
                    <div class="health-tile">
                        <span class="health-tile-icon">{ icon(glyph, 16) }</span>
                        <div>
                            <p class="health-label">{ label }</p>
                            <p class="health-value">{ value }</p>
                        </div>
                    </div>
                }) }
            </div>
        }
    }

    /// A device's tags, and their editor for a remembered one.
    /// A card's fabrics: the controllers it belongs to, opening their popup.
    fn view_fabrics_row(&self, node_id: u64, sub: Option<&LiveSub>, link: &Scope<Self>) -> Html {
        let Some(health) = sub.map(|sub| &sub.health) else {
            return html! {};
        };
        let Some(list) = health.fabrics.as_ref() else {
            return html! {};
        };

        html! {
            <button class="device-fabrics" title="The controllers it belongs to"
                onclick={link.callback(move |_| Msg::Fabrics(crate::fabrics::FabricsMsg::Open(node_id)))}>
                <span class="device-fabrics-label">{ icon(Icon::Fabrics, 14) }{ "Fabrics" }</span>
                <span class="device-fabrics-chips">
                    { for list.iter().map(|fabric| html! {
                        <span class={classes!("fabric-chip", fabric.ours.then_some("is-ours"))}>
                            { fabric.name() }
                        </span>
                    }) }
                </span>
                if let Some(supported) = health.supported_fabrics {
                    <span class="device-fabrics-count">{ format!("{} of {supported}", list.len()) }</span>
                }
                { icon(Icon::ChevronRight, 16) }
            </button>
        }
    }

    fn view_tags(&self, d: &api::DeviceInfo, link: &Scope<Self>) -> Html {
        let node_id = d.handle.node_id;

        if self.editing_tags == Some(node_id) {
            let text = self
                .tag_inputs
                .get(&node_id)
                .cloned()
                .unwrap_or_else(|| d.tags.join(", "));

            return html! {
                <div class="field has-addons tag-editor">
                    <div class="control is-expanded has-icons-left">
                        <input class="input is-small" placeholder="kitchen, ground floor"
                            value={text}
                            oninput={link.callback(move |e: InputEvent| Msg::TagsInput(node_id, input_value(e)))} />
                        <span class="icon is-small is-left">{ icon(Icon::Tag, 14) }</span>
                    </div>
                    <div class="control">
                        <button class="button is-small is-link" disabled={self.busy_stored}
                            title="Save the tags"
                            onclick={link.callback(move |_| Msg::SaveTags(node_id))}>
                            { icon(Icon::Check, 16) }
                        </button>
                    </div>
                    <div class="control">
                        <button class="button is-small" title="Cancel"
                            onclick={link.callback(|_| Msg::EditTags(None))}>
                            { icon(Icon::Close, 16) }
                        </button>
                    </div>
                </div>
            };
        }

        if !d.remembered {
            return html! {};
        }

        html! {
            <div class="device-tags">
                { for d.tags.iter().map(|tag| html! {
                    <span class="tag-chip">{ icon(Icon::Tag, 12) }{ tag.clone() }</span>
                }) }
                <button class="tag-edit" title="Edit the tags"
                    onclick={link.callback(move |_| Msg::EditTags(Some(node_id)))}>
                    { icon(if d.tags.is_empty() { Icon::Plus } else { Icon::Edit }, 13) }
                    { if d.tags.is_empty() { "Add tags" } else { "Edit" } }
                </button>
            </div>
        }
    }

    /// The images a device can update to: for its vendor and product, and newer
    /// than what it runs.
    pub(crate) fn applicable<'a>(
        &'a self,
        d: &'a api::DeviceInfo,
    ) -> impl Iterator<Item = &'a api::OtaImageInfo> + 'a {
        let fp = &d.fingerprint;

        self.ota_images.iter().filter(move |image| {
            fp.vendor_id
                .is_none_or(|vid| image.vendor_id == 0 || image.vendor_id == vid)
                && fp
                    .product_id
                    .is_none_or(|pid| image.product_id == 0 || image.product_id == pid)
                && fp.software_version.is_none_or(|v| image.version > v)
        })
    }

    /// A device card's update strip, when there is an image for it or an
    /// update going on.
    fn view_device_update(&self, d: &api::DeviceInfo, offline: bool, link: &Scope<Self>) -> Html {
        let node_id = d.handle.node_id;
        let images = self.applicable(d).collect::<Vec<_>>();
        let status = self.ota_status.get(&node_id);
        let progress = self.ota_progress.get(&node_id);

        if images.is_empty() && status.is_none() {
            return html! {};
        }

        let chosen = self
            .ota_choice
            .get(&node_id)
            .cloned()
            .or_else(|| images.first().map(|image| image.name.clone()));
        let title = match (progress, images.first()) {
            (Some(api::OtaProgress::Applied { version }), _) => {
                format!("Updated to {}", display::hex_u32(*version))
            }
            (_, Some(image)) if images.len() == 1 => {
                format!("Update available: {}", image.version_str)
            }
            (_, Some(_)) => format!("{} updates available", images.len()),
            (_, None) => "Update".to_string(),
        };
        let applied = matches!(progress, Some(api::OtaProgress::Applied { .. }));
        let percent = match progress {
            Some(api::OtaProgress::Downloading { percent }) => Some(*percent),
            _ => None,
        };
        let remaining = percent
            .and(self.ota_remaining.get(&node_id))
            .map(|secs| format!(" · about {} left", display::duration(*secs)));

        html! {
            <div class={classes!("device-update", applied.then_some("is-done"))}>
                <span class="device-update-icon">
                    { icon(if applied { Icon::CheckCircle } else { Icon::UpdateArrow }, 20) }
                </span>
                <div class="device-update-text">
                    <p class="device-update-title">{ title }</p>
                    if let Some(status) = status {
                        <p class="device-update-status">{ status.clone() }{ remaining.unwrap_or_default() }</p>
                    }
                    if let Some(percent) = percent {
                        <progress class="progress is-small is-link mt-1" value={percent.to_string()} max="100"></progress>
                    }
                </div>
                if !images.is_empty() {
                    <div class="device-update-actions">
                        if images.len() > 1 {
                            <div class="select is-small">
                                <select onchange={link.callback(move |e: Event| Msg::OtaChoose(node_id, select_value(e)))}>
                                    { for images.iter().map(|image| html! {
                                        <option value={image.name.clone()}
                                            selected={chosen.as_deref() == Some(image.name.as_str())}>
                                            { format!("{} ({})", image.version_str, image.version) }
                                        </option>
                                    }) }
                                </select>
                            </div>
                        }
                        <button class="button is-small is-link" disabled={offline}
                            onclick={link.callback(move |_| Msg::OtaUpdate(node_id))}>
                            { "Update" }
                        </button>
                    </div>
                }
            </div>
        }
    }

    /// Full-screen popup to retrieve, read and download a device's logs.
    pub(crate) fn view_logs_modal(&self, link: &Scope<Self>) -> Html {
        let Some(logs) = &self.logs else {
            return html! {};
        };

        let title = self.device_title(logs.node_id);
        let intents = [
            (api::LogIntent::EndUserSupport, Icon::Info),
            (api::LogIntent::NetworkDiag, Icon::Globe),
            (api::LogIntent::CrashLogs, Icon::Alert),
        ];

        html! {
            <div class="modal is-active">
                <div class="modal-background"></div>
                <div class="modal-card is-fullscreen">
                    { ui::modal_head("Logs", &title, html! {}, link.callback(|_| Msg::CloseLogs)) }
                    <section class="modal-card-body">
                        <div class="app container">
                            if let Some(e) = &self.error {
                                { ui::notification(Icon::Alert, classes!("is-danger", "is-light"), html! { e.clone() },
                                    link.callback(|_| Msg::DismissError)) }
                            }
                            <div class="segmented mb-4">
                                { for intents.into_iter().map(|(intent, glyph)| {
                                    let active = logs.intent == Some(intent);
                                    html! {
                                        <button class={classes!("segment", active.then_some("is-active"))}
                                            disabled={logs.loading}
                                            onclick={link.callback(move |_| Msg::FetchLogs(intent))}>
                                            if active && logs.loading {
                                                <span class="loader"></span>
                                            } else {
                                                { icon(glyph, 16) }
                                            }
                                            <span>{ log_intent_name(intent) }</span>
                                        </button>
                                    }
                                }) }
                            </div>
                            { view_log(logs) }
                        </div>
                    </section>
                </div>
            </div>
        }
    }

    /// Full-screen popup with a device's networking.
    pub(crate) fn view_network_modal(&self, link: &Scope<Self>) -> Html {
        let Some(view) = &self.network_view else {
            return html! {};
        };

        network::view_modal(
            view,
            &self.device_title(view.node_id),
            self.error.as_deref(),
            link,
        )
    }

    /// The title of a listed device, else its node ID.
    pub(crate) fn device_title(&self, node_id: u64) -> String {
        self.stored
            .iter()
            .find(|d| d.handle.node_id == node_id)
            .map(api::DeviceInfo::title)
            .unwrap_or_else(|| format!("node {}", display::hex_u64(node_id)))
    }
}

/// A retrieved log: its text, or its size if it is binary, with a download.
fn view_log(logs: &LogsView) -> Html {
    use base64::Engine as _;

    let (Some(intent), Some(data)) = (logs.intent, &logs.data) else {
        return if logs.loading {
            html! {
                <div class="empty-state">
                    <span class="loader is-large"></span>
                    <p class="empty-state-text">{ "Retrieving the log… A large one takes a while." }</p>
                </div>
            }
        } else {
            ui::empty_state(
                Icon::Logs,
                "Pick a log",
                "The device sends the one picked: its end-user support, network diagnostics or crash log.",
                html! {},
            )
        };
    };

    let log = base64::engine::general_purpose::STANDARD
        .decode(data)
        .unwrap_or_default();
    if log.is_empty() {
        return ui::empty_state(
            Icon::CheckCircle,
            "Nothing logged",
            &format!(
                "The device has no {} log.",
                log_intent_name(intent).to_lowercase()
            ),
            html! {},
        );
    }

    let size = log.len();
    let file_name = format!("node-{:016x}-{}.log", logs.node_id, log_intent_file(intent));
    let (meta, body) = match String::from_utf8(log) {
        Ok(text) => {
            let lines = text.lines().count();
            (
                format!(
                    "{} · {lines} line{}",
                    ui::bytes(size as u64),
                    if lines == 1 { "" } else { "s" }
                ),
                html! {
                    <pre class="log-text">
                        { for text.lines().map(|line| html! { <span class="log-line">{ line.to_string() }</span> }) }
                    </pre>
                },
            )
        }
        Err(_) => (
            format!("{} of binary data", ui::bytes(size as u64)),
            html! {
                <div class="log-binary">
                    { icon(Icon::Chip, 22) }
                    <span>{ "Binary data: download it to inspect." }</span>
                </div>
            },
        ),
    };

    html! {
        <div class="log-viewer">
            <div class="log-toolbar">
                <div class="log-title">
                    { icon(Icon::Logs, 16) }
                    <span>{ format!("{} log", log_intent_name(intent)) }</span>
                    <span class="log-meta">{ meta }</span>
                </div>
                <a class="button is-small is-link is-light" download={file_name}
                    href={format!("data:application/octet-stream;base64,{data}")}>
                    <span class="icon is-small">{ icon(Icon::Download, 16) }</span>
                    <span>{ "Download" }</span>
                </a>
            </div>
            { body }
        </div>
    }
}
