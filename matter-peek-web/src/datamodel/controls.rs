//! The controls of an endpoint, for people to act on a device: switches,
//! sliders, choices, buttons and readings.
//!
//! Where they come from, cluster by cluster: curated ones for the clusters
//! people act on most; one per shape for the clusters that share one (modes,
//! operational states, measurements, filters and tanks); for any other cluster
//! of an application endpoint, ones derived from its schema.

use yew::prelude::*;

use matter_peek_core::api::{AttributeModel, ClusterModel, DeviceModel, EndpointModel, ValueKind};

use super::Action;
use crate::{field_value, select_value};

const IDENTIFY: u32 = 0x0003;
const GROUPS: u32 = 0x0004;
const SCENES: u32 = 0x0005;
const ON_OFF: u32 = 0x0006;
const LEVEL_CONTROL: u32 = 0x0008;
const DESCRIPTOR: u32 = 0x001D;
const BINDING: u32 = 0x001E;
const POWER_SOURCE: u32 = 0x002F;
const FIXED_LABEL: u32 = 0x0040;
const USER_LABEL: u32 = 0x0041;
const BOOLEAN_STATE: u32 = 0x0045;
const SCENES_MANAGEMENT: u32 = 0x0062;
const ELECTRICAL_POWER: u32 = 0x0090;
const ELECTRICAL_ENERGY: u32 = 0x0091;
const WINDOW_COVERING: u32 = 0x0102;
const THERMOSTAT: u32 = 0x0201;
const FAN_CONTROL: u32 = 0x0202;
const ILLUMINANCE: u32 = 0x0400;
const TEMPERATURE: u32 = 0x0402;
const PRESSURE: u32 = 0x0403;
const FLOW: u32 = 0x0404;
const HUMIDITY: u32 = 0x0405;
const OCCUPANCY: u32 = 0x0406;

/// The Matter epoch, 2000-01-01, in Unix seconds.
const MATTER_EPOCH_UNIX_SECS: u64 = 946_684_800;

/// The global attributes (attribute lists, revision, …) start here.
const FIRST_GLOBAL_ATTRIBUTE: u32 = 0xFFF8;

/// The plumbing clusters: nothing to act on for people.
const PLUMBING: &[u32] = &[
    IDENTIFY,
    GROUPS,
    SCENES,
    DESCRIPTOR,
    BINDING,
    FIXED_LABEL,
    USER_LABEL,
    SCENES_MANAGEMENT,
];

/// Words of spec names that read better spelled otherwise.
const SPELLINGS: &[(&str, &str)] = &[
    ("Pm1", "PM1"),
    ("Pm10", "PM10"),
    ("Pm25", "PM2.5"),
    ("Rvc", "RVC"),
    ("Hepa", "HEPA"),
    ("Evse", "EVSE"),
];

/// The commands an operational state cluster may offer, in the order shown.
const OPERATIONS: &[&str] = &["Start", "Pause", "Resume", "Stop", "GoHome"];

/// The controls of one cluster.
pub(super) struct Group<'m> {
    pub cluster: &'m ClusterModel,
    /// A heading for controls derived from the cluster's schema.
    pub title: Option<String>,
    pub controls: Vec<Control>,
}

/// A control: its label and what it does.
pub(super) struct Control {
    label: String,
    kind: Kind,
}

enum Kind {
    /// On or off as attribute `attr` reads it, set with commands `on` and
    /// `off`.
    Switch { attr: u32, on: u32, off: u32 },
    /// A writable boolean.
    Toggle { attr: u32 },
    /// A number between `min` and `max` (raw), shown divided by `scale` in
    /// `unit`, as attribute `attr` reads it, set as `set` says.
    Slider {
        attr: u32,
        min: f64,
        max: f64,
        step: f64,
        scale: f64,
        unit: &'static str,
        set: Set,
    },
    /// A writable number, typed in.
    Number { attr: u32 },
    /// A writable enum, among its values.
    Choice { attr: u32 },
    /// One of the modes `supported` lists, as `current` reads it, set with
    /// command `change`.
    Mode {
        supported: u32,
        current: u32,
        change: u32,
    },
    /// Commands needing no fields.
    Buttons { commands: Vec<(u32, String)> },
    /// A value to read.
    Reading { attr: u32, unit: Unit },
}

/// How a slider sets its value.
enum Set {
    /// By writing its attribute.
    Attr,
    /// As `field` of command `cmd`, its other fields as `others` (JSON).
    Command {
        cmd: u32,
        field: &'static str,
        others: &'static str,
    },
}

/// What a reading's raw value is in.
#[derive(Clone, Copy)]
enum Unit {
    /// As read: an enum's name, a number.
    Plain,
    /// Divided by the factor, in the unit.
    Scaled(f64, &'static str),
    /// In the unit an enum attribute names.
    Named(u32),
    /// Illuminance: `10000 × log10(lux) + 1`.
    Lux,
    /// Seconds.
    Duration,
    /// Seconds since 2000-01-01 (the Matter epoch).
    Moment,
    /// A field of a struct, divided by the factor, in the unit.
    Field(&'static str, f64, &'static str),
}

/// The controls of `endpoint`, cluster by cluster: the curated and shaped
/// ones first, then those derived from the schema (with `brief`, only where
/// it has no others).
pub(super) fn of(endpoint: &EndpointModel, brief: bool) -> Vec<Group<'_>> {
    let mut groups: Vec<Group<'_>> = endpoint
        .clusters
        .iter()
        .filter_map(|cluster| {
            let (title, controls) = match curated(cluster).or_else(|| shaped(cluster)) {
                Some(controls) => (None, controls),
                None if endpoint.id != 0
                    && !PLUMBING.contains(&cluster.id)
                    && cluster.name.is_some() =>
                {
                    (cluster.name.as_deref().map(words), derived(cluster))
                }
                None => return None,
            };
            (!controls.is_empty()).then_some(Group {
                cluster,
                title,
                controls,
            })
        })
        .collect();
    groups.sort_by_key(|group| group.title.is_some());
    if brief && groups.first().is_some_and(|group| group.title.is_none()) {
        groups.retain(|group| group.title.is_none());
    }

    groups
}

/// Whether `endpoint` has controls (`brief`: as [`of`] says).
pub(crate) fn any(endpoint: &EndpointModel, brief: bool) -> bool {
    !of(endpoint, brief).is_empty()
}

/// The controls of `endpoint` (`brief`: as [`of`] says), disabled while
/// `busy`; what they do goes to `on_act`.
pub(crate) fn view(
    model: &DeviceModel,
    endpoint: &EndpointModel,
    brief: bool,
    busy: bool,
    on_act: &Callback<Action>,
) -> Html {
    let ep = endpoint.id;

    html! {
        <div class="dm-controls">
            { for of(endpoint, brief).into_iter().map(|group| html! {
                <>
                    if let Some(title) = &group.title {
                        <p class="dm-control-group">{ title.clone() }</p>
                    }
                    { for group.controls.iter().map(|control| html! {
                        <div class="dm-control">
                            <span class="dm-control-label">{ control.label.clone() }</span>
                            { view_control(model, ep, control, group.cluster, busy, on_act) }
                        </div>
                    }) }
                </>
            }) }
        </div>
    }
}

/// The curated controls of `cluster`, if it is one of those.
fn curated(cluster: &ClusterModel) -> Option<Vec<Control>> {
    let reading = |attr, label: &str, unit| control(label, Kind::Reading { attr, unit });

    let controls = match cluster.id {
        ON_OFF => vec![control(
            "Power",
            Kind::Switch {
                attr: 0x0000,
                on: 0x01,
                off: 0x00,
            },
        )],
        LEVEL_CONTROL => vec![control(
            "Level",
            Kind::Slider {
                attr: 0x0000,
                min: number(attribute(cluster, 0x0002)).unwrap_or(1.0),
                max: number(attribute(cluster, 0x0003)).unwrap_or(254.0),
                step: 1.0,
                scale: 1.0,
                unit: "",
                set: Set::Command {
                    cmd: 0x04,
                    field: "Level",
                    others: r#"{"TransitionTime":null,"OptionsMask":[],"OptionsOverride":[]}"#,
                },
            },
        )],
        THERMOSTAT => vec![
            reading(0x0000, "Room temperature", Unit::Scaled(100.0, "°C")),
            control("Mode", Kind::Choice { attr: 0x001C }),
            setpoint(cluster, "Heat to", 0x0012, 0x0015, 0x0016, 700.0, 3000.0),
            setpoint(cluster, "Cool to", 0x0011, 0x0017, 0x0018, 1600.0, 3200.0),
        ],
        FAN_CONTROL => vec![
            control("Fan", Kind::Choice { attr: 0x0000 }),
            control(
                "Fan speed",
                Kind::Slider {
                    attr: 0x0002,
                    min: 0.0,
                    max: 100.0,
                    step: 1.0,
                    scale: 1.0,
                    unit: "%",
                    set: Set::Attr,
                },
            ),
        ],
        WINDOW_COVERING => vec![
            control(
                "Cover",
                Kind::Buttons {
                    commands: vec![
                        (0x00, "Open".into()),
                        (0x02, "Stop".into()),
                        (0x01, "Close".into()),
                    ],
                },
            ),
            reading(0x000E, "Position", Unit::Scaled(100.0, "% closed")),
        ],
        POWER_SOURCE => vec![
            reading(0x0000, "Power source", Unit::Plain),
            reading(0x000C, "Battery", Unit::Scaled(2.0, "%")),
            reading(0x000E, "Battery level", Unit::Plain),
        ],
        BOOLEAN_STATE => vec![reading(0x0000, "Boolean state", Unit::Plain)],
        OCCUPANCY => vec![reading(0x0000, "Occupancy", Unit::Plain)],
        ELECTRICAL_POWER => vec![
            reading(0x0008, "Power", Unit::Scaled(1000.0, "W")),
            reading(0x0004, "Voltage", Unit::Scaled(1000.0, "V")),
            reading(0x0005, "Current", Unit::Scaled(1000.0, "A")),
        ],
        ELECTRICAL_ENERGY => vec![
            reading(0x0001, "Energy used", Unit::Field("Energy", 1e6, "kWh")),
            reading(0x0002, "Energy fed in", Unit::Field("Energy", 1e6, "kWh")),
        ],
        _ => return None,
    };

    Some(present(cluster, controls))
}

/// A setpoint slider, bounded by its limit attributes.
fn setpoint(
    cluster: &ClusterModel,
    label: &str,
    attr: u32,
    min: u32,
    max: u32,
    min_default: f64,
    max_default: f64,
) -> Control {
    control(
        label,
        Kind::Slider {
            attr,
            min: number(attribute(cluster, min)).unwrap_or(min_default),
            max: number(attribute(cluster, max)).unwrap_or(max_default),
            step: 50.0,
            scale: 100.0,
            unit: "°C",
            set: Set::Attr,
        },
    )
}

/// The controls of a cluster of a shape many share, if it has one: by the
/// names of its attributes and commands.
fn shaped(cluster: &ClusterModel) -> Option<Vec<Control>> {
    let name = cluster.name.as_deref().unwrap_or_default();
    let attr = |name: &str| named_attribute(cluster, name).map(|a| a.id);
    let cmd = |name: &str| {
        cluster
            .commands
            .iter()
            .find(|c| c.name.as_deref() == Some(name))
    };

    // Modes: every mode cluster, and Mode Select.
    if let (Some(supported), Some(current), Some(change)) = (
        attr("SupportedModes"),
        attr("CurrentMode"),
        cmd("ChangeToMode"),
    ) {
        return Some(vec![control(
            &words(name),
            Kind::Mode {
                supported,
                current,
                change: change.id,
            },
        )]);
    }

    // Operational states: the device's state, and the operations it accepts.
    if let Some(state) = attr("OperationalState") {
        // Prefixed with the kind of operation, if not the generic one.
        let what = |label: &str| {
            let prefix = words(name.strip_suffix("OperationalState").unwrap_or_default());
            if prefix.is_empty() {
                label.to_string()
            } else {
                format!("{prefix} {}", label.to_lowercase())
            }
        };
        let commands = OPERATIONS
            .iter()
            .filter_map(|op| cmd(op).map(|c| (c.id, words(op))))
            .collect::<Vec<_>>();
        let mut controls = vec![control(
            &what("State"),
            Kind::Reading {
                attr: state,
                unit: Unit::Plain,
            },
        )];
        if let Some(countdown) = attr("CountdownTime") {
            controls.push(control(
                &what("Time left"),
                Kind::Reading {
                    attr: countdown,
                    unit: Unit::Duration,
                },
            ));
        }
        if !commands.is_empty() {
            controls.push(control(&what("Operation"), Kind::Buttons { commands }));
        }
        return Some(present(cluster, controls));
    }

    // Concentrations: in the unit the device names.
    if let (Some(value), Some(unit)) = (attr("MeasuredValue"), attr("MeasurementUnit")) {
        let what = words(
            name.strip_suffix("ConcentrationMeasurement")
                .unwrap_or(name),
        );
        let mut controls = vec![control(
            &what,
            Kind::Reading {
                attr: value,
                unit: Unit::Named(unit),
            },
        )];
        if let Some(level) = attr("LevelValue") {
            controls.push(control(
                &format!("{what} level"),
                Kind::Reading {
                    attr: level,
                    unit: Unit::Plain,
                },
            ));
        }
        return Some(present(cluster, controls));
    }

    // Measurements.
    if let Some(value) = attr("MeasuredValue") {
        let unit = match cluster.id {
            TEMPERATURE => Unit::Scaled(100.0, "°C"),
            HUMIDITY => Unit::Scaled(100.0, "%"),
            PRESSURE => Unit::Scaled(10.0, "kPa"),
            FLOW => Unit::Scaled(10.0, "m³/h"),
            ILLUMINANCE => Unit::Lux,
            _ => Unit::Plain,
        };
        let what = words(name.strip_suffix("Measurement").unwrap_or(name));
        return Some(vec![control(&what, Kind::Reading { attr: value, unit })]);
    }

    // Filters and tanks: their condition, reset when replaced.
    if let (Some(condition), Some(reset)) = (attr("Condition"), cmd("ResetCondition")) {
        let what = words(name.strip_suffix("Monitoring").unwrap_or(name));
        let mut controls = vec![control(
            &what,
            Kind::Reading {
                attr: condition,
                unit: Unit::Scaled(1.0, "%"),
            },
        )];
        if let Some(change) = attr("ChangeIndication") {
            controls.push(control(
                "Change",
                Kind::Reading {
                    attr: change,
                    unit: Unit::Plain,
                },
            ));
        }
        controls.push(control(
            "Replaced",
            Kind::Buttons {
                commands: vec![(reset.id, "Reset".into())],
            },
        ));
        return Some(present(cluster, controls));
    }

    None
}

/// The controls of any other cluster, from its schema: a writable boolean as
/// a toggle, a writable enum as a choice, a writable number typed in, the
/// other values as readings, and the commands needing no fields as buttons.
fn derived(cluster: &ClusterModel) -> Vec<Control> {
    let mut controls = Vec::new();

    for a in &cluster.attributes {
        let Some(ty) = a.value_type.as_ref() else {
            continue;
        };
        if a.id >= FIRST_GLOBAL_ATTRIBUTE || ty.list || ty.kind == ValueKind::Struct {
            continue;
        }
        let label = words(a.name.as_deref().unwrap_or_default());
        let kind = match (a.writable, ty.kind) {
            (true, ValueKind::Bool) => Kind::Toggle { attr: a.id },
            (true, ValueKind::Enum) => Kind::Choice { attr: a.id },
            (true, ValueKind::UInt | ValueKind::Int | ValueKind::Float) => {
                Kind::Number { attr: a.id }
            }
            (_, ValueKind::Unknown) => continue,
            _ => Kind::Reading {
                attr: a.id,
                unit: idl_unit(&ty.idl),
            },
        };
        controls.push(Control { label, kind });
    }

    let commands: Vec<(u32, String)> = cluster
        .commands
        .iter()
        .filter(|c| c.fields.iter().all(|f| f.value_type.optional))
        .filter_map(|c| Some((c.id, words(c.name.as_deref()?))))
        .collect();
    if !commands.is_empty() {
        controls.push(control("Commands", Kind::Buttons { commands }));
    }

    controls
}

fn view_control(
    model: &DeviceModel,
    endpoint: u16,
    control: &Control,
    cluster: &ClusterModel,
    busy: bool,
    on_act: &Callback<Action>,
) -> Html {
    let cluster_id = cluster.id;

    match &control.kind {
        Kind::Switch { attr, on, off } => {
            let state = is_on(attribute(cluster, *attr));
            let cmd = if state { *off } else { *on };
            switch(state, ("On", "Off"), busy, on_act.reform(move |_| Action::Invoke {
                endpoint, cluster: cluster_id, command: cmd, fields: "{}".into(),
            }))
        }
        Kind::Toggle { attr } => {
            let state = is_on(attribute(cluster, *attr));
            let attr = *attr;
            switch(state, ("Yes", "No"), busy, on_act.reform(move |_| Action::Write {
                endpoint, cluster: cluster_id, attribute: attr, value: (!state).to_string(),
            }))
        }
        Kind::Slider {
            attr,
            min,
            max,
            step,
            scale,
            unit,
            set,
        } => {
            let value = number(attribute(cluster, *attr));
            let shown = value.map_or_else(|| "—".to_string(), |v| scaled(v, *scale, unit));
            let (attr, min) = (*attr, *min);
            let set = match set {
                Set::Attr => None,
                Set::Command { cmd, field, others } => Some((*cmd, *field, *others)),
            };
            html! {
                <>
                    <input class="dm-slider" type="range" disabled={busy}
                        min={min.to_string()} max={max.to_string()} step={step.to_string()}
                        value={value.unwrap_or(min).to_string()}
                        onchange={on_act.reform(move |e: Event| {
                            let v = field_value(&e);
                            match set {
                                None => Action::Write { endpoint, cluster: cluster_id, attribute: attr, value: v },
                                Some((cmd, field, others)) => {
                                    let mut fields: serde_json::Value = serde_json::from_str(others).unwrap_or_default();
                                    fields[field] = serde_json::from_str(&v).unwrap_or_default();
                                    Action::Invoke { endpoint, cluster: cluster_id, command: cmd, fields: fields.to_string() }
                                }
                            }
                        })} />
                    <span class="dm-control-value">{ shown }</span>
                </>
            }
        }
        Kind::Number { attr } => {
            let value = attribute(cluster, *attr)
                .and_then(|a| a.value.clone())
                .filter(|v| v != "null")
                .unwrap_or_default();
            let attr = *attr;
            html! {
                <input class="input is-small dm-number" type="text" disabled={busy}
                    value={value}
                    onchange={on_act.reform(move |e: Event| Action::Write {
                        endpoint, cluster: cluster_id, attribute: attr, value: field_value(&e),
                    })} />
            }
        }
        Kind::Choice { attr } => {
            let a = attribute(cluster, *attr);
            let current = string(a).unwrap_or_default();
            let names: Vec<String> = a
                .and_then(|a| a.value_type.as_ref()?.schema)
                .and_then(|i| model.types.get(i as usize))
                .map(|info| info.values.iter().map(|v| v.name.clone()).collect())
                .unwrap_or_default();
            let writable = a.is_some_and(|a| a.writable);
            let attr = *attr;
            html! {
                <div class="select is-small">
                    <select disabled={busy || !writable}
                        onchange={on_act.reform(move |e: Event| Action::Write {
                            endpoint, cluster: cluster_id, attribute: attr,
                            value: serde_json::Value::String(select_value(e)).to_string(),
                        })}>
                        { for names.iter().map(|n| html! {
                            <option value={n.clone()} selected={*n == current}>{ n.clone() }</option>
                        }) }
                    </select>
                </div>
            }
        }
        Kind::Mode {
            supported,
            current,
            change,
        } => {
            let modes = modes(attribute(cluster, *supported));
            let current = number(attribute(cluster, *current)).map(|v| v as u64);
            let change = *change;
            html! {
                <div class="select is-small">
                    <select disabled={busy || modes.is_empty()}
                        onchange={on_act.reform(move |e: Event| Action::Invoke {
                            endpoint, cluster: cluster_id, command: change,
                            fields: serde_json::json!({ "NewMode": select_value(e).parse::<u64>().unwrap_or_default() }).to_string(),
                        })}>
                        { for modes.iter().map(|(mode, label)| html! {
                            <option value={mode.to_string()} selected={Some(*mode) == current}>{ label.clone() }</option>
                        }) }
                    </select>
                </div>
            }
        }
        Kind::Buttons { commands } => html! {
            <div class="buttons are-small">
                { for commands.iter().filter(|(cmd, _)| cluster.commands.iter().any(|c| c.id == *cmd)).map(|(cmd, name)| {
                    let cmd = *cmd;
                    html! {
                        <button class="button is-small is-soft" disabled={busy}
                            onclick={on_act.reform(move |_| Action::Invoke {
                                endpoint, cluster: cluster_id, command: cmd, fields: "{}".into(),
                            })}>
                            { name.clone() }
                        </button>
                    }
                }) }
            </div>
        },
        Kind::Reading { attr, unit } => html! {
            <span class="dm-control-value is-reading">{ reading(cluster, *attr, *unit) }</span>
        },
    }
}

/// A two-state switch showing `state` (`labels`: when on, when off); a click
/// turns it over.
fn switch(state: bool, labels: (&str, &str), busy: bool, onclick: Callback<MouseEvent>) -> Html {
    html! {
        <button class={classes!("dm-switch", state.then_some("is-on"))} role="switch"
            aria-checked={state.to_string()} disabled={busy} {onclick}>
            <span class="dm-switch-track"><span class="dm-switch-knob"></span></span>
            <span class="dm-switch-label">{ if state { labels.0 } else { labels.1 } }</span>
        </button>
    }
}

/// What a reading shows.
fn reading(cluster: &ClusterModel, attr: u32, unit: Unit) -> String {
    let Some(value) = json(attribute(cluster, attr)) else {
        return "—".into();
    };

    match (unit, &value) {
        (_, serde_json::Value::Null) => "—".into(),
        (Unit::Field(field, scale, unit), serde_json::Value::Object(fields)) => fields
            .get(field)
            .and_then(serde_json::Value::as_f64)
            .map_or_else(|| "—".into(), |v| scaled(v, scale, unit)),
        (Unit::Scaled(scale, unit), serde_json::Value::Number(n)) => {
            scaled(n.as_f64().unwrap_or_default(), scale, unit)
        }
        (Unit::Named(unit_attr), serde_json::Value::Number(n)) => {
            let unit = string(attribute(cluster, unit_attr)).unwrap_or_default();
            format!("{n} {unit}")
        }
        (Unit::Lux, serde_json::Value::Number(n)) => {
            let raw = n.as_f64().unwrap_or_default();
            let lux = 10f64.powf((raw - 1.0) / 10000.0);
            format!("{lux:.0} lx")
        }
        (Unit::Duration, serde_json::Value::Number(n)) => {
            matter_peek_core::api::display::duration(n.as_u64().unwrap_or_default())
        }
        (Unit::Moment, serde_json::Value::Number(n)) => {
            let unix_secs = n.as_u64().unwrap_or_default() + MATTER_EPOCH_UNIX_SECS;
            matter_peek_core::api::display::utc_label(unix_secs * 1000)
        }
        (_, serde_json::Value::Bool(b)) => if *b { "Yes" } else { "No" }.into(),
        (_, serde_json::Value::String(s)) => s.clone(),
        (_, serde_json::Value::Array(items)) if items.is_empty() => "None".into(),
        (_, serde_json::Value::Array(items)) => items
            .iter()
            .map(|i| i.as_str().map_or_else(|| i.to_string(), str::to_string))
            .collect::<Vec<_>>()
            .join(", "),
        (_, other) => other.to_string(),
    }
}

/// The unit of a value by its IDL type, if it says one.
fn idl_unit(idl: &str) -> Unit {
    match idl {
        "temperature" => Unit::Scaled(100.0, "°C"),
        "percent" => Unit::Scaled(1.0, "%"),
        "percent100ths" => Unit::Scaled(100.0, "%"),
        "elapsed_s" => Unit::Duration,
        "epoch_s" => Unit::Moment,
        _ => Unit::Plain,
    }
}

/// The modes a `SupportedModes` list offers: their values and labels.
fn modes(attr: Option<&AttributeModel>) -> Vec<(u64, String)> {
    let Some(serde_json::Value::Array(modes)) = json(attr) else {
        return Vec::new();
    };

    modes
        .iter()
        .filter_map(|m| {
            let mode = m.get("Mode")?.as_u64()?;
            let label = m
                .get("Label")
                .and_then(serde_json::Value::as_str)
                .map_or_else(|| mode.to_string(), str::to_string);
            Some((mode, label))
        })
        .collect()
}

/// `controls` without those whose attribute the cluster lacks.
fn present(cluster: &ClusterModel, controls: Vec<Control>) -> Vec<Control> {
    controls
        .into_iter()
        .filter(|c| match &c.kind {
            Kind::Switch { attr, .. }
            | Kind::Toggle { attr }
            | Kind::Slider { attr, .. }
            | Kind::Number { attr }
            | Kind::Choice { attr }
            | Kind::Reading { attr, .. } => attribute(cluster, *attr).is_some(),
            Kind::Mode { supported, .. } => attribute(cluster, *supported).is_some(),
            Kind::Buttons { commands } => commands
                .iter()
                .any(|(id, _)| cluster.commands.iter().any(|c| c.id == *id)),
        })
        .collect()
}

fn control(label: &str, kind: Kind) -> Control {
    Control {
        label: label.to_string(),
        kind,
    }
}

/// A spec name in words: `OccupiedCoolingSetpoint` → `Occupied cooling
/// setpoint`, `COState` → `CO state`.
fn words(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();

    for (i, c) in chars.iter().enumerate() {
        let next_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
        let prev_lower = i > 0 && chars[i - 1].is_lowercase();
        if c.is_uppercase() && !word.is_empty() && (prev_lower || next_lower) {
            words.push(std::mem::take(&mut word));
        }
        word.push(*c);
    }
    if !word.is_empty() {
        words.push(word);
    }

    words
        .iter()
        .enumerate()
        .map(|(i, w)| {
            let acronym = w.chars().filter(|c| c.is_uppercase()).count() > 1;
            match SPELLINGS.iter().find(|(word, _)| word == w) {
                Some((_, spelled)) => spelled.to_string(),
                None if i == 0 || acronym => w.clone(),
                None => w.to_lowercase(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn attribute(cluster: &ClusterModel, id: u32) -> Option<&AttributeModel> {
    cluster.attributes.iter().find(|a| a.id == id)
}

fn named_attribute<'c>(cluster: &'c ClusterModel, name: &str) -> Option<&'c AttributeModel> {
    cluster
        .attributes
        .iter()
        .find(|a| a.name.as_deref() == Some(name))
}

fn json(attr: Option<&AttributeModel>) -> Option<serde_json::Value> {
    serde_json::from_str(attr?.value.as_deref()?).ok()
}

fn number(attr: Option<&AttributeModel>) -> Option<f64> {
    json(attr)?.as_f64()
}

fn string(attr: Option<&AttributeModel>) -> Option<String> {
    json(attr)?.as_str().map(str::to_string)
}

fn is_on(attr: Option<&AttributeModel>) -> bool {
    match json(attr) {
        Some(serde_json::Value::Bool(b)) => b,
        Some(serde_json::Value::Number(n)) => n.as_f64().is_some_and(|v| v != 0.0),
        _ => false,
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

#[cfg(test)]
mod tests {
    use super::words;

    #[test]
    fn spec_names_read_as_words() {
        assert_eq!(
            words("OccupiedCoolingSetpoint"),
            "Occupied cooling setpoint"
        );
        assert_eq!(words("COState"), "CO state");
        assert_eq!(words("RvcRunMode"), "RVC run mode");
        assert_eq!(words("Pm25"), "PM2.5");
        assert_eq!(words("CarbonDioxide"), "Carbon dioxide");
        assert_eq!(words("GoHome"), "Go home");
    }
}
