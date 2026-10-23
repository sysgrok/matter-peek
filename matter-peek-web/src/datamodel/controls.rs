//! The controls of an endpoint, for people to act on a device: switches,
//! sliders, choices, buttons and readings.
//!
//! Where they come from, cluster by cluster: curated ones for the clusters
//! people act on most (lights and their colours, plugs, thermostats, fans,
//! covers, locks, closures, valves, pumps, chargers, media); one per shape
//! for the clusters that share one (modes, operational states, measurements,
//! filters and tanks); for any other cluster of an application endpoint, ones
//! derived from its schema.

use serde_json::{json, Value};
use wasm_bindgen::JsCast;
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
const VALVE: u32 = 0x0081;
const ENERGY_EVSE: u32 = 0x0099;
const DOOR_LOCK: u32 = 0x0101;
const CLOSURE_CONTROL: u32 = 0x0104;
const CLOSURE_DIMENSION: u32 = 0x0105;
const PUMP: u32 = 0x0200;
const COLOR_CONTROL: u32 = 0x0300;
const WAKE_ON_LAN: u32 = 0x0503;
const CHANNEL: u32 = 0x0504;
const TARGET_NAVIGATOR: u32 = 0x0505;
const MEDIA_PLAYBACK: u32 = 0x0506;
const MEDIA_INPUT: u32 = 0x0507;
const LOW_POWER: u32 = 0x0508;
const KEYPAD_INPUT: u32 = 0x0509;
const AUDIO_OUTPUT: u32 = 0x050B;

/// The global attribute of a cluster's features.
const FEATURE_MAP: u32 = 0xFFFC;

/// The colour modes of Color Control.
const COLOR_MODE: u32 = 0x0008;
const ENHANCED_COLOR_MODE: u32 = 0x4001;

/// The lowest current an EV charger offers, mA.
const EVSE_MIN_CURRENT_MA: f64 = 6000.0;

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

/// The transport commands of media playback, in the order shown: their IDs,
/// what their buttons show, and names.
const TRANSPORT: &[(u32, &str, &str)] = &[
    (0x04, "⏮", "Previous"),
    (0x06, "⏪", "Rewind"),
    (0x00, "▶", "Play"),
    (0x01, "⏸", "Pause"),
    (0x02, "⏹", "Stop"),
    (0x07, "⏩", "Fast forward"),
    (0x05, "⏭", "Next"),
];

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
    /// On (`on`) or off, its state read as `state`, turned with the commands
    /// (and fields) `turn_on` and `turn_off`.
    Flip {
        on: bool,
        state: String,
        turn_on: (u32, String),
        turn_off: (u32, String),
    },
    /// A number between `min` and `max` (raw), shown divided by `scale` in
    /// `unit`, as attribute `attr` (its field `from`, if a struct) reads it,
    /// set as `set` says.
    Slider {
        attr: u32,
        from: Option<&'static str>,
        min: f64,
        max: f64,
        step: f64,
        scale: f64,
        unit: &'static str,
        set: Set,
    },
    /// A colour temperature, in mireds as `attr` reads it, between those
    /// given, shown and picked in kelvin.
    Kelvin {
        attr: u32,
        min_mireds: f64,
        max_mireds: f64,
    },
    /// A writable number, typed in.
    Number { attr: u32 },
    /// A writable enum, among its values.
    Choice { attr: u32 },
    /// One of the entries list attribute `list` has, each shown by its field
    /// `label`, the one `current` reads (by the entry's `keys`) picked; a pick
    /// invokes `cmd`, with the entry's `keys` as its `fields`.
    Pick {
        list: u32,
        label: &'static str,
        keys: &'static [&'static str],
        current: u32,
        cmd: u32,
        fields: &'static [&'static str],
    },
    /// The colour of a light, picked as `picker` says.
    Color { picker: Picker },
    /// Commands.
    Buttons { buttons: Vec<Button> },
    /// Commands taking the PIN typed in.
    Pin { buttons: Vec<Button> },
    /// The keys of a remote, sent with command `cmd`.
    Keypad { cmd: u32 },
    /// A value to read.
    Reading { attr: u32, unit: Unit },
}

/// A command, as a button: what it shows, and its fields (JSON).
struct Button {
    cmd: u32,
    label: String,
    title: Option<String>,
    fields: String,
}

/// How a colour is picked.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Picker {
    /// Not: it is shown only.
    None,
    /// As CIE 1931 x and y.
    Xy,
    HueSaturation,
}

/// How a slider sets its value.
enum Set {
    /// By writing its attribute.
    Attr,
    /// As `field` of command `cmd`, its other fields as `others` (JSON).
    Command {
        cmd: u32,
        field: &'static str,
        others: String,
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
    /// A field of a struct, as it is.
    Part(&'static str),
    /// A playback's position, of its duration.
    Playback,
    /// A TV channel: its numbers and name.
    Channel,
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
                from: None,
                min: number(attribute(cluster, 0x0002)).unwrap_or(1.0),
                max: number(attribute(cluster, 0x0003)).unwrap_or(254.0),
                step: 1.0,
                scale: 1.0,
                unit: "",
                set: Set::Command {
                    cmd: 0x04,
                    field: "Level",
                    others: r#"{"TransitionTime":null,"OptionsMask":[],"OptionsOverride":[]}"#
                        .into(),
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
                    from: None,
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
                buttons(vec![(0x00, "Open"), (0x02, "Stop"), (0x01, "Close")]),
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
        COLOR_CONTROL => color(cluster),
        DOOR_LOCK => door_lock(cluster),
        CLOSURE_CONTROL => closure(cluster),
        CLOSURE_DIMENSION => closure_dimension(cluster),
        VALVE => valve(cluster),
        PUMP => vec![
            control("Pump mode", Kind::Choice { attr: 0x0020 }),
            control("Pump control", Kind::Choice { attr: 0x0021 }),
            reading(0x0011, "Pump running as", Unit::Plain),
            reading(0x0012, "Pump controlled by", Unit::Plain),
            reading(0x0013, "Pump capacity", Unit::Scaled(200.0, "%")),
            reading(0x0014, "Pump speed", Unit::Scaled(1.0, "rpm")),
            reading(0x0016, "Pump power", Unit::Scaled(1.0, "W")),
            reading(0x0010, "Pump status", Unit::Plain),
        ],
        ENERGY_EVSE => evse(cluster),
        MEDIA_PLAYBACK => vec![
            reading(0x0000, "Playback", Unit::Plain),
            control(
                "Transport",
                Kind::Buttons {
                    buttons: TRANSPORT
                        .iter()
                        .map(|(cmd, label, title)| Button {
                            cmd: *cmd,
                            label: label.to_string(),
                            title: Some(title.to_string()),
                            fields: "{}".into(),
                        })
                        .collect(),
                },
            ),
            reading(0x0003, "Position", Unit::Playback),
        ],
        KEYPAD_INPUT => vec![control("Remote", Kind::Keypad { cmd: 0x00 })],
        MEDIA_INPUT => vec![pick("Input", "Name", &["Index"], 0x00, &["Index"])],
        AUDIO_OUTPUT => vec![pick("Output", "Name", &["Index"], 0x00, &["Index"])],
        TARGET_NAVIGATOR => vec![pick("Target", "Name", &["Identifier"], 0x00, &["Target"])],
        CHANNEL => vec![
            reading(0x0002, "Channel", Unit::Channel),
            control(
                "Zap",
                Kind::Buttons {
                    buttons: vec![
                        button(0x03, "−", r#"{"Count":-1}"#),
                        button(0x03, "+", r#"{"Count":1}"#),
                    ],
                },
            ),
            control(
                "Go to",
                Kind::Pick {
                    list: 0x0000,
                    label: "Name",
                    keys: &["MajorNumber", "MinorNumber"],
                    current: 0x0002,
                    cmd: 0x02,
                    fields: &["MajorNumber", "MinorNumber"],
                },
            ),
        ],
        LOW_POWER => vec![control("Power", buttons(vec![(0x00, "Sleep")]))],
        WAKE_ON_LAN => vec![reading(0x0000, "MAC address", Unit::Plain)],
        _ => return None,
    };

    Some(present(cluster, controls))
}

/// A light's colour: as its colour mode reads it, picked as x and y or as hue
/// and saturation, and its white as a colour temperature, as its features
/// say. Picked also while the light is off, which keeps it for when on.
fn color(cluster: &ClusterModel) -> Vec<Control> {
    let features = features(cluster);
    let has = |feature: &str| features.iter().any(|f| f == feature);
    let picker = if has("XY") {
        Picker::Xy
    } else if has("HueAndSaturation") {
        Picker::HueSaturation
    } else {
        Picker::None
    };
    let others = |extra: Value| {
        let mut fields = json!({
            "TransitionTime": 0,
            "OptionsMask": ["ExecuteIfOff"],
            "OptionsOverride": ["ExecuteIfOff"],
        });
        if let (Some(fields), Value::Object(extra)) = (fields.as_object_mut(), extra) {
            fields.extend(extra);
        }
        fields.to_string()
    };

    let mut controls = vec![control("Colour", Kind::Color { picker })];
    if has("HueAndSaturation") {
        controls.push(control(
            "Hue",
            Kind::Slider {
                attr: 0x0000,
                from: None,
                min: 0.0,
                max: 254.0,
                step: 1.0,
                scale: 254.0 / 360.0,
                unit: "°",
                set: Set::Command {
                    cmd: 0x00,
                    field: "Hue",
                    others: others(json!({ "Direction": "Shortest" })),
                },
            },
        ));
        controls.push(control(
            "Saturation",
            Kind::Slider {
                attr: 0x0001,
                from: None,
                min: 0.0,
                max: 254.0,
                step: 1.0,
                scale: 2.54,
                unit: "%",
                set: Set::Command {
                    cmd: 0x03,
                    field: "Saturation",
                    others: others(json!({})),
                },
            },
        ));
    }
    if has("ColorTemperature") {
        controls.push(control(
            "White",
            Kind::Kelvin {
                attr: 0x0007,
                min_mireds: number(attribute(cluster, 0x400B)).unwrap_or(153.0),
                max_mireds: number(attribute(cluster, 0x400C)).unwrap_or(500.0),
            },
        ));
    }

    controls
}

/// A lock: locked or not, as its lock state reads it; with the PIN typed in
/// where it needs one remotely; and its door, if it senses it.
fn door_lock(cluster: &ClusterModel) -> Vec<Control> {
    let state = string(attribute(cluster, 0x0000));
    let needs_pin = is_on(attribute(cluster, 0x0033));

    let mut controls = vec![if needs_pin {
        control(
            "Lock",
            Kind::Pin {
                buttons: vec![
                    button(0x00, "Lock", "{}"),
                    button(0x01, "Unlock", "{}"),
                    button(0x27, "Unbolt", "{}"),
                ],
            },
        )
    } else {
        control(
            "Lock",
            Kind::Flip {
                on: state.as_deref() == Some("Locked"),
                state: state.as_deref().map_or_else(|| "Unknown".into(), words),
                turn_on: (0x00, "{}".into()),
                turn_off: (0x01, "{}".into()),
            },
        )
    }];
    if needs_pin {
        controls.push(control(
            "Lock state",
            Kind::Reading {
                attr: 0x0000,
                unit: Unit::Plain,
            },
        ));
    } else if cluster.commands.iter().any(|c| c.id == 0x27) {
        controls.push(control("Latch", buttons(vec![(0x27, "Unbolt")])));
    }
    controls.push(control(
        "Door",
        Kind::Reading {
            attr: 0x0003,
            unit: Unit::Plain,
        },
    ));

    controls
}

/// A closure (a gate, a garage door, a window): its state and position,
/// moved fully open or closed or to the positions its features add, and
/// latched where it can be.
fn closure(cluster: &ClusterModel) -> Vec<Control> {
    let features = features(cluster);
    let has = |feature: &str| features.iter().any(|f| f == feature);
    let current = json(attribute(cluster, 0x0003));
    let latch = current
        .as_ref()
        .and_then(|c| c.get("Latch"))
        .and_then(Value::as_bool);
    let move_to = |position: &str| json!({ "Position": position }).to_string();

    let mut moves = vec![
        button(0x01, "Open", &move_to("MoveToFullyOpen")),
        button(0x00, "Stop", "{}"),
        button(0x01, "Close", &move_to("MoveToFullyClosed")),
    ];
    if has("Pedestrian") {
        moves.push(button(
            0x01,
            "Pedestrian",
            &move_to("MoveToPedestrianPosition"),
        ));
    }
    if has("Ventilation") {
        moves.push(button(
            0x01,
            "Ventilation",
            &move_to("MoveToVentilationPosition"),
        ));
    }

    let mut controls = vec![
        control(
            "Closure state",
            Kind::Reading {
                attr: 0x0001,
                unit: Unit::Plain,
            },
        ),
        control(
            "Closure position",
            Kind::Reading {
                attr: 0x0003,
                unit: Unit::Part("Position"),
            },
        ),
        control("Move", Kind::Buttons { buttons: moves }),
    ];
    if let Some(latched) = latch {
        controls.push(latch_control(latched, 0x01));
    }
    controls.push(control(
        "Closure time left",
        Kind::Reading {
            attr: 0x0000,
            unit: Unit::Duration,
        },
    ));

    controls
}

/// A dimension of a closure (a panel's position, a slat's angle): moved to a
/// position, or a step; latched where it can be.
fn closure_dimension(cluster: &ClusterModel) -> Vec<Control> {
    let current = json(attribute(cluster, 0x0000));
    let latch = current
        .as_ref()
        .and_then(|c| c.get("Latch"))
        .and_then(Value::as_bool);
    let step = number(attribute(cluster, 0x0003))
        .filter(|s| *s > 0.0)
        .unwrap_or(100.0);
    let steps = |direction: &str| json!({ "Direction": direction, "NumberOfSteps": 1 }).to_string();

    let mut controls = vec![
        control(
            "Position",
            Kind::Slider {
                attr: 0x0000,
                from: Some("Position"),
                min: 0.0,
                max: 10000.0,
                step,
                scale: 100.0,
                unit: "% closed",
                set: Set::Command {
                    cmd: 0x00,
                    field: "Position",
                    others: "{}".into(),
                },
            },
        ),
        control(
            "Step",
            Kind::Buttons {
                buttons: vec![
                    button(0x01, "Open a step", &steps("Decrease")),
                    button(0x01, "Close a step", &steps("Increase")),
                ],
            },
        ),
    ];
    if let Some(latched) = latch {
        controls.push(latch_control(latched, 0x00));
    }

    controls
}

/// A latch, latched or not, set with command `cmd`'s `Latch` field.
fn latch_control(latched: bool, cmd: u32) -> Control {
    control(
        "Latch",
        Kind::Flip {
            on: latched,
            state: if latched { "Latched" } else { "Unlatched" }.into(),
            turn_on: (cmd, r#"{"Latch":true}"#.into()),
            turn_off: (cmd, r#"{"Latch":false}"#.into()),
        },
    )
}

/// A valve: open or closed, at a level where it has one; how long it stays
/// open, and its faults.
fn valve(cluster: &ClusterModel) -> Vec<Control> {
    let state = string(attribute(cluster, 0x0004));
    let target = string(attribute(cluster, 0x0005));
    let step = number(attribute(cluster, 0x000A))
        .filter(|s| *s > 0.0)
        .unwrap_or(1.0);

    vec![
        control(
            "Valve",
            Kind::Flip {
                on: target.as_deref().or(state.as_deref()) == Some("Open"),
                state: state.as_deref().map_or_else(|| "Unknown".into(), words),
                turn_on: (0x00, "{}".into()),
                turn_off: (0x01, "{}".into()),
            },
        ),
        control(
            "Valve level",
            Kind::Slider {
                attr: 0x0006,
                from: None,
                min: 0.0,
                max: 100.0,
                step,
                scale: 1.0,
                unit: "%",
                set: Set::Command {
                    cmd: 0x00,
                    field: "TargetLevel",
                    others: "{}".into(),
                },
            },
        ),
        control(
            "Valve time left",
            Kind::Reading {
                attr: 0x0003,
                unit: Unit::Duration,
            },
        ),
        control(
            "Valve fault",
            Kind::Reading {
                attr: 0x0009,
                unit: Unit::Plain,
            },
        ),
    ]
}

/// An EV charger: its state, charging enabled or not, up to a current
/// between the lowest it offers and its circuit's capacity; the session.
fn evse(cluster: &ClusterModel) -> Vec<Control> {
    let supply = string(attribute(cluster, 0x0001));
    let lowest = number(attribute(cluster, 0x0006))
        .filter(|c| *c > 0.0)
        .unwrap_or(EVSE_MIN_CURRENT_MA);
    let capacity = number(attribute(cluster, 0x0005))
        .filter(|c| *c > lowest)
        .unwrap_or(32000.0);
    let most = number(attribute(cluster, 0x0007))
        .filter(|c| *c >= lowest)
        .unwrap_or(capacity);
    let enable = |most: f64| {
        json!({
            "ChargingEnabledUntil": null,
            "MinimumChargeCurrent": lowest as u64,
            "MaximumChargeCurrent": most as u64,
        })
        .to_string()
    };

    vec![
        control(
            "Charger state",
            Kind::Reading {
                attr: 0x0000,
                unit: Unit::Plain,
            },
        ),
        control(
            "Charging",
            Kind::Flip {
                on: matches!(supply.as_deref(), Some("ChargingEnabled" | "Enabled")),
                state: supply.as_deref().map_or_else(|| "Unknown".into(), words),
                turn_on: (0x02, enable(most)),
                turn_off: (0x01, "{}".into()),
            },
        ),
        control(
            "Charge at most",
            Kind::Slider {
                attr: 0x0007,
                from: None,
                min: lowest,
                max: capacity,
                step: 1000.0,
                scale: 1000.0,
                unit: "A",
                set: Set::Command {
                    cmd: 0x02,
                    field: "MaximumChargeCurrent",
                    others: json!({
                        "ChargingEnabledUntil": null,
                        "MinimumChargeCurrent": lowest as u64,
                    })
                    .to_string(),
                },
            },
        ),
        control(
            "Vehicle charge",
            Kind::Reading {
                attr: 0x0030,
                unit: Unit::Scaled(1.0, "%"),
            },
        ),
        control(
            "Session",
            Kind::Reading {
                attr: 0x0041,
                unit: Unit::Duration,
            },
        ),
        control(
            "Session energy",
            Kind::Reading {
                attr: 0x0042,
                unit: Unit::Scaled(1e6, "kWh"),
            },
        ),
        control(
            "Charger fault",
            Kind::Reading {
                attr: 0x0002,
                unit: Unit::Plain,
            },
        ),
    ]
}

/// One of the entries of list attribute 0x0000, the current one as attribute
/// 0x0001 reads it, picked with command `cmd`.
fn pick(
    label: &str,
    shown: &'static str,
    keys: &'static [&'static str],
    cmd: u32,
    fields: &'static [&'static str],
) -> Control {
    control(
        label,
        Kind::Pick {
            list: 0x0000,
            label: shown,
            keys,
            current: 0x0001,
            cmd,
            fields,
        },
    )
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
            from: None,
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
            Kind::Pick {
                list: supported,
                label: "Label",
                keys: &["Mode"],
                current,
                cmd: change.id,
                fields: &["NewMode"],
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
            .filter_map(|op| cmd(op).map(|c| button(c.id, &words(op), "{}")))
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
            controls.push(control(
                &what("Operation"),
                Kind::Buttons { buttons: commands },
            ));
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
        controls.push(control("Replaced", buttons(vec![(reset.id, "Reset")])));
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

    let commands: Vec<Button> = cluster
        .commands
        .iter()
        .filter(|c| c.fields.iter().all(|f| f.value_type.optional))
        .filter_map(|c| Some(button(c.id, &words(c.name.as_deref()?), "{}")))
        .collect();
    if !commands.is_empty() {
        controls.push(control("Commands", Kind::Buttons { buttons: commands }));
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
            switch(
                state,
                if state { "On" } else { "Off" },
                busy,
                on_act.reform(move |_| Action::Invoke {
                    endpoint,
                    cluster: cluster_id,
                    command: cmd,
                    fields: "{}".into(),
                }),
            )
        }
        Kind::Toggle { attr } => {
            let state = is_on(attribute(cluster, *attr));
            let attr = *attr;
            switch(
                state,
                if state { "Yes" } else { "No" },
                busy,
                on_act.reform(move |_| Action::Write {
                    endpoint,
                    cluster: cluster_id,
                    attribute: attr,
                    value: (!state).to_string(),
                }),
            )
        }
        Kind::Slider {
            attr,
            from,
            min,
            max,
            step,
            scale,
            unit,
            set,
        } => {
            let value = match from {
                Some(field) => json(attribute(cluster, *attr)).and_then(|v| v.get(field)?.as_f64()),
                None => number(attribute(cluster, *attr)),
            };
            let shown = value.map_or_else(|| "—".to_string(), |v| scaled(v, *scale, unit));
            let (attr, min) = (*attr, *min);
            let set = match set {
                Set::Attr => None,
                Set::Command { cmd, field, others } => Some((*cmd, *field, others.clone())),
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
                                Some((cmd, field, ref others)) => {
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
        Kind::Pick {
            list,
            label,
            keys,
            current,
            cmd,
            fields,
        } => {
            let entries = entries(attribute(cluster, *list));
            let now = json(attribute(cluster, *current));
            let selected = entries
                .iter()
                .position(|e| is_current(e, keys, now.as_ref()));
            let labels: Vec<String> = entries
                .iter()
                .map(|e| entry_label(e, label, keys))
                .collect();
            let (cmd, keys, fields) = (*cmd, *keys, *fields);
            html! {
                <div class="select is-small">
                    <select disabled={busy || entries.is_empty()}
                        onchange={on_act.reform(move |e: Event| {
                            let entry = select_value(e).parse::<usize>().ok().and_then(|i| entries.get(i).cloned());
                            let mut picked = serde_json::Map::new();
                            for (key, field) in keys.iter().zip(fields) {
                                if let Some(v) = entry.as_ref().and_then(|e| e.get(*key)) {
                                    picked.insert(field.to_string(), v.clone());
                                }
                            }
                            Action::Invoke { endpoint, cluster: cluster_id, command: cmd, fields: Value::Object(picked).to_string() }
                        })}>
                        if selected.is_none() {
                            <option selected=true disabled=true>{ "—" }</option>
                        }
                        { for labels.iter().enumerate().map(|(i, label)| html! {
                            <option value={i.to_string()} selected={Some(i) == selected}>{ label.clone() }</option>
                        }) }
                    </select>
                </div>
            }
        }
        Kind::Flip {
            on,
            state,
            turn_on,
            turn_off,
        } => {
            let (cmd, fields) = if *on {
                turn_off.clone()
            } else {
                turn_on.clone()
            };
            switch(
                *on,
                state,
                busy,
                on_act.reform(move |_| Action::Invoke {
                    endpoint,
                    cluster: cluster_id,
                    command: cmd,
                    fields: fields.clone(),
                }),
            )
        }
        Kind::Kelvin {
            attr,
            min_mireds,
            max_mireds,
        } => {
            let kelvin = number(attribute(cluster, *attr))
                .filter(|m| *m > 0.0)
                .map(|m| (1e6 / m).round());
            let (lowest, highest) = ((1e6 / max_mireds).round(), (1e6 / min_mireds).round());
            html! {
                <>
                    <input class="dm-slider is-kelvin" type="range" disabled={busy}
                        min={lowest.to_string()} max={highest.to_string()} step="50"
                        value={kelvin.unwrap_or(lowest).to_string()}
                        onchange={on_act.reform(move |e: Event| {
                            let kelvin = field_value(&e).parse::<f64>().unwrap_or(lowest).max(1.0);
                            Action::Invoke {
                                endpoint, cluster: cluster_id, command: 0x0A,
                                fields: json!({
                                    "ColorTemperatureMireds": (1e6 / kelvin).round() as u64,
                                    "TransitionTime": 0,
                                    "OptionsMask": ["ExecuteIfOff"],
                                    "OptionsOverride": ["ExecuteIfOff"],
                                }).to_string(),
                            }
                        })} />
                    <span class="dm-control-value">{ kelvin.map_or_else(|| "—".into(), |k| format!("{k} K")) }</span>
                </>
            }
        }
        Kind::Color { picker } => {
            let rgb = light_color(cluster);
            let hex = rgb.map_or_else(|| "#ffffff".into(), hex_color);
            let picker = *picker;
            html! {
                <div class="dm-color">
                    <span class={classes!("dm-swatch", rgb.is_none().then_some("is-unknown"))}
                        style={format!("background: {hex}")}></span>
                    if picker != Picker::None {
                        <input class="dm-color-input" type="color" value={hex.clone()} disabled={busy}
                            title="Pick a colour"
                            onchange={on_act.reform(move |e: Event| {
                                let rgb = parse_hex(&field_value(&e)).unwrap_or((1.0, 1.0, 1.0));
                                let (command, fields) = match picker {
                                    Picker::Xy => {
                                        let (x, y) = rgb_to_xy(rgb);
                                        (0x07, json!({ "ColorX": x, "ColorY": y }))
                                    }
                                    _ => {
                                        let (hue, saturation) = rgb_to_hue_saturation(rgb);
                                        (0x06, json!({ "Hue": hue, "Saturation": saturation }))
                                    }
                                };
                                let mut fields = fields;
                                fields["TransitionTime"] = json!(0);
                                fields["OptionsMask"] = json!(["ExecuteIfOff"]);
                                fields["OptionsOverride"] = json!(["ExecuteIfOff"]);
                                Action::Invoke { endpoint, cluster: cluster_id, command, fields: fields.to_string() }
                            })} />
                    }
                    <span class="dm-control-value is-color">{ color_text(cluster) }</span>
                </div>
            }
        }
        Kind::Pin { buttons } => html! {
            <div class="dm-pin">
                <input class="input is-small" type="password" inputmode="numeric" autocomplete="off"
                    placeholder="PIN" disabled={busy} />
                <div class="buttons are-small">
                    { for buttons.iter().filter(|b| accepts(cluster, b.cmd)).map(|b| {
                        let cmd = b.cmd;
                        html! {
                            <button class="button is-small is-soft" disabled={busy}
                                onclick={on_act.reform(move |e: MouseEvent| Action::Invoke {
                                    endpoint, cluster: cluster_id, command: cmd,
                                    fields: json!({ "PINCode": ascii_hex(&typed_pin(&e)) }).to_string(),
                                })}>
                                { b.label.clone() }
                            </button>
                        }
                    }) }
                </div>
            </div>
        },
        Kind::Keypad { cmd } => {
            let cmd = *cmd;
            let key = |code: &'static str, label: &'static str, class: Option<&'static str>| {
                html! {
                    <button class={classes!("button", "is-small", "is-soft", class)} title={code} disabled={busy}
                        onclick={on_act.reform(move |_| Action::Invoke {
                            endpoint, cluster: cluster_id, command: cmd,
                            fields: json!({ "KeyCode": code }).to_string(),
                        })}>
                        { label }
                    </button>
                }
            };
            html! {
                <div class="dm-keypad">
                    <div class="dm-dpad">
                        <span></span>{ key("Up", "▲", None) }<span></span>
                        { key("Left", "◀", None) }{ key("Select", "OK", Some("is-ok")) }{ key("Right", "▶", None) }
                        <span></span>{ key("Down", "▼", None) }<span></span>
                    </div>
                    <div class="buttons are-small">
                        { key("Exit", "Back", None) }
                        { key("RootMenu", "Menu", None) }
                        { key("VolumeDown", "Vol −", None) }
                        { key("Mute", "Mute", None) }
                        { key("VolumeUp", "Vol +", None) }
                    </div>
                </div>
            }
        }
        Kind::Buttons { buttons } => html! {
            <div class="buttons are-small">
                { for buttons.iter().filter(|b| accepts(cluster, b.cmd)).map(|b| {
                    let (cmd, fields) = (b.cmd, b.fields.clone());
                    html! {
                        <button class="button is-small is-soft" disabled={busy} title={b.title.clone()}
                            onclick={on_act.reform(move |_| Action::Invoke {
                                endpoint, cluster: cluster_id, command: cmd, fields: fields.clone(),
                            })}>
                            { b.label.clone() }
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

/// A two-state switch, on as `on` says, its state read as `label`; a click
/// turns it over.
fn switch(on: bool, label: &str, busy: bool, onclick: Callback<MouseEvent>) -> Html {
    html! {
        <button class={classes!("dm-switch", on.then_some("is-on"))} role="switch"
            aria-checked={on.to_string()} disabled={busy} {onclick}>
            <span class="dm-switch-track"><span class="dm-switch-knob"></span></span>
            <span class="dm-switch-label">{ label.to_string() }</span>
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
        (Unit::Part(field), serde_json::Value::Object(fields)) => match fields.get(field) {
            Some(serde_json::Value::String(s)) => words(s),
            Some(serde_json::Value::Null) | None => "—".into(),
            Some(other) => other.to_string(),
        },
        (Unit::Playback, serde_json::Value::Object(fields)) => {
            let at = fields.get("Position").and_then(serde_json::Value::as_u64);
            let of = number(attribute(cluster, 0x0002)).map(|d| d as u64);
            match (at, of) {
                (Some(at), Some(of)) => format!("{} / {}", clock(at), clock(of)),
                (Some(at), None) => clock(at),
                _ => "—".into(),
            }
        }
        (Unit::Channel, serde_json::Value::Object(fields)) => {
            let number = |k| {
                fields
                    .get(k)
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or_default()
            };
            let name = fields
                .get("Name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            format!("{}.{} {name}", number("MajorNumber"), number("MinorNumber"))
                .trim()
                .to_string()
        }
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

/// A playback position (ms) as `h:mm:ss`, or `m:ss`.
fn clock(ms: u64) -> String {
    let secs = ms / 1000;
    if secs >= 3600 {
        format!("{}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
    } else {
        format!("{}:{:02}", secs / 60, secs % 60)
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

/// `controls` without those whose attribute or commands the cluster lacks.
fn present(cluster: &ClusterModel, controls: Vec<Control>) -> Vec<Control> {
    controls
        .into_iter()
        .filter(|c| match &c.kind {
            Kind::Switch { attr, .. }
            | Kind::Toggle { attr }
            | Kind::Slider { attr, .. }
            | Kind::Kelvin { attr, .. }
            | Kind::Number { attr }
            | Kind::Choice { attr }
            | Kind::Reading { attr, .. } => attribute(cluster, *attr).is_some(),
            Kind::Pick { list, .. } => attribute(cluster, *list).is_some(),
            Kind::Color { .. } => attribute(cluster, COLOR_MODE).is_some(),
            Kind::Flip { turn_on, .. } => accepts(cluster, turn_on.0),
            Kind::Buttons { buttons } | Kind::Pin { buttons } => {
                buttons.iter().any(|b| accepts(cluster, b.cmd))
            }
            Kind::Keypad { cmd } => accepts(cluster, *cmd),
        })
        .collect()
}

/// Whether `cluster` accepts command `cmd`.
fn accepts(cluster: &ClusterModel, cmd: u32) -> bool {
    cluster.commands.iter().any(|c| c.id == cmd)
}

/// The features `cluster` has, by name.
fn features(cluster: &ClusterModel) -> Vec<String> {
    match json(attribute(cluster, FEATURE_MAP)) {
        Some(Value::Array(names)) => names
            .iter()
            .filter_map(|n| n.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

/// A button invoking `cmd` with `fields` (JSON).
fn button(cmd: u32, label: &str, fields: &str) -> Button {
    Button {
        cmd,
        label: label.to_string(),
        title: None,
        fields: fields.to_string(),
    }
}

/// Buttons invoking commands needing no fields.
fn buttons(commands: Vec<(u32, &str)>) -> Kind {
    Kind::Buttons {
        buttons: commands
            .into_iter()
            .map(|(cmd, label)| button(cmd, label, "{}"))
            .collect(),
    }
}

/// The entries of a list attribute.
fn entries(attr: Option<&AttributeModel>) -> Vec<Value> {
    match json(attr) {
        Some(Value::Array(entries)) => entries,
        _ => Vec::new(),
    }
}

/// Whether `entry` is the one `current` reads: by its first key, or by all
/// of them for a struct.
fn is_current(entry: &Value, keys: &[&str], current: Option<&Value>) -> bool {
    match current {
        Some(Value::Object(current)) => keys.iter().all(|k| entry.get(*k) == current.get(*k)),
        Some(Value::Null) | None => false,
        Some(value) => keys.first().and_then(|k| entry.get(*k)) == Some(value),
    }
}

/// What an entry shows: its field `label`, else its `keys`.
fn entry_label(entry: &Value, label: &str, keys: &[&str]) -> String {
    match entry.get(label).and_then(Value::as_str) {
        Some(label) => label.to_string(),
        None => keys
            .iter()
            .filter_map(|k| entry.get(*k).map(Value::to_string))
            .collect::<Vec<_>>()
            .join("."),
    }
}

/// The PIN typed in next to the button clicked.
fn typed_pin(e: &MouseEvent) -> String {
    e.target()
        .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
        .and_then(|button| button.closest(".dm-pin").ok().flatten())
        .and_then(|pin| pin.query_selector("input").ok().flatten())
        .and_then(|input| input.dyn_into::<web_sys::HtmlInputElement>().ok())
        .map(|input| input.value())
        .unwrap_or_default()
}

/// `text` as the hex of its ASCII bytes, as an octet string.
fn ascii_hex(text: &str) -> String {
    text.bytes().map(|b| format!("{b:02x}")).collect()
}

/// The colour a light shows, as its colour mode says: red, green and blue,
/// each from 0 to 1.
fn light_color(cluster: &ClusterModel) -> Option<(f64, f64, f64)> {
    let value = |id| number(attribute(cluster, id));
    let mode = string(attribute(cluster, ENHANCED_COLOR_MODE))
        .or_else(|| string(attribute(cluster, COLOR_MODE)))?;

    match mode.as_str() {
        "CurrentXAndCurrentY" => Some(xy_to_rgb(
            value(0x0003)? / 65536.0,
            value(0x0004)? / 65536.0,
        )),
        "ColorTemperatureMireds" => Some(kelvin_to_rgb(1e6 / value(0x0007).filter(|m| *m > 0.0)?)),
        "EnhancedCurrentHueAndCurrentSaturation" => Some(hsv_to_rgb(
            value(0x4000)? * 360.0 / 65536.0,
            value(0x0001)? / 254.0,
        )),
        _ => Some(hsv_to_rgb(
            value(0x0000)? * 360.0 / 254.0,
            value(0x0001)? / 254.0,
        )),
    }
}

/// A light's colour in words, as its colour mode says.
fn color_text(cluster: &ClusterModel) -> String {
    let value = |id| number(attribute(cluster, id));
    let mode = string(attribute(cluster, COLOR_MODE)).unwrap_or_default();

    let text = match mode.as_str() {
        "CurrentXAndCurrentY" => value(0x0003)
            .zip(value(0x0004))
            .map(|(x, y)| format!("x {:.3}, y {:.3}", x / 65536.0, y / 65536.0)),
        "ColorTemperatureMireds" => value(0x0007)
            .filter(|m| *m > 0.0)
            .map(|m| format!("{:.0} K", 1e6 / m)),
        _ => value(0x0000)
            .zip(value(0x0001))
            .map(|(h, s)| format!("{:.0}°, {:.0} %", h * 360.0 / 254.0, s / 2.54)),
    };
    text.unwrap_or_else(|| "—".into())
}

/// CIE 1931 x and y, at full brightness, as sRGB.
fn xy_to_rgb(x: f64, y: f64) -> (f64, f64, f64) {
    if y <= 0.0 {
        return (1.0, 1.0, 1.0);
    }
    let (big_x, big_y, big_z) = (x / y, 1.0, (1.0 - x - y) / y);
    let r = 3.2406 * big_x - 1.5372 * big_y - 0.4986 * big_z;
    let g = -0.9689 * big_x + 1.8758 * big_y + 0.0415 * big_z;
    let b = 0.0557 * big_x - 0.2040 * big_y + 1.0570 * big_z;
    let top = r.max(g).max(b).max(1e-9);
    let gamma = |v: f64| {
        let v = (v / top).clamp(0.0, 1.0);
        if v <= 0.003_130_8 {
            12.92 * v
        } else {
            1.055 * v.powf(1.0 / 2.4) - 0.055
        }
    };

    (gamma(r), gamma(g), gamma(b))
}

/// sRGB as CIE 1931 x and y, as Color Control has them (`/ 65536`).
fn rgb_to_xy((r, g, b): (f64, f64, f64)) -> (u64, u64) {
    let linear = |v: f64| {
        if v <= 0.040_45 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    let (r, g, b) = (linear(r), linear(g), linear(b));
    let big_x = 0.4124 * r + 0.3576 * g + 0.1805 * b;
    let big_y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    let big_z = 0.0193 * r + 0.1192 * g + 0.9505 * b;
    let sum = (big_x + big_y + big_z).max(1e-9);
    let at = |v: f64| ((v / sum) * 65536.0).round().clamp(0.0, 65279.0) as u64;

    (at(big_x), at(big_y))
}

/// sRGB as Color Control's hue and saturation (0 to 254).
fn rgb_to_hue_saturation((r, g, b): (f64, f64, f64)) -> (u64, u64) {
    let (top, bottom) = (r.max(g).max(b), r.min(g).min(b));
    let delta = top - bottom;
    let hue = if delta == 0.0 {
        0.0
    } else if top == r {
        60.0 * ((g - b) / delta).rem_euclid(6.0)
    } else if top == g {
        60.0 * ((b - r) / delta + 2.0)
    } else {
        60.0 * ((r - g) / delta + 4.0)
    };
    let saturation = if top == 0.0 { 0.0 } else { delta / top };

    (
        (hue * 254.0 / 360.0).round().min(254.0) as u64,
        (saturation * 254.0).round() as u64,
    )
}

/// A hue (degrees) and saturation (0 to 1), at full brightness, as sRGB.
fn hsv_to_rgb(hue: f64, saturation: f64) -> (f64, f64, f64) {
    let c = saturation.clamp(0.0, 1.0);
    let h = hue.rem_euclid(360.0) / 60.0;
    let x = c * (1.0 - (h % 2.0 - 1.0).abs());
    let (r, g, b) = match h as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = 1.0 - c;

    (r + m, g + m, b + m)
}

/// The colour of white light of `kelvin`, roughly, as sRGB.
fn kelvin_to_rgb(kelvin: f64) -> (f64, f64, f64) {
    let t = kelvin / 100.0;
    let r = if t <= 66.0 {
        255.0
    } else {
        329.698_727_446 * (t - 60.0).powf(-0.133_204_759_2)
    };
    let g = if t <= 66.0 {
        99.470_802_586_1 * t.ln() - 161.119_568_166_1
    } else {
        288.122_169_528_3 * (t - 60.0).powf(-0.075_514_849_2)
    };
    let b = if t >= 66.0 {
        255.0
    } else if t <= 19.0 {
        0.0
    } else {
        138.517_731_223_1 * (t - 10.0).ln() - 305.044_792_730_7
    };
    let unit = |v: f64| (v / 255.0).clamp(0.0, 1.0);

    (unit(r), unit(g), unit(b))
}

/// A colour as `#rrggbb`.
fn hex_color((r, g, b): (f64, f64, f64)) -> String {
    let byte = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}", byte(r), byte(g), byte(b))
}

/// `#rrggbb` as red, green and blue from 0 to 1.
fn parse_hex(hex: &str) -> Option<(f64, f64, f64)> {
    let hex = hex.strip_prefix('#')?;
    let byte = |i: usize| {
        u8::from_str_radix(hex.get(i..i + 2)?, 16)
            .ok()
            .map(|b| b as f64 / 255.0)
    };
    Some((byte(0)?, byte(2)?, byte(4)?))
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
    use matter_peek_core::api::{AttributeModel, ClusterModel, CommandModel};
    use serde_json::{json, Value};

    use super::*;

    /// A cluster with these attribute values (JSON) and accepted commands.
    fn cluster(id: u32, attrs: &[(u32, Value)], commands: &[u32]) -> ClusterModel {
        ClusterModel {
            id,
            name: None,
            attributes: attrs
                .iter()
                .map(|(id, value)| AttributeModel {
                    id: *id,
                    name: None,
                    writable: false,
                    timed: false,
                    value_type: None,
                    value: Some(value.to_string()),
                    error: None,
                })
                .collect(),
            commands: commands
                .iter()
                .map(|id| CommandModel {
                    id: *id,
                    name: None,
                    fields: Vec::new(),
                    response: None,
                    timed: false,
                })
                .collect(),
            events: Vec::new(),
        }
    }

    fn flip(controls: &[Control], label: &str) -> (bool, String, (u32, String), (u32, String)) {
        match &controls
            .iter()
            .find(|c| c.label == label)
            .expect(label)
            .kind
        {
            Kind::Flip {
                on,
                state,
                turn_on,
                turn_off,
            } => (*on, state.clone(), turn_on.clone(), turn_off.clone()),
            _ => panic!("{label} is no switch"),
        }
    }

    fn fields(json: &str) -> Value {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn a_lock_switches_with_lock_and_unlock() {
        let lock = cluster(
            DOOR_LOCK,
            &[(0x0000, json!("Locked")), (0x0003, json!("DoorClosed"))],
            &[0x00, 0x01],
        );
        let controls = curated(&lock).unwrap();

        let (on, state, turn_on, turn_off) = flip(&controls, "Lock");
        assert!(on);
        assert_eq!(state, "Locked");
        assert_eq!(turn_on.0, 0x00);
        assert_eq!(turn_off.0, 0x01);
        assert!(controls.iter().any(|c| c.label == "Door"));
        // No unbolt, as it does not accept it.
        assert!(!controls.iter().any(|c| c.label == "Latch"));
    }

    #[test]
    fn a_lock_needing_a_pin_takes_it_typed_in() {
        let lock = cluster(
            DOOR_LOCK,
            &[(0x0000, json!("Unlocked")), (0x0033, json!(true))],
            &[0x00, 0x01, 0x27],
        );
        let controls = curated(&lock).unwrap();

        assert!(matches!(controls[0].kind, Kind::Pin { .. }));
        assert_eq!(ascii_hex("1234"), "31323334");
    }

    #[test]
    fn a_charger_enables_charging_up_to_its_current_limit() {
        let evse = cluster(
            ENERGY_EVSE,
            &[
                (0x0000, json!("PluggedInDemand")),
                (0x0001, json!("Disabled")),
                (0x0005, json!(32000)),
                (0x0006, json!(6000)),
                (0x0007, json!(16000)),
            ],
            &[0x01, 0x02],
        );
        let controls = curated(&evse).unwrap();

        let (on, state, turn_on, turn_off) = flip(&controls, "Charging");
        assert!(!on);
        assert_eq!(state, "Disabled");
        assert_eq!(turn_on.0, 0x02);
        assert_eq!(
            fields(&turn_on.1),
            json!({ "ChargingEnabledUntil": null, "MinimumChargeCurrent": 6000, "MaximumChargeCurrent": 16000 })
        );
        assert_eq!(turn_off, (0x01, "{}".to_string()));

        let slider = controls
            .iter()
            .find(|c| c.label == "Charge at most")
            .unwrap();
        let Kind::Slider {
            min,
            max,
            set: Set::Command { cmd, field, others },
            ..
        } = &slider.kind
        else {
            panic!("no slider");
        };
        assert_eq!(
            (*min, *max, *cmd, *field),
            (6000.0, 32000.0, 0x02, "MaximumChargeCurrent")
        );
        assert_eq!(
            fields(others),
            json!({ "ChargingEnabledUntil": null, "MinimumChargeCurrent": 6000 })
        );
    }

    #[test]
    fn a_closure_moves_to_the_positions_its_features_add_and_latches() {
        let closure = cluster(
            CLOSURE_CONTROL,
            &[
                (0x0001, json!("Stopped")),
                (
                    0x0003,
                    json!({ "Position": "FullyClosed", "Latch": true, "SecureState": true }),
                ),
                (
                    FEATURE_MAP,
                    json!(["Positioning", "MotionLatching", "Pedestrian"]),
                ),
            ],
            &[0x00, 0x01],
        );
        let controls = curated(&closure).unwrap();

        let Kind::Buttons { buttons } = &controls.iter().find(|c| c.label == "Move").unwrap().kind
        else {
            panic!("no buttons");
        };
        let labels: Vec<&str> = buttons.iter().map(|b| b.label.as_str()).collect();
        assert_eq!(labels, ["Open", "Stop", "Close", "Pedestrian"]);
        assert_eq!(
            fields(&buttons[3].fields),
            json!({ "Position": "MoveToPedestrianPosition" })
        );

        let (on, state, turn_on, _) = flip(&controls, "Latch");
        assert!(on);
        assert_eq!(state, "Latched");
        assert_eq!(fields(&turn_on.1), json!({ "Latch": true }));
        assert_eq!(
            reading(&closure, 0x0003, Unit::Part("Position")),
            "Fully closed"
        );
    }

    #[test]
    fn a_valve_is_open_as_it_is_going() {
        let valve = cluster(
            VALVE,
            &[(0x0004, json!("Transitioning")), (0x0005, json!("Open"))],
            &[0x00, 0x01],
        );
        let controls = curated(&valve).unwrap();

        let (on, state, ..) = flip(&controls, "Valve");
        assert!(on);
        assert_eq!(state, "Transitioning");
        // No level, as it has none.
        assert!(!controls.iter().any(|c| c.label == "Valve level"));
    }

    #[test]
    fn entries_are_picked_by_their_keys() {
        let channel = json!({ "MajorNumber": 4, "MinorNumber": 1, "Name": "News" });
        assert!(is_current(
            &channel,
            &["MajorNumber", "MinorNumber"],
            Some(&json!({ "MajorNumber": 4, "MinorNumber": 1 }))
        ));
        assert!(!is_current(
            &channel,
            &["MajorNumber", "MinorNumber"],
            Some(&json!({ "MajorNumber": 4, "MinorNumber": 2 }))
        ));
        assert!(is_current(
            &json!({ "Index": 2 }),
            &["Index"],
            Some(&json!(2))
        ));
        assert!(!is_current(
            &json!({ "Index": 2 }),
            &["Index"],
            Some(&Value::Null)
        ));
        assert_eq!(
            entry_label(
                &json!({ "MajorNumber": 7, "MinorNumber": 2 }),
                "Name",
                &["MajorNumber", "MinorNumber"]
            ),
            "7.2"
        );
    }

    #[test]
    fn colours_convert() {
        // sRGB red, at its primary's chromaticity.
        let (x, y) = rgb_to_xy((1.0, 0.0, 0.0));
        assert!((x as f64 / 65536.0 - 0.64).abs() < 0.005, "{x}");
        assert!((y as f64 / 65536.0 - 0.33).abs() < 0.005, "{y}");
        // D65 is white.
        assert_eq!(hex_color(xy_to_rgb(0.3127, 0.3290)), "#ffffff");
        assert_eq!(rgb_to_hue_saturation((0.0, 1.0, 0.0)), (85, 254));
        assert_eq!(hex_color(hsv_to_rgb(0.0, 1.0)), "#ff0000");
        assert_eq!(
            parse_hex("#ff8000").map(hex_color).as_deref(),
            Some("#ff8000")
        );
        let warm = kelvin_to_rgb(2700.0);
        assert!(warm.0 > warm.2, "{warm:?}");
    }

    #[test]
    fn a_light_shows_its_colour_in_its_mode() {
        let light = cluster(
            COLOR_CONTROL,
            &[
                (COLOR_MODE, json!("ColorTemperatureMireds")),
                (0x0007, json!(370)),
            ],
            &[],
        );
        assert_eq!(color_text(&light), "2703 K");

        let light = cluster(
            COLOR_CONTROL,
            &[
                (COLOR_MODE, json!("CurrentHueAndCurrentSaturation")),
                (0x0000, json!(0)),
                (0x0001, json!(254)),
            ],
            &[],
        );
        assert_eq!(
            light_color(&light).map(hex_color).as_deref(),
            Some("#ff0000")
        );
    }

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
