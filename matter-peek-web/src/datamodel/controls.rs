//! The quick controls of an endpoint: switches, sliders, choices and readings
//! for the clusters people act on, declared per cluster and acting through
//! the generic writes and invokes.

use yew::html::Scope;
use yew::prelude::*;

use matter_peek_core::api::{AttributeModel, ClusterModel, DeviceModel, EndpointModel};

use super::{model_msg, Action, ModelMsg, ModelView};
use crate::{field_value, select_value, App, Msg};

/// A control, and the cluster it acts on.
pub(super) enum Control {
    /// On or off, as attribute `attr` reads it, set with commands `on` and
    /// `off`.
    Switch {
        cluster: u32,
        attr: u32,
        on: u32,
        off: u32,
        label: &'static str,
    },
    /// A number between `min` and `max` (raw), shown divided by `scale` in
    /// `unit`, as attribute `attr` reads it, set as `set` says.
    Slider {
        cluster: u32,
        attr: u32,
        min: Bound,
        max: Bound,
        step: f64,
        scale: f64,
        unit: &'static str,
        set: Set,
        label: &'static str,
    },
    /// An enum attribute's value, among its choices.
    Choice {
        cluster: u32,
        attr: u32,
        label: &'static str,
    },
    /// Commands without fields.
    Buttons {
        cluster: u32,
        commands: &'static [(u32, &'static str)],
        label: &'static str,
    },
    /// A value to read, divided by `scale`, in `unit`.
    Reading {
        cluster: u32,
        attr: u32,
        scale: f64,
        unit: &'static str,
        label: &'static str,
    },
}

/// A bound of a slider: fixed, or an attribute of the cluster (this if it
/// has none).
pub(super) enum Bound {
    Fixed(f64),
    Attr(u32, f64),
}

/// How a slider sets its value.
pub(super) enum Set {
    /// By writing its attribute.
    Attr,
    /// As `field` of command `cmd`, its other fields as `others` (JSON).
    Command {
        cmd: u32,
        field: &'static str,
        others: &'static str,
    },
}

const ON_OFF: u32 = 0x0006;
const LEVEL_CONTROL: u32 = 0x0008;
const BOOLEAN_STATE: u32 = 0x0045;
const WINDOW_COVERING: u32 = 0x0102;
const THERMOSTAT: u32 = 0x0201;
const FAN_CONTROL: u32 = 0x0202;
const ILLUMINANCE: u32 = 0x0400;
const TEMPERATURE: u32 = 0x0402;
const HUMIDITY: u32 = 0x0405;
const OCCUPANCY: u32 = 0x0406;

/// The controls, by cluster.
static CONTROLS: &[Control] = &[
    Control::Switch {
        cluster: ON_OFF,
        attr: 0x0000,
        on: 0x01,
        off: 0x00,
        label: "Power",
    },
    Control::Slider {
        cluster: LEVEL_CONTROL,
        attr: 0x0000,
        min: Bound::Attr(0x0002, 1.0),
        max: Bound::Attr(0x0003, 254.0),
        step: 1.0,
        scale: 1.0,
        unit: "",
        set: Set::Command {
            cmd: 0x04,
            field: "Level",
            others: r#"{"TransitionTime":null,"OptionsMask":[],"OptionsOverride":[]}"#,
        },
        label: "Level",
    },
    Control::Reading {
        cluster: THERMOSTAT,
        attr: 0x0000,
        scale: 100.0,
        unit: "°C",
        label: "Room temperature",
    },
    Control::Choice {
        cluster: THERMOSTAT,
        attr: 0x001C,
        label: "Mode",
    },
    Control::Slider {
        cluster: THERMOSTAT,
        attr: 0x0012,
        min: Bound::Attr(0x0015, 700.0),
        max: Bound::Attr(0x0016, 3000.0),
        step: 50.0,
        scale: 100.0,
        unit: "°C",
        set: Set::Attr,
        label: "Heat to",
    },
    Control::Slider {
        cluster: THERMOSTAT,
        attr: 0x0011,
        min: Bound::Attr(0x0017, 1600.0),
        max: Bound::Attr(0x0018, 3200.0),
        step: 50.0,
        scale: 100.0,
        unit: "°C",
        set: Set::Attr,
        label: "Cool to",
    },
    Control::Choice {
        cluster: FAN_CONTROL,
        attr: 0x0000,
        label: "Fan",
    },
    Control::Slider {
        cluster: FAN_CONTROL,
        attr: 0x0002,
        min: Bound::Fixed(0.0),
        max: Bound::Fixed(100.0),
        step: 1.0,
        scale: 1.0,
        unit: "%",
        set: Set::Attr,
        label: "Fan speed",
    },
    Control::Buttons {
        cluster: WINDOW_COVERING,
        commands: &[(0x00, "Open"), (0x02, "Stop"), (0x01, "Close")],
        label: "Cover",
    },
    Control::Reading {
        cluster: TEMPERATURE,
        attr: 0x0000,
        scale: 100.0,
        unit: "°C",
        label: "Temperature",
    },
    Control::Reading {
        cluster: HUMIDITY,
        attr: 0x0000,
        scale: 100.0,
        unit: "%",
        label: "Humidity",
    },
    Control::Reading {
        cluster: ILLUMINANCE,
        attr: 0x0000,
        scale: 1.0,
        unit: "",
        label: "Illuminance",
    },
    Control::Reading {
        cluster: OCCUPANCY,
        attr: 0x0000,
        scale: 1.0,
        unit: "",
        label: "Occupancy",
    },
    Control::Reading {
        cluster: BOOLEAN_STATE,
        attr: 0x0000,
        scale: 1.0,
        unit: "",
        label: "State",
    },
];

impl Control {
    fn cluster(&self) -> u32 {
        match self {
            Self::Switch { cluster, .. }
            | Self::Slider { cluster, .. }
            | Self::Choice { cluster, .. }
            | Self::Buttons { cluster, .. }
            | Self::Reading { cluster, .. } => *cluster,
        }
    }

    /// Whether `cluster` has what the control needs.
    fn fits(&self, cluster: &ClusterModel) -> bool {
        match self {
            Self::Switch { attr, .. }
            | Self::Slider { attr, .. }
            | Self::Choice { attr, .. }
            | Self::Reading { attr, .. } => attribute(cluster, *attr).is_some(),
            Self::Buttons { commands, .. } => commands
                .iter()
                .all(|(id, _)| cluster.commands.iter().any(|c| c.id == *id)),
        }
    }
}

/// The controls fitting `endpoint`, with their clusters.
pub(super) fn of(endpoint: &EndpointModel) -> Vec<(&'static Control, &ClusterModel)> {
    CONTROLS
        .iter()
        .filter_map(|control| {
            let cluster = endpoint
                .clusters
                .iter()
                .find(|c| c.id == control.cluster())?;
            control.fits(cluster).then_some((control, cluster))
        })
        .collect()
}

/// The controls of `endpoint`.
pub(super) fn view(
    view: &ModelView,
    model: &DeviceModel,
    endpoint: &EndpointModel,
    link: &Scope<App>,
) -> Html {
    let controls = of(endpoint);
    let ep = endpoint.id;

    html! {
        <div class="dm-controls">
            { for controls.into_iter().map(|(control, cluster)| html! {
                <div class="dm-control">
                    { view_control(view, model, ep, control, cluster, link) }
                </div>
            }) }
        </div>
    }
}

fn view_control(
    view: &ModelView,
    model: &DeviceModel,
    endpoint: u16,
    control: &Control,
    cluster: &ClusterModel,
    link: &Scope<App>,
) -> Html {
    let busy = view.busy;
    let cluster_id = cluster.id;

    match control {
        Control::Switch {
            attr,
            on,
            off,
            label,
            ..
        } => {
            let state = number(attribute(cluster, *attr)).is_some_and(|v| v != 0.0)
                || bool_of(attribute(cluster, *attr));
            let cmd = if state { *off } else { *on };
            html! {
                <>
                    <span class="dm-control-label">{ *label }</span>
                    <button class={classes!("button", "is-small", if state { "is-success" } else { "is-soft" })}
                        disabled={busy}
                        onclick={link.callback(move |_| act(Action::Invoke {
                            endpoint, cluster: cluster_id, command: cmd, fields: "{}".into(), refresh: true,
                        }))}>
                        { if state { "On" } else { "Off" } }
                    </button>
                </>
            }
        }
        Control::Slider {
            attr,
            min,
            max,
            step,
            scale,
            unit,
            set,
            label,
            ..
        } => {
            let value = number(attribute(cluster, *attr));
            let (min, max) = (bound(cluster, min), bound(cluster, max));
            let shown = value.map_or_else(|| "—".to_string(), |v| scaled(v, *scale, unit));
            let attr = *attr;
            let set = match set {
                Set::Attr => None,
                Set::Command { cmd, field, others } => Some((*cmd, *field, *others)),
            };
            html! {
                <>
                    <span class="dm-control-label">{ *label }</span>
                    <input class="dm-slider" type="range" disabled={busy}
                        min={min.to_string()} max={max.to_string()} step={step.to_string()}
                        value={value.unwrap_or(min).to_string()}
                        onchange={link.callback(move |e: Event| {
                            let v = field_value(&e);
                            act(match set {
                                None => Action::Write { endpoint, cluster: cluster_id, attribute: attr, value: v },
                                Some((cmd, field, others)) => {
                                    let mut fields: serde_json::Value = serde_json::from_str(others).unwrap_or_default();
                                    fields[field] = serde_json::from_str(&v).unwrap_or_default();
                                    Action::Invoke { endpoint, cluster: cluster_id, command: cmd, fields: fields.to_string(), refresh: true }
                                }
                            })
                        })} />
                    <span class="dm-control-value">{ shown }</span>
                </>
            }
        }
        Control::Choice { attr, label, .. } => {
            let a = attribute(cluster, *attr);
            let current = a
                .and_then(|a| a.value.as_deref())
                .and_then(|v| serde_json::from_str::<serde_json::Value>(v).ok())
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default();
            let names: Vec<String> = a
                .and_then(|a| a.value_type.as_ref()?.schema)
                .and_then(|i| model.types.get(i as usize))
                .map(|info| info.values.iter().map(|v| v.name.clone()).collect())
                .unwrap_or_default();
            let writable = a.is_some_and(|a| a.writable);
            let attr = *attr;
            html! {
                <>
                    <span class="dm-control-label">{ *label }</span>
                    <div class="select is-small">
                        <select disabled={busy || !writable}
                            onchange={link.callback(move |e: Event| act(Action::Write {
                                endpoint, cluster: cluster_id, attribute: attr,
                                value: serde_json::Value::String(select_value(e)).to_string(),
                            }))}>
                            { for names.iter().map(|n| html! {
                                <option value={n.clone()} selected={*n == current}>{ n.clone() }</option>
                            }) }
                        </select>
                    </div>
                </>
            }
        }
        Control::Buttons {
            commands, label, ..
        } => html! {
            <>
                <span class="dm-control-label">{ *label }</span>
                <div class="buttons are-small">
                    { for commands.iter().map(|(cmd, name)| {
                        let cmd = *cmd;
                        html! {
                            <button class="button is-small is-soft" disabled={busy}
                                onclick={link.callback(move |_| act(Action::Invoke {
                                    endpoint, cluster: cluster_id, command: cmd, fields: "{}".into(), refresh: true,
                                }))}>
                                { *name }
                            </button>
                        }
                    }) }
                </div>
            </>
        },
        Control::Reading {
            attr,
            scale,
            unit,
            label,
            ..
        } => {
            let a = attribute(cluster, *attr);
            let shown = match number(a) {
                Some(v) => scaled(v, *scale, unit),
                None => a
                    .and_then(|a| a.value.clone())
                    .filter(|v| v != "null")
                    .map(|v| v.trim_matches('"').to_string())
                    .unwrap_or_else(|| "—".into()),
            };
            html! {
                <>
                    <span class="dm-control-label">{ *label }</span>
                    <span class="dm-control-value is-reading">{ shown }</span>
                </>
            }
        }
    }
}

fn act(action: Action) -> Msg {
    model_msg(ModelMsg::Act(action))
}

fn attribute(cluster: &ClusterModel, id: u32) -> Option<&AttributeModel> {
    cluster.attributes.iter().find(|a| a.id == id)
}

fn number(attr: Option<&AttributeModel>) -> Option<f64> {
    serde_json::from_str::<serde_json::Value>(attr?.value.as_deref()?)
        .ok()?
        .as_f64()
}

fn bool_of(attr: Option<&AttributeModel>) -> bool {
    attr.and_then(|a| a.value.as_deref()) == Some("true")
}

fn bound(cluster: &ClusterModel, bound: &Bound) -> f64 {
    match bound {
        Bound::Fixed(v) => *v,
        Bound::Attr(attr, fallback) => number(attribute(cluster, *attr)).unwrap_or(*fallback),
    }
}

fn scaled(v: f64, scale: f64, unit: &str) -> String {
    let text = if scale == 1.0 {
        format!("{v}")
    } else {
        format!("{:.1}", v / scale)
    };
    if unit.is_empty() {
        text
    } else {
        format!("{text} {unit}")
    }
}
