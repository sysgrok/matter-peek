//! A device's data model, in a popup: its endpoints and their clusters, the
//! clusters' attributes (written in place where writable) and commands
//! (invoked with a form of their fields).

use std::collections::BTreeMap;

use yew::html::Scope;
use yew::prelude::*;

use matter_peek_core::api::{
    display, wire, AttributeModel, ClusterModel, CommandModel, DeviceModel, ValueKind, ValueType,
};

use crate::icons::{icon, Icon};
use crate::{field_value, request_loop, select_value, ui, App, Msg};

mod controls;

/// The global attributes (attribute lists, revision, …) start here.
const FIRST_GLOBAL_ATTRIBUTE: u32 = 0xFFF8;

/// The data model popup of one device.
pub struct ModelView {
    pub node_id: u64,
    pub loading: bool,
    /// As last read; `None` before the first read.
    pub model: Option<DeviceModel>,
    pub error: Option<String>,
    /// The cluster shown: its endpoint and ID.
    pub selected: Option<(u16, u32)>,
    /// The endpoint whose controls are shown instead.
    pub controls: Option<u16>,
    /// The cluster to read again once a command is answered.
    pub refresh_after: Option<(u16, u32)>,
    /// The attribute being edited, and the value typed so far.
    pub editing: Option<(u32, String)>,
    /// The command whose form is open, and its fields typed so far.
    pub command: Option<(u32, BTreeMap<String, String>)>,
    /// A write or an invoke under way.
    pub busy: bool,
    /// What the last write or invoke ended with.
    pub outcome: Option<Result<String, String>>,
}

/// What happens in the data model popup.
pub enum ModelMsg {
    Open(u64),
    Refresh,
    Close,
    Select(u16, u32),
    SelectControls(u16),
    Act(Action),
    Edit(u32),
    Draft(String),
    Save,
    CancelEdit,
    OpenCommand(u32),
    CloseCommand,
    FieldDraft(String, String),
    Invoke,
    Loaded(wire::Response),
    Failed(String),
}

/// What a control does.
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
        /// Read the cluster again once answered.
        refresh: bool,
    },
}

impl App {
    pub(crate) fn update_model(&mut self, ctx: &Context<Self>, msg: ModelMsg) -> bool {
        let link = ctx.link();

        if let ModelMsg::Open(node_id) = msg {
            self.model_view = Some(ModelView {
                node_id,
                loading: true,
                model: None,
                error: None,
                selected: None,
                controls: None,
                refresh_after: None,
                editing: None,
                command: None,
                busy: false,
                outcome: None,
            });
            send(
                link,
                wire::Request::ReadModel {
                    node_id,
                    endpoint: None,
                    cluster: None,
                },
            );
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
            ModelMsg::Close => self.model_view = None,
            ModelMsg::Select(endpoint, cluster) => {
                view.selected = Some((endpoint, cluster));
                view.controls = None;
                view.editing = None;
                view.command = None;
                view.outcome = None;
            }
            ModelMsg::SelectControls(endpoint) => {
                view.controls = Some(endpoint);
                view.editing = None;
                view.command = None;
                view.outcome = None;
            }
            ModelMsg::Act(action) => {
                view.busy = true;
                view.outcome = None;
                let node_id = view.node_id;
                send(
                    link,
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
                            refresh,
                        } => {
                            view.refresh_after = refresh.then_some((endpoint, cluster));
                            wire::Request::InvokeCommand {
                                node_id,
                                endpoint,
                                cluster: cluster.to_string(),
                                command: command.to_string(),
                                fields,
                            }
                        }
                    },
                );
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
            ModelMsg::Loaded(wire::Response::Model { model }) => {
                view.loading = false;
                if view.busy {
                    view.busy = false;
                    view.editing = None;
                    view.outcome = Some(Ok("Written.".into()));
                }
                match view.model.as_mut() {
                    Some(shown) if model.node_id == shown.node_id => merge(shown, &model),
                    _ => {
                        view.selected = view.selected.or_else(|| first_cluster(&model));
                        // An application endpoint's controls, if it has any.
                        view.controls = model
                            .endpoints
                            .iter()
                            .find(|e| e.id != 0 && !controls::of(e).is_empty())
                            .map(|e| e.id);
                        view.model = Some(model);
                    }
                }
            }
            ModelMsg::Loaded(wire::Response::CommandOutcome { outcome }) => {
                view.busy = false;
                if let Some((endpoint, cluster)) = view.refresh_after.take() {
                    view.loading = true;
                    send(
                        link,
                        wire::Request::ReadModel {
                            node_id: view.node_id,
                            endpoint: Some(endpoint),
                            cluster: Some(cluster.to_string()),
                        },
                    );
                }
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

    /// Full-screen popup with a device's data model.
    pub(crate) fn view_model_modal(&self, link: &Scope<Self>) -> Html {
        let Some(view) = &self.model_view else {
            return html! {};
        };
        let title = self.device_title(view.node_id);

        html! {
            <div class="modal is-active">
                <div class="modal-background"></div>
                <div class="modal-card is-fullscreen">
                    { ui::modal_head("Data model", &title, html! {
                        <button
                            class={classes!("button", "is-small", view.loading.then_some("is-loading"))}
                            title="Read the cluster again"
                            disabled={view.loading}
                            onclick={link.callback(|_| model_msg(ModelMsg::Refresh))}>
                            <span class="icon is-small">{ icon(Icon::Refresh, 16) }</span>
                            <span>{ "Refresh" }</span>
                        </button>
                    }, link.callback(|_| model_msg(ModelMsg::Close))) }
                    <section class="modal-card-body">
                        if let Some(error) = &view.error {
                            <p class="tasks-error">{ icon(Icon::Alert, 16) }<span>{ error.clone() }</span></p>
                        }
                        { match &view.model {
                            None if view.loading => html! {
                                <div class="empty-state">
                                    <span class="loader is-large"></span>
                                    <p class="empty-state-text">{ "Reading its data model…" }</p>
                                </div>
                            },
                            None => html! {},
                            Some(model) => html! {
                                <div class="dm-layout">
                                    { view_tree(view, model, link) }
                                    { match view.controls.and_then(|ep| model.endpoints.iter().find(|e| e.id == ep)) {
                                        Some(endpoint) => view_controls(view, model, endpoint, link),
                                        None => view_cluster(view, model, link),
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

/// The endpoints, each with its device types and clusters.
fn view_tree(view: &ModelView, model: &DeviceModel, link: &Scope<App>) -> Html {
    html! {
        <nav class="dm-tree">
            { for model.endpoints.iter().map(|e| {
                let types: Vec<String> = e.device_types.iter()
                    .map(|t| display::device_type_name(t.id).unwrap_or_else(|| display::hex_u32(t.id)))
                    .collect();
                html! {
                    <div class="dm-endpoint">
                        <p class="dm-endpoint-title">{ format!("Endpoint {}", e.id) }</p>
                        <p class="dm-endpoint-types">{ types.join(", ") }</p>
                        <ul>
                            if !controls::of(e).is_empty() {
                                <li>
                                    <button class={classes!("dm-cluster", "is-controls", (view.controls == Some(e.id)).then_some("is-selected"))}
                                        onclick={link.callback({ let id = e.id; move |_| model_msg(ModelMsg::SelectControls(id)) })}>
                                        <span>{ "Controls" }</span>
                                    </button>
                                </li>
                            }
                            { for e.clusters.iter().map(|c| {
                                let (endpoint, id) = (e.id, c.id);
                                let selected = view.controls.is_none() && view.selected == Some((endpoint, id));
                                html! {
                                    <li>
                                        <button class={classes!("dm-cluster", selected.then_some("is-selected"))}
                                            onclick={link.callback(move |_| model_msg(ModelMsg::Select(endpoint, id)))}>
                                            <span>{ c.name.clone().unwrap_or_else(|| display::hex_u32(c.id)) }</span>
                                            <span class="dm-id">{ cluster_id(c.id) }</span>
                                        </button>
                                    </li>
                                }
                            }) }
                        </ul>
                    </div>
                }
            }) }
        </nav>
    }
}

/// An endpoint's controls.
fn view_controls(
    view: &ModelView,
    model: &DeviceModel,
    endpoint: &matter_peek_core::api::EndpointModel,
    link: &Scope<App>,
) -> Html {
    let types: Vec<String> = endpoint
        .device_types
        .iter()
        .map(|t| display::device_type_name(t.id).unwrap_or_else(|| display::hex_u32(t.id)))
        .collect();

    html! {
        <div class="dm-cluster-view">
            <h3 class="dm-title">
                { types.join(", ") }
                <span class="dm-id">{ format!("endpoint {}", endpoint.id) }</span>
            </h3>
            if let Some(outcome) = view.outcome.as_ref().filter(|o| o.is_err()) {
                <p class="dm-outcome is-error">{ match outcome { Ok(text) | Err(text) => text.clone() } }</p>
            }
            { controls::view(view, model, endpoint, link) }
        </div>
    }
}

/// The cluster shown: its attributes, then its commands.
fn view_cluster(view: &ModelView, model: &DeviceModel, link: &Scope<App>) -> Html {
    let Some(cluster) = view.cluster() else {
        return html! { <div class="dm-cluster-view"></div> };
    };
    let (own, global): (Vec<_>, Vec<_>) = cluster
        .attributes
        .iter()
        .partition(|a| a.id < FIRST_GLOBAL_ATTRIBUTE);

    html! {
        <div class="dm-cluster-view">
            <h3 class="dm-title">
                { cluster.name.clone().unwrap_or_else(|| display::hex_u32(cluster.id)) }
                <span class="dm-id">{ cluster_id(cluster.id) }</span>
            </h3>
            if let Some(outcome) = &view.outcome {
                <p class={classes!("dm-outcome", outcome.is_err().then_some("is-error"))}>
                    { match outcome { Ok(text) | Err(text) => text.clone() } }
                </p>
            }
            <table class="table is-fullwidth is-narrow dm-attrs">
                <tbody>
                    { for own.iter().chain(global.iter()).map(|a| view_attribute(view, model, a, link)) }
                </tbody>
            </table>
            if !cluster.commands.is_empty() {
                <h4 class="dm-subtitle">{ "Commands" }</h4>
                <div class="dm-commands">
                    { for cluster.commands.iter().map(|c| view_command(view, model, c, link)) }
                </div>
            }
        </div>
    }
}

fn view_attribute(
    view: &ModelView,
    model: &DeviceModel,
    a: &AttributeModel,
    link: &Scope<App>,
) -> Html {
    let id = a.id;
    let editing = view
        .editing
        .as_ref()
        .filter(|(attr, _)| *attr == id)
        .map(|(_, draft)| draft);

    let value = match (&a.value, &a.error) {
        (Some(value), _) => html! { { value_text(a.value_type.as_ref(), value) } },
        (None, Some(e)) => html! { <span class="dm-error">{ e.clone() }</span> },
        (None, None) => html! {},
    };

    html! {
        <tr class={classes!((a.id >= FIRST_GLOBAL_ATTRIBUTE).then_some("dm-global"))}>
            <td class="dm-name">
                { a.name.clone().unwrap_or_else(|| display::hex_u32(a.id)) }
                <span class="dm-id">{ format!(" {}", display::hex_u16(a.id as u16)) }</span>
            </td>
            <td class="dm-value">
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
            </td>
            <td class="dm-actions">
                if a.writable && editing.is_none() {
                    <button class="icon-action" title="Write it"
                        onclick={link.callback(move |_| model_msg(ModelMsg::Edit(id)))}>
                        { icon(Icon::Edit, 16) }
                    </button>
                }
            </td>
        </tr>
    }
}

fn view_command(
    view: &ModelView,
    model: &DeviceModel,
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
        <div class="dm-command">
            <div class="dm-command-head">
                <span class="dm-name">{ name }</span>
                if c.timed {
                    { ui::pill("Timed", None, Some("Invoked with a timed interaction".into())) }
                }
                if open.is_none() {
                    <button class="button is-small is-soft"
                        onclick={link.callback(move |_| model_msg(ModelMsg::OpenCommand(id)))}>
                        { if c.fields.is_empty() { "Invoke…" } else { "Fill in…" } }
                    </button>
                }
            </div>
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

/// What to show for a value: a string or a name as is, a list joined, a
/// temperature or a percentage also in its unit.
fn value_text(ty: Option<&ValueType>, json: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return json.to_string();
    };
    let unit = |v: f64| match ty.map(|t| t.idl.as_str()) {
        Some("temperature") => Some(format!("{:.2} °C", v / 100.0)),
        Some("percent") => Some(format!("{v} %")),
        Some("percent100ths") => Some(format!("{:.2} %", v / 100.0)),
        _ => None,
    };

    match &value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => match n.as_f64().and_then(unit) {
            Some(text) => format!("{text} ({n})"),
            None => n.to_string(),
        },
        serde_json::Value::Array(items) if items.iter().all(|i| !i.is_object()) => items
            .iter()
            .map(|i| match i {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect::<Vec<_>>()
            .join(", "),
        other => other.to_string(),
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

/// Copy the values `partial` read into `shown`.
fn merge(shown: &mut DeviceModel, partial: &DeviceModel) {
    for e in &partial.endpoints {
        let Some(endpoint) = shown.endpoints.iter_mut().find(|s| s.id == e.id) else {
            continue;
        };
        for c in &e.clusters {
            let Some(cluster) = endpoint.clusters.iter_mut().find(|s| s.id == c.id) else {
                continue;
            };
            for a in &c.attributes {
                if let Some(attr) = cluster.attributes.iter_mut().find(|s| s.id == a.id) {
                    attr.value = a.value.clone();
                    attr.error = a.error.clone();
                }
            }
        }
    }
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
