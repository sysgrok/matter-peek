//! A device's data model, in a popup: its endpoints and their clusters, the
//! clusters' attributes (written in place where writable) and commands
//! (invoked with a form of their fields), and the events it reported. It
//! follows the device's subscription, as the backs of the device cards do.

use std::collections::{BTreeMap, HashMap, HashSet};

use yew::html::Scope;
use yew::prelude::*;

use matter_peek_core::api::{
    display, wire, AttributeModel, ClusterModel, CommandModel, DeviceModel, EndpointModel,
    EventModel, EventPriority, FieldType, TypeInfo, ValueKind, ValueType,
};

use crate::icons::{device_type_icon, icon, Icon};
use crate::{field_value, input_value, request_loop, select_value, ui, App, Msg, SubConn};

pub(crate) mod controls;
mod value;

/// The global attributes (attribute lists, revision, …) start here.
const FIRST_GLOBAL_ATTRIBUTE: u32 = 0xFFF8;

/// The global attribute listing the commands a cluster accepts.
const ACCEPTED_COMMAND_LIST: u32 = 0xFFF9;

/// The global attributes of a cluster's features and revision.
const FEATURE_MAP: u32 = 0xFFFC;
const CLUSTER_REVISION: u32 = 0xFFFD;

/// The device type of the root endpoint.
const ROOT_NODE: u32 = 0x0016;

/// The events kept of a device, the latest.
const EVENTS_KEPT: usize = 500;

/// The events shown at most, the latest.
const EVENTS_SHOWN: usize = 200;

/// How long what changed shows so (ms).
const RECENT_MS: f64 = 2500.0;

/// The kinds of clusters, as the tree colours them: their classes and names.
const CATEGORIES: &[(&str, &str)] = &[
    ("is-application", "Application"),
    ("is-utility", "Utility"),
    ("is-diagnostics", "Diagnostics"),
    ("is-vendor", "Vendor-specific"),
];

/// The clusters that keep a node running, rather than doing what it is for.
const UTILITY: &[u32] = &[
    0x0003, 0x0004, 0x0005, 0x001D, 0x001E, 0x001F, 0x0025, 0x0028, 0x0029, 0x002A, 0x002B, 0x002C,
    0x002D, 0x002E, 0x0030, 0x0031, 0x0038, 0x0039, 0x003C, 0x003E, 0x003F, 0x0040, 0x0041, 0x0046,
    0x0062, 0x0801, 0x0802,
];

/// The diagnostics clusters.
const DIAGNOSTICS: core::ops::RangeInclusive<u32> = 0x0032..=0x0037;

/// The data model popup of one device.
pub struct ModelView {
    pub node_id: u64,
    /// A read under way.
    pub loading: bool,
    /// As reported so far; `None` until the first report.
    pub model: Option<DeviceModel>,
    pub error: Option<String>,
    /// The cluster shown: its endpoint and ID.
    pub selected: Option<(u16, u32)>,
    /// The endpoint whose controls are shown instead.
    pub controls: Option<u16>,
    /// The device's events are shown instead.
    pub timeline: bool,
    /// The attribute being edited, and the value typed so far.
    pub editing: Option<(u32, String)>,
    /// The command whose form is open, and its fields typed so far.
    pub command: Option<(u32, BTreeMap<String, String>)>,
    /// A write or an invoke under way.
    pub busy: bool,
    /// What the last write or invoke ended with.
    pub outcome: Option<Result<String, String>>,
    /// The tree shows the clusters whose name or ID has this.
    pub filter: String,
    /// The endpoints folded in the tree.
    pub folded: HashSet<u16>,
    /// The cluster shows as JSON.
    pub json: bool,
    /// When each attribute last changed (ms).
    pub changed: HashMap<(u16, u32, u32), f64>,
    /// When an attribute of each cluster last changed (ms).
    pub changed_clusters: HashMap<(u16, u32), f64>,
    /// When each event came, by its number (ms).
    pub new_events: HashMap<u64, f64>,
}

/// What happens in the data model popup.
pub enum ModelMsg {
    Open(u64),
    Refresh,
    Close,
    Select(u16, u32),
    SelectControls(u16),
    SelectEvents,
    Act(Action),
    Edit(u32),
    Draft(String),
    Save,
    CancelEdit,
    OpenCommand(u32),
    CloseCommand,
    FieldDraft(String, String),
    Invoke,
    Filter(String),
    Fold(u16),
    ShowJson(bool),
    Loaded(wire::Response),
    Failed(String),
}

/// What a control does. What changes comes with the subscription.
pub enum Action {
    Write {
        endpoint: u16,
        cluster: u32,
        attribute: u32,
        value: String,
    },
    Invoke {
        endpoint: u16,
        cluster: u32,
        command: u32,
        fields: String,
    },
}

impl App {
    pub(crate) fn update_model(&mut self, ctx: &Context<Self>, msg: ModelMsg) -> bool {
        let link = ctx.link();

        if let ModelMsg::Open(node_id) = msg {
            let mut view = ModelView {
                node_id,
                loading: false,
                model: None,
                error: None,
                selected: None,
                controls: None,
                timeline: false,
                editing: None,
                command: None,
                busy: false,
                outcome: None,
                filter: String::new(),
                folded: HashSet::new(),
                json: false,
                changed: HashMap::new(),
                changed_clusters: HashMap::new(),
                new_events: HashMap::new(),
            };
            // What its card shows, until the whole model is reported.
            if let Some(model) = self.card_models.get(&node_id) {
                view.show(model.clone());
            }
            self.model_view = Some(view);
            self.sync_model_scopes(ctx);
            return true;
        }

        let Some(view) = self.model_view.as_mut() else {
            return false;
        };

        match msg {
            ModelMsg::Open(_) => unreachable!("opened above"),
            ModelMsg::Refresh => {
                view.loading = true;
                view.error = None;
                let (endpoint, cluster) = match (view.controls, view.selected) {
                    (Some(endpoint), _) => (Some(endpoint), None),
                    (None, Some((endpoint, cluster))) => {
                        (Some(endpoint), Some(cluster.to_string()))
                    }
                    (None, None) => (None, None),
                };
                send(
                    link,
                    wire::Request::ReadModel {
                        node_id: view.node_id,
                        endpoint,
                        cluster,
                    },
                );
            }
            ModelMsg::Close => {
                self.model_view = None;
                self.sync_model_scopes(ctx);
            }
            ModelMsg::Select(endpoint, cluster) => {
                view.selected = Some((endpoint, cluster));
                view.controls = None;
                view.timeline = false;
                view.editing = None;
                view.command = None;
                view.outcome = None;
            }
            ModelMsg::SelectControls(endpoint) => {
                view.controls = Some(endpoint);
                view.timeline = false;
                view.editing = None;
                view.command = None;
                view.outcome = None;
            }
            ModelMsg::Act(action) => {
                view.busy = true;
                view.outcome = None;
                send(link, request(view.node_id, action));
            }
            ModelMsg::Edit(attr) => {
                let draft = view
                    .attribute(attr)
                    .map(|(a, _)| draft_of(a.value.as_deref(), a.value_type.as_ref()))
                    .unwrap_or_default();
                view.editing = Some((attr, draft));
                view.outcome = None;
            }
            ModelMsg::Draft(draft) => {
                if let Some((_, current)) = view.editing.as_mut() {
                    *current = draft;
                }
            }
            ModelMsg::CancelEdit => view.editing = None,
            ModelMsg::Save => {
                let (Some((endpoint, cluster)), Some((attr, draft))) =
                    (view.selected, view.editing.as_ref())
                else {
                    return false;
                };
                let value = view
                    .attribute(*attr)
                    .and_then(|(a, _)| a.value_type.as_ref())
                    .map_or_else(|| Ok(draft.clone()), |ty| json_of(ty, draft));
                match value {
                    Ok(value) => {
                        view.busy = true;
                        send(
                            link,
                            wire::Request::WriteAttribute {
                                node_id: view.node_id,
                                endpoint,
                                cluster: cluster.to_string(),
                                attribute: attr.to_string(),
                                value,
                            },
                        );
                    }
                    Err(e) => view.outcome = Some(Err(e)),
                }
            }
            ModelMsg::OpenCommand(id) => {
                view.command = Some((id, BTreeMap::new()));
                view.outcome = None;
            }
            ModelMsg::CloseCommand => view.command = None,
            ModelMsg::FieldDraft(name, draft) => {
                if let Some((_, drafts)) = view.command.as_mut() {
                    drafts.insert(name, draft);
                }
            }
            ModelMsg::Invoke => {
                let (Some((endpoint, cluster)), Some((id, drafts))) =
                    (view.selected, view.command.as_ref())
                else {
                    return false;
                };
                let fields = view
                    .command_model(*id)
                    .map(|(cmd, _)| fields_json(cmd, drafts))
                    .unwrap_or_else(|| Ok(String::new()));
                match fields {
                    Ok(fields) => {
                        view.busy = true;
                        send(
                            link,
                            wire::Request::InvokeCommand {
                                node_id: view.node_id,
                                endpoint,
                                cluster: cluster.to_string(),
                                command: id.to_string(),
                                fields,
                            },
                        );
                    }
                    Err(e) => view.outcome = Some(Err(e)),
                }
            }
            ModelMsg::SelectEvents => {
                view.timeline = true;
                view.editing = None;
                view.command = None;
                view.outcome = None;
            }
            ModelMsg::Filter(filter) => view.filter = filter,
            ModelMsg::Fold(endpoint) => {
                if !view.folded.remove(&endpoint) {
                    view.folded.insert(endpoint);
                }
            }
            ModelMsg::ShowJson(json) => view.json = json,
            ModelMsg::Loaded(wire::Response::Model { model }) => {
                if view.busy {
                    view.busy = false;
                    view.editing = None;
                    view.outcome = Some(Ok("Written.".into()));
                } else {
                    view.loading = false;
                }
                view.show(model);
            }
            ModelMsg::Loaded(wire::Response::CommandOutcome { outcome }) => {
                view.busy = false;
                view.outcome = Some(Ok(match (outcome.response, outcome.fields) {
                    (Some(response), Some(fields)) => format!("{response}: {fields}"),
                    _ => "Done.".into(),
                }));
            }
            ModelMsg::Loaded(wire::Response::Error { message }) | ModelMsg::Failed(message) => {
                if view.busy {
                    view.busy = false;
                    view.outcome = Some(Err(message));
                } else {
                    view.loading = false;
                    view.error = Some(message);
                }
            }
            ModelMsg::Loaded(_) => {}
        }

        true
    }

    /// Attributes the subscription of `node_id` reported: into its card's
    /// model, and the popup's.
    pub(crate) fn model_event(&mut self, node_id: u64, model: DeviceModel) -> bool {
        let mut shown = false;

        if let Some(view) = self.model_view.as_mut().filter(|v| v.node_id == node_id) {
            view.show(model.clone());
            shown = true;
        }
        if self.flipped.contains(&node_id) {
            match self.card_models.get_mut(&node_id) {
                Some(card) => {
                    absorb(card, model);
                }
                None => {
                    self.card_models.insert(node_id, model);
                }
            }
            shown = true;
        }

        shown
    }

    /// Full-screen popup with a device's data model.
    pub(crate) fn view_model_modal(&self, link: &Scope<Self>) -> Html {
        let Some(view) = &self.model_view else {
            return html! {};
        };
        let title = self.device_title(view.node_id);
        let offline = matches!(
            self.subs.get(&view.node_id).map(|sub| sub.conn),
            Some(SubConn::Offline)
        );
        let now = js_sys::Date::now();

        html! {
            <div class="modal is-active">
                <div class="modal-background"></div>
                <div class="modal-card is-fullscreen">
                    { ui::modal_head("Data model", &title, html! {
                        <>
                        { self.device_state(view.node_id).1 }
                        <button
                            class={classes!("button", "is-small", view.loading.then_some("is-loading"))}
                            title="Read the cluster again"
                            disabled={view.loading}
                            onclick={link.callback(|_| model_msg(ModelMsg::Refresh))}>
                            <span class="icon is-small">{ icon(Icon::Refresh, 16) }</span>
                            <span>{ "Refresh" }</span>
                        </button>
                        </>
                    }, link.callback(|_| model_msg(ModelMsg::Close))) }
                    <section class="modal-card-body">
                        if let Some(error) = &view.error {
                            <p class="tasks-error">{ icon(Icon::Alert, 16) }<span>{ error.clone() }</span></p>
                        }
                        { match &view.model {
                            None if view.error.is_some() => html! {},
                            None => html! {
                                <div class="empty-state">
                                    <span class="loader is-large"></span>
                                    <p class="empty-state-text">{ if offline {
                                        "It is not answering; trying again…"
                                    } else {
                                        "Reading its data model…"
                                    } }</p>
                                </div>
                            },
                            Some(model) => html! {
                                <div class="dm-layout">
                                    { view_tree(view, model, link, now) }
                                    { match view.controls.and_then(|ep| model.endpoints.iter().find(|e| e.id == ep)) {
                                        _ if view.timeline => view_timeline(view, model, now),
                                        Some(endpoint) => view_controls(view, model, endpoint, link),
                                        None => view_cluster(view, model, link, now),
                                    } }
                                </div>
                            },
                        } }
                    </section>
                </div>
            </div>
        }
    }
}

impl ModelView {
    /// Take `model` in; on the first, show its first application cluster, or
    /// the controls of an application endpoint having any.
    fn show(&mut self, model: DeviceModel) {
        self.error = None;
        let first = match self.model.as_mut() {
            Some(shown) => {
                let now = js_sys::Date::now();
                let before = shown.events.last().map(|e| e.number);
                for (endpoint, cluster, attr) in absorb(shown, model) {
                    self.changed.insert((endpoint, cluster, attr), now);
                    self.changed_clusters.insert((endpoint, cluster), now);
                }
                for e in shown
                    .events
                    .iter()
                    .filter(|e| before.is_none_or(|before| e.number > before))
                {
                    self.new_events.insert(e.number, now);
                }
                false
            }
            None => {
                self.model = Some(model);
                true
            }
        };

        let model = self.model.as_ref().expect("just set");
        if self.selected.is_none() {
            self.selected = first_cluster(model);
        }
        if first {
            self.controls = model
                .endpoints
                .iter()
                .find(|e| e.id != 0 && controls::any(e, false))
                .map(|e| e.id);
        }
    }

    fn cluster(&self) -> Option<&ClusterModel> {
        let (endpoint, cluster) = self.selected?;
        self.model
            .as_ref()?
            .endpoints
            .iter()
            .find(|e| e.id == endpoint)?
            .clusters
            .iter()
            .find(|c| c.id == cluster)
    }

    fn attribute(&self, id: u32) -> Option<(&AttributeModel, &DeviceModel)> {
        let attr = self.cluster()?.attributes.iter().find(|a| a.id == id)?;
        Some((attr, self.model.as_ref()?))
    }

    fn command_model(&self, id: u32) -> Option<(&CommandModel, &DeviceModel)> {
        let cmd = self.cluster()?.commands.iter().find(|c| c.id == id)?;
        Some((cmd, self.model.as_ref()?))
    }
}

/// The endpoints, each with its device types and clusters; how many there
/// are, and a search through the clusters.
fn view_tree(view: &ModelView, model: &DeviceModel, link: &Scope<App>, now: f64) -> Html {
    let filter = view.filter.trim().to_lowercase();
    let clusters: usize = model.endpoints.iter().map(|e| e.clusters.len()).sum();
    let attributes: usize = model
        .endpoints
        .iter()
        .flat_map(|e| &e.clusters)
        .map(|c| c.attributes.len())
        .sum();

    html! {
        <nav class="dm-tree">
            <div class="control has-icons-left dm-search">
                <input class="input is-small" type="search" placeholder="Find a cluster by name or ID"
                    value={view.filter.clone()}
                    oninput={link.callback(|e: InputEvent| model_msg(ModelMsg::Filter(input_value(e))))} />
                <span class="icon is-left">{ icon(Icon::Search, 16) }</span>
            </div>
            <p class="dm-stats">
                <span><b>{ model.endpoints.len() }</b>{ " endpoints" }</span>
                <span><b>{ clusters }</b>{ " clusters" }</span>
                <span><b>{ attributes }</b>{ " attributes" }</span>
            </p>
            <button class={classes!("dm-tree-events", view.timeline.then_some("is-selected"))}
                onclick={link.callback(|_| model_msg(ModelMsg::SelectEvents))}>
                { icon(Icon::Bolt, 15) }
                <span class="dm-cluster-name">{ "Events" }</span>
                if let Some(t) = model.events.last().and_then(|e| view.new_events.get(&e.number)).filter(|t| now - **t < RECENT_MS) {
                    <span class="dm-pulse" key={t.to_string()} title="Just reported"></span>
                }
                <span class="dm-count">{ model.events.len() }</span>
            </button>
            { for model.endpoints.iter().map(|e| view_endpoint(view, e, &filter, link, now)) }
            <p class="dm-legend">
                { for CATEGORIES.iter().map(|(class, label)| html! {
                    <span><span class={classes!("dm-dot", *class)}></span>{ *label }</span>
                }) }
            </p>
        </nav>
    }
}

/// An endpoint in the tree: its device types, then its clusters, those
/// `filter` finds (lowercase), unless folded.
fn view_endpoint(
    view: &ModelView,
    e: &EndpointModel,
    filter: &str,
    link: &Scope<App>,
    now: f64,
) -> Html {
    let shown: Vec<&ClusterModel> = e
        .clusters
        .iter()
        .filter(|c| filter.is_empty() || finds(c, filter))
        .collect();
    if !filter.is_empty() && shown.is_empty() {
        return html! {};
    }
    let folded = filter.is_empty() && view.folded.contains(&e.id);

    // What it is for first; on the root, the root node.
    let mut types = e.device_types.clone();
    types.sort_by_key(|t| (display::is_utility_device_type(t.id), t.id != ROOT_NODE));
    let mut names = types
        .iter()
        .map(|t| display::device_type_name(t.id).unwrap_or_else(|| display::hex_u32(t.id)));
    let name = names.next().unwrap_or_else(|| format!("Endpoint {}", e.id));
    let mut meta: Vec<String> = names.collect();
    meta.push(format!(
        "{} cluster{}",
        e.clusters.len(),
        if e.clusters.len() == 1 { "" } else { "s" }
    ));
    let glyph = types
        .first()
        .filter(|t| !display::is_utility_device_type(t.id))
        .and_then(|t| device_type_icon(Some(t.id)))
        .unwrap_or(Icon::Chip);
    let id = e.id;

    html! {
        <section class={classes!("dm-endpoint", folded.then_some("is-folded"))}>
            <button class="dm-endpoint-head" title={if folded { "Unfold" } else { "Fold" }}
                onclick={link.callback(move |_| model_msg(ModelMsg::Fold(id)))}>
                <span class="dm-ep-badge" title={format!("Endpoint {id}")}>{ id }</span>
                <span class="dm-endpoint-text">
                    <span class="dm-endpoint-name">{ icon(glyph, 14) }<span>{ name }</span></span>
                    <span class="dm-endpoint-meta">{ meta.join(" · ") }</span>
                </span>
                <span class="dm-chevron">{ icon(Icon::ChevronDown, 16) }</span>
            </button>
            if !folded {
                <ul class="dm-clusters">
                    if filter.is_empty() && controls::any(e, false) {
                        <li>
                            <button class={classes!("dm-cluster", "is-controls", (view.controls == Some(id)).then_some("is-selected"))}
                                onclick={link.callback(move |_| model_msg(ModelMsg::SelectControls(id)))}>
                                { icon(Icon::Sliders, 14) }
                                <span class="dm-cluster-name">{ "Controls" }</span>
                            </button>
                        </li>
                    }
                    { for shown.into_iter().map(|c| {
                        let cluster = c.id;
                        let selected = view.controls.is_none() && view.selected == Some((id, cluster));
                        let (class, kind) = category(cluster);
                        let pulse = view.changed_clusters.get(&(id, cluster)).filter(|t| now - **t < RECENT_MS);
                        html! {
                            <li>
                                <button class={classes!("dm-cluster", selected.then_some("is-selected"))}
                                    title={kind}
                                    onclick={link.callback(move |_| model_msg(ModelMsg::Select(id, cluster)))}>
                                    <span class={classes!("dm-dot", class)}></span>
                                    <span class="dm-cluster-name">{ c.name.clone().unwrap_or_else(|| display::hex_u32(cluster)) }</span>
                                    if let Some(t) = pulse {
                                        <span class="dm-pulse" key={t.to_string()} title="Just changed"></span>
                                    }
                                    <span class="dm-id">{ cluster_id(cluster) }</span>
                                </button>
                            </li>
                        }
                    }) }
                </ul>
            }
        </section>
    }
}

/// An endpoint's controls.
fn view_controls(
    view: &ModelView,
    model: &DeviceModel,
    endpoint: &EndpointModel,
    link: &Scope<App>,
) -> Html {
    let types: Vec<String> = endpoint
        .device_types
        .iter()
        .map(|t| display::device_type_name(t.id).unwrap_or_else(|| display::hex_u32(t.id)))
        .collect();

    html! {
        <div class="dm-cluster-view">
            <header class="dm-hero is-controls">
                <div class="dm-hero-text">
                    <p class="dm-kicker">{ format!("Endpoint {} · Controls", endpoint.id) }</p>
                    <h3 class="dm-hero-title">{ types.join(", ") }</h3>
                    <p class="dm-hero-note">{ "What people act on, from its clusters; what changes comes live." }</p>
                </div>
            </header>
            if let Some(outcome) = view.outcome.as_ref().filter(|o| o.is_err()) {
                <p class="dm-outcome is-error">{ match outcome { Ok(text) | Err(text) => text.clone() } }</p>
            }
            <div class="dm-card dm-controls-card">
                { controls::view(model, endpoint, false, view.busy,
                    &link.callback(|action| model_msg(ModelMsg::Act(action)))) }
            </div>
        </div>
    }
}

/// The cluster shown: what it is, its attributes, then its commands.
fn view_cluster(view: &ModelView, model: &DeviceModel, link: &Scope<App>, now: f64) -> Html {
    let (Some(cluster), Some((endpoint, _))) = (view.cluster(), view.selected) else {
        return html! { <div class="dm-cluster-view"></div> };
    };
    let (own, global): (Vec<_>, Vec<_>) = cluster
        .attributes
        .iter()
        .partition(|a| a.id < FIRST_GLOBAL_ATTRIBUTE);
    let (class, kind) = category(cluster.id);
    let value_of = |id: u32| {
        cluster
            .attributes
            .iter()
            .find(|a| a.id == id)
            .and_then(|a| serde_json::from_str::<serde_json::Value>(a.value.as_deref()?).ok())
    };
    let revision = value_of(CLUSTER_REVISION).and_then(|v| v.as_u64());
    let features: Vec<String> = match value_of(FEATURE_MAP) {
        Some(serde_json::Value::Array(names)) => names
            .iter()
            .map(|n| n.as_str().map_or_else(|| n.to_string(), str::to_string))
            .collect(),
        Some(serde_json::Value::Number(n)) if n.as_u64() != Some(0) => {
            vec![format!("features 0x{:X}", n.as_u64().unwrap_or_default())]
        }
        _ => Vec::new(),
    };
    let ep_types: Vec<String> = model
        .endpoints
        .iter()
        .find(|e| e.id == endpoint)
        .map(|e| {
            e.device_types
                .iter()
                .filter_map(|t| display::device_type_name(t.id))
                .collect()
        })
        .unwrap_or_default();
    let name = cluster
        .name
        .clone()
        .unwrap_or_else(|| display::hex_u32(cluster.id));
    let writable = own.iter().filter(|a| a.writable).count();
    let json = cluster_json(cluster);
    let events: Vec<&EventModel> = model
        .events
        .iter()
        .rev()
        .filter(|e| e.endpoint == endpoint && e.cluster == cluster.id)
        .take(EVENTS_SHOWN)
        .collect();

    html! {
        <div class="dm-cluster-view">
            <header class={classes!("dm-hero", class)}>
                <div class="dm-hero-text">
                    <p class="dm-kicker">
                        { format!("Endpoint {endpoint}") }
                        if !ep_types.is_empty() {
                            { format!(" · {}", ep_types.join(", ")) }
                        }
                    </p>
                    <h3 class="dm-hero-title">{ name }</h3>
                    <div class="dm-chips">
                        <button class="dm-chip is-mono" title="Copy its ID" onclick={copy(cluster_id(cluster.id))}>
                            { cluster_id(cluster.id) }
                        </button>
                        if let Some(revision) = revision {
                            <span class="dm-chip">{ format!("Revision {revision}") }</span>
                        }
                        <span class={classes!("dm-chip", "is-category", class)}>{ kind }</span>
                        { for features.iter().map(|f| html! {
                            <span class="dm-chip is-feature" title="A feature it has">{ f.clone() }</span>
                        }) }
                    </div>
                </div>
                <div class="dm-hero-stats">
                    { stat(own.len(), "attributes") }
                    { stat(writable, "writable") }
                    { stat(cluster.commands.len(), "commands") }
                </div>
            </header>
            <div class="dm-toolbar">
                { ui::two_way((Icon::Table, "Table", "Its attributes as a table"),
                    (Icon::Braces, "JSON", "Its attributes as JSON"),
                    Some(view.json), false, link.callback(|json| model_msg(ModelMsg::ShowJson(json)))) }
                <button class="button is-small is-soft" title="Copy its attributes as JSON"
                    onclick={copy(value::json_text(&json))}>
                    <span class="icon is-small">{ icon(Icon::Copy, 15) }</span>
                    <span>{ "Copy JSON" }</span>
                </button>
            </div>
            if let Some(outcome) = &view.outcome {
                <p class={classes!("dm-outcome", outcome.is_err().then_some("is-error"))}>
                    { match outcome { Ok(text) | Err(text) => text.clone() } }
                </p>
            }
            if view.json {
                <div class="dm-card">{ value::json(&json) }</div>
            } else {
                <div class="dm-card">
                    <table class="dm-attrs">
                        <thead>
                            <tr>
                                <th>{ "ID" }</th>
                                <th>{ "Attribute" }</th>
                                <th>{ "Type" }</th>
                                <th>{ "Value" }</th>
                                <th></th>
                            </tr>
                        </thead>
                        <tbody>
                            { for own.iter().map(|a| view_attribute(view, model, endpoint, cluster, a, link, now)) }
                        </tbody>
                    </table>
                </div>
                if !global.is_empty() {
                    <details class="dm-globals">
                        <summary>
                            { icon(Icon::ChevronRight, 14) }
                            { "Global attributes" }
                            <span class="dm-count">{ global.len() }</span>
                        </summary>
                        <div class="dm-card">
                            <table class="dm-attrs">
                                <tbody>
                                    { for global.iter().map(|a| view_attribute(view, model, endpoint, cluster, a, link, now)) }
                                </tbody>
                            </table>
                        </div>
                    </details>
                }
            }
            if !cluster.commands.is_empty() {
                <h4 class="dm-subtitle">
                    { "Commands" }
                    <span class="dm-count">{ cluster.commands.len() }</span>
                </h4>
                <div class="dm-commands">
                    { for cluster.commands.iter().map(|c| view_command(view, model, endpoint, cluster.id, c, link)) }
                </div>
            }
            if !cluster.events.is_empty() || !events.is_empty() {
                <h4 class="dm-subtitle">
                    { "Events" }
                    <span class="dm-count">{ events.len() }</span>
                </h4>
                if !cluster.events.is_empty() {
                    <p class="dm-event-types">
                        { for cluster.events.iter().map(|t| html! {
                            <span class={classes!("dm-chip", priority_class(t.priority))}
                                title={format!("Event 0x{:02X}, {} priority", t.id, priority_name(t.priority))}>
                                <span class="dm-priority-dot"></span>{ t.name.clone() }
                            </span>
                        }) }
                    </p>
                }
                if events.is_empty() {
                    <p class="dm-events-none">{ "None reported since we follow it." }</p>
                } else {
                    <div class="dm-card dm-events">
                        { for events.iter().map(|e| view_event(view, model, e, false, now)) }
                    </div>
                }
            }
        </div>
    }
}

/// The device's events, the latest first.
fn view_timeline(view: &ModelView, model: &DeviceModel, now: f64) -> Html {
    let count = |priority| {
        model
            .events
            .iter()
            .filter(|e| e.priority == priority)
            .count()
    };

    html! {
        <div class="dm-cluster-view">
            <header class="dm-hero is-events">
                <div class="dm-hero-text">
                    <p class="dm-kicker">{ "The whole device" }</p>
                    <h3 class="dm-hero-title">{ "Events" }</h3>
                    <p class="dm-hero-note">
                        { "What it kept when we started following it, then each event as it comes." }
                    </p>
                </div>
                <div class="dm-hero-stats">
                    { stat(count(EventPriority::Critical), "critical") }
                    { stat(count(EventPriority::Info), "info") }
                    { stat(count(EventPriority::Debug), "debug") }
                </div>
            </header>
            if model.events.is_empty() {
                <p class="dm-events-none">{ "None reported." }</p>
            } else {
                <div class="dm-card dm-events">
                    { for model.events.iter().rev().take(EVENTS_SHOWN).map(|e| view_event(view, model, e, true, now)) }
                </div>
            }
        </div>
    }
}

/// An event: when, how important, what (`whole`: also where), and its fields.
fn view_event(
    view: &ModelView,
    model: &DeviceModel,
    e: &EventModel,
    whole: bool,
    now: f64,
) -> Html {
    let fresh = view
        .new_events
        .get(&e.number)
        .is_some_and(|t| now - t < RECENT_MS);
    let fields = event_fields(model, e);
    let cluster = e
        .cluster_name
        .clone()
        .unwrap_or_else(|| display::hex_u32(e.cluster));

    html! {
        <div class={classes!("dm-event", priority_class(e.priority), fresh.then_some("is-changed"))} key={e.number.to_string()}>
            <div class="dm-event-when">
                <span class="dm-event-time" title={e.utc_ms.map(display::utc_label)}>{ event_time(e) }</span>
                <span class="dm-event-number">{ format!("#{}", e.number) }</span>
            </div>
            <span class="dm-priority" title={format!("{} priority", priority_name(e.priority))}>
                { priority_name(e.priority) }
            </span>
            <div class="dm-event-what">
                <p class="dm-event-title">
                    if whole {
                        <span class="dm-event-where">{ format!("EP {} · {cluster}", e.endpoint) }</span>
                    }
                    <span class="dm-event-name">{ e.name.clone().unwrap_or_else(|| display::hex_u32(e.event)) }</span>
                </p>
                { match (&e.fields, &e.error) {
                    (Some(json), _) => match serde_json::from_str::<serde_json::Value>(json) {
                        Ok(serde_json::Value::Object(o)) if o.is_empty() => html! {},
                        Ok(v) => html! { <div class="dm-event-fields">{ value::fields(&v, fields, model) }</div> },
                        Err(_) => html! { <span class="v-text">{ json.clone() }</span> },
                    },
                    (None, Some(error)) => html! { <span class="v-error">{ icon(Icon::Alert, 14) }{ error.clone() }</span> },
                    (None, None) => html! {},
                } }
            </div>
        </div>
    }
}

/// A count in a cluster's header.
fn stat(n: usize, what: &str) -> Html {
    html! {
        <div class="dm-stat">
            <span class="dm-stat-n">{ n }</span>
            <span class="dm-stat-what">{ what.to_string() }</span>
        </div>
    }
}

fn view_attribute(
    view: &ModelView,
    model: &DeviceModel,
    endpoint: u16,
    cluster: &ClusterModel,
    a: &AttributeModel,
    link: &Scope<App>,
    now: f64,
) -> Html {
    let id = a.id;
    let editing = view
        .editing
        .as_ref()
        .filter(|(attr, _)| *attr == id)
        .map(|(_, draft)| draft);
    let path = format!(
        "{endpoint}/{}/{}",
        cluster_id(cluster.id),
        display::hex_u16(a.id as u16)
    );
    let changed = view.changed.get(&(endpoint, cluster.id, a.id)).copied();
    // A fresh element for each change, so that it flashes once.
    let key = match (editing, changed) {
        (Some(_), _) => "editing".to_string(),
        (None, Some(t)) => t.to_string(),
        (None, None) => String::new(),
    };
    let flash = editing.is_none() && changed.is_some_and(|t| now - t < RECENT_MS);
    let nullable = a.value_type.as_ref().is_some_and(|t| t.nullable);

    let value = match (&a.value, &a.error) {
        (Some(v), _) => value::attribute(model, cluster, a.id, a.value_type.as_ref(), v),
        (None, Some(e)) => {
            html! { <span class="v-error">{ icon(Icon::Alert, 14) }{ e.clone() }</span> }
        }
        (None, None) => html! { <span class="v-none">{ "—" }</span> },
    };

    html! {
        <tr class={classes!("dm-attr", editing.is_some().then_some("is-editing"))}>
            <td class="dm-attr-id">
                <button class="dm-id-button" title={format!("Copy its path, {path}")} onclick={copy(path.clone())}>
                    { display::hex_u16(a.id as u16) }
                </button>
            </td>
            <td class="dm-attr-name">
                <span>{ a.name.clone().unwrap_or_else(|| display::hex_u32(a.id)) }</span>
                <span class="dm-flags">
                    if a.writable {
                        <span class="dm-flag is-writable" title="Writable">{ "W" }</span>
                    }
                    if a.timed {
                        <span class="dm-flag is-timed" title="Written with a timed interaction">{ "T" }</span>
                    }
                    if nullable {
                        <span class="dm-flag" title="Nullable">{ "N" }</span>
                    }
                </span>
            </td>
            <td class="dm-attr-type">{ type_badge(a.value_type.as_ref(), model) }</td>
            <td class="dm-attr-value">
                <div class={classes!("dm-value", flash.then_some("is-changed"))} {key}>
                    { match (editing, a.value_type.as_ref()) {
                        (Some(draft), Some(ty)) => html! {
                            <div class="dm-edit">
                                { editor(ty, model, draft, link.callback(|v| model_msg(ModelMsg::Draft(v)))) }
                                <button class={classes!("button", "is-small", "is-link", view.busy.then_some("is-loading"))}
                                    disabled={view.busy}
                                    onclick={link.callback(|_| model_msg(ModelMsg::Save))}>{ "Write" }</button>
                                <button class="button is-small" disabled={view.busy}
                                    onclick={link.callback(|_| model_msg(ModelMsg::CancelEdit))}>{ "Cancel" }</button>
                            </div>
                        },
                        _ => value,
                    } }
                </div>
            </td>
            <td class="dm-attr-actions">
                if let Some(json) = &a.value {
                    <button class="icon-action" title="Copy its value (JSON)" onclick={copy(json.clone())}>
                        { icon(Icon::Copy, 15) }
                    </button>
                }
                if a.writable && editing.is_none() {
                    <button class="icon-action" title="Write it"
                        onclick={link.callback(move |_| model_msg(ModelMsg::Edit(id)))}>
                        { icon(Icon::Edit, 15) }
                    </button>
                }
            </td>
        </tr>
    }
}

/// A value's type, shortly, coloured by its kind.
fn type_badge(ty: Option<&ValueType>, model: &DeviceModel) -> Html {
    let Some(ty) = ty else {
        return html! { <span class="dm-type is-unknown">{ "?" }</span> };
    };
    let base = match ty.schema.and_then(|i| model.types.get(i as usize)) {
        Some(info) => info.name.clone(),
        None => ty.idl.clone(),
    };
    let class = match ty.kind {
        ValueKind::Bool => "is-bool",
        ValueKind::UInt | ValueKind::Int | ValueKind::Float => "is-number",
        ValueKind::Text => "is-text",
        ValueKind::Octets => "is-octets",
        ValueKind::Enum => "is-enum",
        ValueKind::Bitmap => "is-bitmap",
        ValueKind::Struct => "is-struct",
        ValueKind::Unknown => "is-unknown",
    };

    html! {
        <span class={classes!("dm-type", class)} title={type_label(ty, model)}>
            { if ty.list { format!("{base}[]") } else { base } }
        </span>
    }
}

fn view_command(
    view: &ModelView,
    model: &DeviceModel,
    endpoint: u16,
    cluster: u32,
    c: &CommandModel,
    link: &Scope<App>,
) -> Html {
    let id = c.id;
    let open = view
        .command
        .as_ref()
        .filter(|(cmd, _)| *cmd == id)
        .map(|(_, drafts)| drafts);
    let name = c.name.clone().unwrap_or_else(|| display::hex_u32(c.id));

    html! {
        <div class={classes!("dm-command", open.is_some().then_some("is-open"))}>
            <div class="dm-command-head">
                <span class="dm-command-id">{ format!("0x{:02X}", c.id) }</span>
                <span class="dm-command-name">{ name.clone() }</span>
                if c.timed {
                    <span class="dm-flag is-timed" title="Invoked with a timed interaction">{ "T" }</span>
                }
                if open.is_none() {
                    if c.fields.is_empty() {
                        <button class="button is-small is-link is-light" disabled={view.busy}
                            onclick={link.callback(move |_| model_msg(ModelMsg::Act(Action::Invoke {
                                endpoint, cluster, command: id, fields: "{}".into(),
                            })))}>
                            <span class="icon is-small">{ icon(Icon::Play, 14) }</span>
                            <span>{ "Invoke" }</span>
                        </button>
                    } else {
                        <button class="button is-small is-soft"
                            onclick={link.callback(move |_| model_msg(ModelMsg::OpenCommand(id)))}>
                            { "Fill in…" }
                        </button>
                    }
                }
            </div>
            <p class="dm-signature">
                <span class="sig-name">{ name }</span>{ "(" }
                { for c.fields.iter().enumerate().map(|(i, f)| html! {
                    <>
                        if i > 0 { { ", " } }
                        <span class="sig-param">{ f.name.clone() }{ if f.value_type.optional { "?" } else { "" } }</span>
                        { ": " }
                        <span class="sig-type">{ short_type(&f.value_type, model) }</span>
                    </>
                }) }
                { ")" }
                if let Some(response) = &c.response {
                    <span class="sig-arrow">{ " → " }</span>
                    <span class="sig-type">{ response.clone() }</span>
                }
            </p>
            if let Some(drafts) = open {
                <div class="dm-form">
                    { for c.fields.iter().map(|f| {
                        let field = f.name.clone();
                        let draft = drafts.get(&f.name).cloned().unwrap_or_default();
                        html! {
                            <label class="dm-field">
                                <span class="dm-field-name">
                                    { f.name.clone() }
                                    <span class="dm-id">{ format!(" {}", type_label(&f.value_type, model)) }</span>
                                </span>
                                { editor(&f.value_type, model, &draft,
                                    link.callback(move |v| model_msg(ModelMsg::FieldDraft(field.clone(), v)))) }
                            </label>
                        }
                    }) }
                    <div class="dm-form-actions">
                        <button class={classes!("button", "is-small", "is-link", view.busy.then_some("is-loading"))}
                            disabled={view.busy}
                            onclick={link.callback(|_| model_msg(ModelMsg::Invoke))}>{ "Invoke" }</button>
                        <button class="button is-small" disabled={view.busy}
                            onclick={link.callback(|_| model_msg(ModelMsg::CloseCommand))}>{ "Close" }</button>
                    </div>
                </div>
            }
        </div>
    }
}

/// An input for a value of `ty`, typed as `draft`.
fn editor(ty: &ValueType, model: &DeviceModel, draft: &str, on: Callback<String>) -> Html {
    let choices = ty
        .schema
        .and_then(|i| model.types.get(i as usize))
        .map(|info| info.values.clone())
        .unwrap_or_default();

    if ty.list || matches!(ty.kind, ValueKind::Struct | ValueKind::Unknown) {
        let on = on.clone();
        return html! {
            <textarea class="textarea is-small mono" rows="3" placeholder="JSON"
                value={draft.to_string()}
                oninput={Callback::from(move |e: InputEvent| on.emit(field_value(&e)))} />
        };
    }

    match ty.kind {
        ValueKind::Bool => select(&["true", "false"], draft, ty.nullable, on),
        ValueKind::Enum => {
            let names: Vec<&str> = choices.iter().map(|v| v.name.as_str()).collect();
            select(&names, draft, ty.nullable, on)
        }
        ValueKind::Bitmap => {
            let set: Vec<String> = draft.split(',').map(|s| s.trim().to_string()).collect();
            html! {
                <div class="dm-bits">
                    { for choices.iter().map(|bit| {
                        let name = bit.name.clone();
                        let checked = set.contains(&name);
                        let others: Vec<String> = set.iter().filter(|s| **s != name && !s.is_empty()).cloned().collect();
                        let on = on.clone();
                        html! {
                            <label class="checkbox">
                                <input type="checkbox" checked={checked}
                                    onchange={Callback::from(move |_| {
                                        let mut next = others.clone();
                                        if !checked {
                                            next.push(name.clone());
                                        }
                                        on.emit(next.join(","));
                                    })} />
                                { format!(" {}", bit.name) }
                            </label>
                        }
                    }) }
                </div>
            }
        }
        _ => {
            let placeholder = match ty.kind {
                ValueKind::Octets => "hex".to_string(),
                ValueKind::Text => String::new(),
                _ => ty.idl.clone(),
            };
            html! {
                <input class="input is-small" type="text" placeholder={placeholder}
                    value={draft.to_string()}
                    oninput={Callback::from(move |e: InputEvent| on.emit(field_value(&e)))} />
            }
        }
    }
}

fn select(options: &[&str], draft: &str, nullable: bool, on: Callback<String>) -> Html {
    html! {
        <div class="select is-small">
            <select onchange={Callback::from(move |e: Event| on.emit(select_value(e)))}>
                <option value="" selected={draft.is_empty()}>{ "—" }</option>
                if nullable {
                    <option value="null" selected={draft == "null"}>{ "null" }</option>
                }
                { for options.iter().map(|o| html! {
                    <option value={o.to_string()} selected={draft == *o}>{ o.to_string() }</option>
                }) }
            </select>
        </div>
    }
}

/// The draft of an editor for `value` (JSON) of `ty`.
fn draft_of(value: Option<&str>, ty: Option<&ValueType>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(value) else {
        return value.to_string();
    };

    match (ty.map(|t| (t.kind, t.list)), &json) {
        (Some((_, true)), _) | (Some((ValueKind::Struct | ValueKind::Unknown, _)), _) => {
            serde_json::to_string_pretty(&json).unwrap_or_default()
        }
        (_, serde_json::Value::String(s)) => s.clone(),
        (Some((ValueKind::Bitmap, _)), serde_json::Value::Array(names)) => names
            .iter()
            .filter_map(|n| n.as_str())
            .collect::<Vec<_>>()
            .join(","),
        (_, other) => other.to_string(),
    }
}

/// The JSON of `draft`, typed into an editor for `ty`.
fn json_of(ty: &ValueType, draft: &str) -> Result<String, String> {
    let draft = draft.trim();
    if draft == "null" && ty.nullable {
        return Ok("null".into());
    }

    if ty.list || matches!(ty.kind, ValueKind::Struct | ValueKind::Unknown) {
        return serde_json::from_str::<serde_json::Value>(draft)
            .map(|v| v.to_string())
            .map_err(|e| format!("not JSON: {e}"));
    }

    match ty.kind {
        ValueKind::Text | ValueKind::Octets | ValueKind::Enum => {
            Ok(serde_json::Value::String(draft.to_string()).to_string())
        }
        ValueKind::Bitmap => {
            let names: Vec<serde_json::Value> = draft
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| serde_json::Value::String(s.to_string()))
                .collect();
            Ok(serde_json::Value::Array(names).to_string())
        }
        ValueKind::Bool | ValueKind::UInt | ValueKind::Int | ValueKind::Float => {
            if draft.is_empty() {
                return Err("no value".into());
            }
            // A number, or a hex or decimal string the controller reads.
            Ok(match serde_json::from_str::<serde_json::Value>(draft) {
                Ok(v @ (serde_json::Value::Number(_) | serde_json::Value::Bool(_))) => {
                    v.to_string()
                }
                _ => serde_json::Value::String(draft.to_string()).to_string(),
            })
        }
        ValueKind::Struct | ValueKind::Unknown => unreachable!("handled above"),
    }
}

/// The fields of a command, as a JSON object; an empty optional field is
/// left out.
fn fields_json(cmd: &CommandModel, drafts: &BTreeMap<String, String>) -> Result<String, String> {
    let mut object = serde_json::Map::new();

    for f in &cmd.fields {
        let draft = drafts.get(&f.name).map(|d| d.trim()).unwrap_or_default();
        if draft.is_empty() {
            if f.value_type.optional {
                continue;
            }
            // A bitmap with no bits set.
            if f.value_type.kind == ValueKind::Bitmap {
                object.insert(f.name.clone(), serde_json::Value::Array(Vec::new()));
                continue;
            }
            return Err(format!("{} is missing", f.name));
        }
        let json = json_of(&f.value_type, draft).map_err(|e| format!("{}: {e}", f.name))?;
        object.insert(
            f.name.clone(),
            serde_json::from_str(&json).map_err(|e| e.to_string())?,
        );
    }

    Ok(serde_json::Value::Object(object).to_string())
}

/// What a field takes, shortly.
fn type_label(ty: &ValueType, model: &DeviceModel) -> String {
    let mut label = match ty.schema.and_then(|i| model.types.get(i as usize)) {
        Some(info) => info.name.clone(),
        None => ty.idl.clone(),
    };
    if ty.list {
        label = format!("list of {label}");
    }
    if ty.optional {
        label.push_str(", optional");
    }
    if ty.nullable {
        label.push_str(", nullable");
    }
    label
}

/// Take what `update` says into `shown`: the values of its attributes, and
/// the endpoints, clusters, attributes and commands `shown` lacks. Returns
/// the attributes whose values changed.
pub(crate) fn absorb(shown: &mut DeviceModel, update: DeviceModel) -> Vec<(u16, u32, u32)> {
    let mut changed = Vec::new();
    let DeviceModel {
        endpoints,
        types,
        events,
        ..
    } = shown;
    let mut remap = Remap {
        from: &update.types,
        to: vec![None; update.types.len()],
        into: types,
    };

    for e in update.endpoints {
        let endpoint = match endpoints.iter().position(|s| s.id == e.id) {
            Some(i) => &mut endpoints[i],
            None => {
                let i = endpoints.partition_point(|s| s.id < e.id);
                endpoints.insert(
                    i,
                    EndpointModel {
                        id: e.id,
                        device_types: Vec::new(),
                        clusters: Vec::new(),
                    },
                );
                &mut endpoints[i]
            }
        };
        // Unless its Descriptor was reported, it has none.
        if !e.device_types.is_empty() {
            endpoint.device_types = e.device_types;
        }

        for c in e.clusters {
            let cluster = match endpoint.clusters.iter().position(|s| s.id == c.id) {
                Some(i) => &mut endpoint.clusters[i],
                None => {
                    let i = endpoint.clusters.partition_point(|s| s.id < c.id);
                    endpoint.clusters.insert(
                        i,
                        ClusterModel {
                            id: c.id,
                            name: c.name.clone(),
                            attributes: Vec::new(),
                            commands: Vec::new(),
                            events: Vec::new(),
                        },
                    );
                    &mut endpoint.clusters[i]
                }
            };
            if !c.events.is_empty() {
                cluster.events = c
                    .events
                    .into_iter()
                    .map(|mut event| {
                        event.fields = remap.fields(event.fields);
                        event
                    })
                    .collect();
            }
            // Its commands are as its accepted command list says.
            if c.attributes.iter().any(|a| a.id == ACCEPTED_COMMAND_LIST) {
                cluster.commands = c
                    .commands
                    .into_iter()
                    .map(|mut cmd| {
                        cmd.fields = remap.fields(cmd.fields);
                        cmd
                    })
                    .collect();
            }
            for mut a in c.attributes {
                a.value_type = a.value_type.map(|ty| remap.value_type(ty));
                match cluster.attributes.iter_mut().find(|s| s.id == a.id) {
                    Some(attr) => {
                        if attr.value != a.value || attr.error != a.error {
                            changed.push((e.id, c.id, a.id));
                        }
                        *attr = a;
                    }
                    None => cluster.attributes.push(a),
                }
            }
        }
    }

    let last = events.last().map(|e| e.number);
    events.extend(
        update
            .events
            .into_iter()
            .filter(|e| last.is_none_or(|last| e.number > last)),
    );
    let excess = events.len().saturating_sub(EVENTS_KEPT);
    events.drain(..excess);

    changed
}

/// The types of an update, as indices into those of the model taking it.
struct Remap<'a> {
    from: &'a [TypeInfo],
    /// Each one's index in `into`, once known.
    to: Vec<Option<u32>>,
    into: &'a mut Vec<TypeInfo>,
}

impl Remap<'_> {
    fn value_type(&mut self, mut ty: ValueType) -> ValueType {
        ty.schema = ty.schema.map(|i| self.index(i));
        ty
    }

    fn fields(&mut self, fields: Vec<FieldType>) -> Vec<FieldType> {
        fields
            .into_iter()
            .map(|mut field| {
                field.value_type = self.value_type(field.value_type);
                field
            })
            .collect()
    }

    /// The index in `into` of type `i`: of the same type there, else of it
    /// added.
    fn index(&mut self, i: u32) -> u32 {
        if let Some(Some(j)) = self.to.get(i as usize) {
            return *j;
        }
        let Some(info) = self.from.get(i as usize) else {
            return i;
        };
        let info = TypeInfo {
            name: info.name.clone(),
            values: info.values.clone(),
            fields: self.fields(info.fields.clone()),
        };
        let j = match self.into.iter().position(|t| *t == info) {
            Some(j) => j,
            None => {
                self.into.push(info);
                self.into.len() - 1
            }
        } as u32;
        self.to[i as usize] = Some(j);
        j
    }
}

/// The request doing `action` on device `node_id`.
pub(crate) fn request(node_id: u64, action: Action) -> wire::Request {
    match action {
        Action::Write {
            endpoint,
            cluster,
            attribute,
            value,
        } => wire::Request::WriteAttribute {
            node_id,
            endpoint,
            cluster: cluster.to_string(),
            attribute: attribute.to_string(),
            value,
        },
        Action::Invoke {
            endpoint,
            cluster,
            command,
            fields,
        } => wire::Request::InvokeCommand {
            node_id,
            endpoint,
            cluster: cluster.to_string(),
            command: command.to_string(),
            fields,
        },
    }
}

/// The types of the fields of event `e`, as its cluster has them.
fn event_fields<'m>(model: &'m DeviceModel, e: &EventModel) -> Option<&'m [FieldType]> {
    let event = model
        .endpoints
        .iter()
        .flat_map(|endpoint| &endpoint.clusters)
        .filter(|c| c.id == e.cluster)
        .flat_map(|c| &c.events)
        .find(|t| t.id == e.event)?;
    Some(event.fields.as_slice())
}

/// When an event happened: in our local time (only the time, today), or
/// since the device started.
pub(crate) fn event_time(e: &EventModel) -> String {
    match (e.utc_ms, e.uptime_ms) {
        (Some(ms), _) => {
            let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms as f64));
            let today = js_sys::Date::new_0().to_date_string() == date.to_date_string();
            let time = String::from(date.to_locale_time_string("en-GB"));
            if today {
                time
            } else {
                format!(
                    "{} {time}",
                    String::from(
                        date.to_locale_date_string("en-GB", &wasm_bindgen::JsValue::UNDEFINED)
                    )
                )
            }
        }
        (None, Some(ms)) => format!("+{}", display::duration(ms / 1000)),
        (None, None) => "—".into(),
    }
}

/// An event in a few words: its name, and its fields' values.
pub(crate) fn event_brief(e: &EventModel) -> String {
    let name = e.name.clone().unwrap_or_else(|| display::hex_u32(e.event));
    let fields: Vec<String> = e
        .fields
        .as_deref()
        .and_then(|f| serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(f).ok())
        .into_iter()
        .flatten()
        .map(|(k, v)| match v {
            serde_json::Value::String(s) => format!("{k} {s}"),
            serde_json::Value::Array(items) if items.is_empty() => format!("{k} none"),
            serde_json::Value::Array(items) => {
                let items: Vec<String> = items
                    .iter()
                    .map(|i| i.as_str().map_or_else(|| i.to_string(), str::to_string))
                    .collect();
                format!("{k} {}", items.join(", "))
            }
            other => format!("{k} {other}"),
        })
        .collect();

    if fields.is_empty() {
        name
    } else {
        format!("{name} · {}", fields.join(", "))
    }
}

fn priority_class(priority: EventPriority) -> &'static str {
    match priority {
        EventPriority::Debug => "is-debug",
        EventPriority::Info => "is-info",
        EventPriority::Critical => "is-critical",
    }
}

fn priority_name(priority: EventPriority) -> &'static str {
    match priority {
        EventPriority::Debug => "debug",
        EventPriority::Info => "info",
        EventPriority::Critical => "critical",
    }
}

/// What a field takes, in a command's signature.
fn short_type(ty: &ValueType, model: &DeviceModel) -> String {
    let base = match ty.schema.and_then(|i| model.types.get(i as usize)) {
        Some(info) => info.name.clone(),
        None => ty.idl.clone(),
    };
    let mut label = if ty.list { format!("{base}[]") } else { base };
    if ty.nullable {
        label.push_str(" | null");
    }
    label
}

/// A cluster's attributes as the fields of one JSON object, by name, in
/// the order of their IDs.
fn cluster_json(cluster: &ClusterModel) -> Vec<(String, serde_json::Value)> {
    let mut attributes: Vec<&AttributeModel> = cluster.attributes.iter().collect();
    attributes.sort_by_key(|a| a.id);

    attributes
        .into_iter()
        .map(|a| {
            let name = a.name.clone().unwrap_or_else(|| display::hex_u32(a.id));
            let value = match (&a.value, &a.error) {
                (Some(v), _) => serde_json::from_str(v).unwrap_or(serde_json::Value::Null),
                (None, Some(e)) => serde_json::json!({ "error": e }),
                (None, None) => serde_json::Value::Null,
            };
            (name, value)
        })
        .collect()
}

/// The kind of cluster `id` is: its class and name.
fn category(id: u32) -> (&'static str, &'static str) {
    let i = if id > 0xFFFF || id >= 0xFC00 {
        3
    } else if DIAGNOSTICS.contains(&id) {
        2
    } else if UTILITY.contains(&id) {
        1
    } else {
        0
    };

    CATEGORIES[i]
}

/// Whether `filter` (lowercase) finds `cluster`: in its name or its ID.
fn finds(cluster: &ClusterModel, filter: &str) -> bool {
    cluster
        .name
        .as_deref()
        .is_some_and(|n| n.to_lowercase().contains(filter))
        || cluster_id(cluster.id).to_lowercase().contains(filter)
}

/// Copy `text` to the clipboard.
fn copy(text: String) -> Callback<MouseEvent> {
    Callback::from(move |_| {
        if let Some(window) = web_sys::window() {
            let _ = window.navigator().clipboard().write_text(&text);
        }
    })
}

/// The first application cluster, else the first one.
fn first_cluster(model: &DeviceModel) -> Option<(u16, u32)> {
    let mut clusters = model
        .endpoints
        .iter()
        .flat_map(|e| e.clusters.iter().map(move |c| (e.id, c.id)));
    let first = clusters.clone().next();

    clusters.find(|(endpoint, _)| *endpoint != 0).or(first)
}

/// A cluster ID: four digits for the standard ones.
fn cluster_id(id: u32) -> String {
    if id <= 0xFFFF {
        display::hex_u16(id as u16)
    } else {
        display::hex_u32(id)
    }
}

fn model_msg(msg: ModelMsg) -> Msg {
    Msg::Model(msg)
}

/// Send `request`; the answer goes to the popup.
fn send(link: &Scope<App>, request: wire::Request) {
    request_loop(
        link.clone(),
        request,
        |response| model_msg(ModelMsg::Loaded(response)),
        |e| model_msg(ModelMsg::Failed(e)),
    );
}
