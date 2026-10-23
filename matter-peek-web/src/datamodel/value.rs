//! How a value shows in the data model popup, by its kind: booleans, numbers
//! in their units, enum names, bits, bytes, lists and structs as tables, and
//! the IDs a cluster lists by name; and values as highlighted JSON.

use serde_json::{Map, Value};
use yew::prelude::*;

use matter_peek_core::api::{display, ClusterModel, DeviceModel, ValueKind, ValueType};

const DESCRIPTOR: u32 = 0x001D;
const SERVER_LIST: u32 = 0x0001;
const CLIENT_LIST: u32 = 0x0002;
const PARTS_LIST: u32 = 0x0003;

const GENERATED_COMMAND_LIST: u32 = 0xFFF8;
const ACCEPTED_COMMAND_LIST: u32 = 0xFFF9;
const ATTRIBUTE_LIST: u32 = 0xFFFB;

/// The Matter epoch, 2000-01-01, in Unix seconds.
const MATTER_EPOCH_UNIX_SECS: u64 = 946_684_800;

/// Bytes shown of a longer octet string.
const OCTETS_SHOWN: usize = 32;

/// Scalars a JSON list keeps on one line.
const JSON_INLINE_ITEMS: usize = 16;

/// Fields a list of structs shows as a table; one with more shows each
/// struct apart.
const TABLE_COLUMNS: usize = 4;

/// The value (JSON) of attribute `attr` of `cluster`.
pub(super) fn attribute(
    model: &DeviceModel,
    cluster: &ClusterModel,
    attr: u32,
    ty: Option<&ValueType>,
    json: &str,
) -> Html {
    let Ok(value) = serde_json::from_str::<Value>(json) else {
        return html! { <span class="v-text">{ json.to_string() }</span> };
    };

    match (cluster.id, attr) {
        (_, ATTRIBUTE_LIST) => ids(&value, model, |id| {
            cluster
                .attributes
                .iter()
                .find(|a| u64::from(a.id) == id)?
                .name
                .clone()
        }),
        (_, ACCEPTED_COMMAND_LIST) => ids(&value, model, |id| {
            cluster
                .commands
                .iter()
                .find(|c| u64::from(c.id) == id)?
                .name
                .clone()
        }),
        (_, GENERATED_COMMAND_LIST) => ids(&value, model, |_| None),
        (DESCRIPTOR, SERVER_LIST | CLIENT_LIST) => ids(&value, model, |id| cluster_name(model, id)),
        (DESCRIPTOR, PARTS_LIST) => match &value {
            Value::Array(parts) if !parts.is_empty() => html! {
                <span class="v-list">
                    { for parts.iter().map(|p| html! { <span class="v-chip is-endpoint">{ format!("EP {p}") }</span> }) }
                </span>
            },
            _ => view(&value, ty, model),
        },
        _ => view(&value, ty, model),
    }
}

/// `value` of `ty`.
pub(super) fn view(value: &Value, ty: Option<&ValueType>, model: &DeviceModel) -> Html {
    match value {
        Value::Null => html! { <span class="v-null">{ "null" }</span> },
        Value::Bool(b) => html! {
            <span class={classes!("v-bool", b.then_some("is-true"))}>
                <span class="v-bool-dot"></span>{ b.to_string() }
            </span>
        },
        Value::Number(n) => number(n, ty),
        Value::String(s) => match ty.map(|t| t.kind) {
            Some(ValueKind::Enum) => html! { <span class="v-enum">{ s.clone() }</span> },
            Some(ValueKind::Octets) => octets(s),
            Some(ValueKind::Text) => html! { <span class="v-text">{ format!("“{s}”") }</span> },
            _ => html! { <span class="v-text">{ s.clone() }</span> },
        },
        // A bitmap, as the names of its bits.
        Value::Array(bits) if ty.is_some_and(|t| !t.list) => {
            if bits.is_empty() {
                html! { <span class="v-none">{ "none" }</span> }
            } else {
                html! {
                    <span class="v-list">
                        { for bits.iter().map(|b| html! {
                            <span class="v-chip is-bit">{ b.as_str().map_or_else(|| b.to_string(), str::to_string) }</span>
                        }) }
                    </span>
                }
            }
        }
        Value::Array(items) => {
            let item = ty.map(|t| ValueType {
                list: false,
                ..t.clone()
            });
            list(items, item.as_ref(), model)
        }
        Value::Object(fields) => structure(fields, ty, model),
    }
}

/// An object of `fields`, in their order, as indented JSON, its parts
/// coloured.
pub(super) fn json(fields: &[(String, Value)]) -> Html {
    html! {
        <pre class="dm-json">
            { "{\n" }
            { for fields.iter().enumerate().map(|(i, (key, v))| html! {
                <>
                    { "  " }
                    <span class="j-key">{ Value::String(key.clone()).to_string() }</span>
                    { ": " }
                    { json_part(v, 1) }
                    { if i + 1 < fields.len() { ",\n" } else { "\n" } }
                </>
            }) }
            { "}" }
        </pre>
    }
}

/// An object of `fields`, in their order, as indented JSON text.
pub(super) fn json_text(fields: &[(String, Value)]) -> String {
    let fields: Vec<String> = fields
        .iter()
        .map(|(key, v)| {
            let v = serde_json::to_string_pretty(v).unwrap_or_default();
            format!("  {}: {}", Value::String(key.clone()), v.replace('\n', "\n  "))
        })
        .collect();

    format!("{{\n{}\n}}", fields.join(",\n"))
}

/// A number, in the unit its IDL type says, with its hex on hover; an ID in
/// hex, by name where known.
fn number(n: &serde_json::Number, ty: Option<&ValueType>) -> Html {
    let hex = n.as_u64().map(|v| format!("0x{v:X}"));
    let idl = ty.map(|t| t.idl.as_str());

    if let Some(id) = n.as_u64() {
        let named = match idl {
            Some("vendor_id") => Some((format!("0x{id:04X}"), display::vendor_name(id as u16))),
            Some("devtype_id") => Some((format!("0x{id:04X}"), display::device_type_name(id as u32))),
            Some("cluster_id" | "attrib_id" | "command_id" | "event_id" | "field_id") => {
                Some((format!("0x{id:04X}"), None))
            }
            Some("node_id" | "fabric_id") => Some((display::hex_u64(id), None)),
            _ => None,
        };
        match named {
            Some((hex, Some(name))) => {
                return html! {
                    <span class="v-chip is-id" title={id.to_string()}>
                        { name }<span class="v-chip-id">{ hex }</span>
                    </span>
                };
            }
            Some((hex, None)) => {
                return html! {
                    <>
                        <span class="v-num">{ hex }</span>
                        <span class="v-raw">{ id }</span>
                    </>
                };
            }
            None => {}
        }
    }

    let v = n.as_f64().unwrap_or_default();
    let shown = match idl {
        Some("temperature") => Some(format!("{:.2} °C", v / 100.0)),
        Some("percent") => Some(format!("{v} %")),
        Some("percent100ths") => Some(format!("{:.2} %", v / 100.0)),
        Some("elapsed_s") => n.as_u64().map(display::duration),
        Some("epoch_s") => n
            .as_u64()
            .map(|s| display::utc_label((s + MATTER_EPOCH_UNIX_SECS) * 1000)),
        _ => None,
    };

    match shown {
        Some(shown) => html! {
            <>
                <span class="v-num" title={hex}>{ shown }</span>
                <span class="v-raw">{ n.to_string() }</span>
            </>
        },
        None => html! { <span class="v-num" title={hex}>{ n.to_string() }</span> },
    }
}

/// An octet string (hex), its bytes apart, a long one cut short.
fn octets(hex: &str) -> Html {
    if hex.is_empty() {
        return html! { <span class="v-none">{ "empty" }</span> };
    }
    let bytes: Vec<&str> = hex
        .as_bytes()
        .chunks(2)
        .filter_map(|b| std::str::from_utf8(b).ok())
        .collect();
    let mut shown = bytes[..bytes.len().min(OCTETS_SHOWN)].join(" ");
    if bytes.len() > OCTETS_SHOWN {
        shown.push_str(" …");
    }

    html! {
        <>
            <span class="v-octets" title={hex.to_string()}>{ shown }</span>
            <span class="v-raw">{ format!("{} B", bytes.len()) }</span>
        </>
    }
}

/// IDs, each by its name where `name` knows it.
fn ids(value: &Value, model: &DeviceModel, name: impl Fn(u64) -> Option<String>) -> Html {
    let Value::Array(ids) = value else {
        return view(value, None, model);
    };
    if ids.is_empty() {
        return html! { <span class="v-none">{ "none" }</span> };
    }

    html! {
        <span class="v-list">
            { for ids.iter().map(|id| {
                let n = id.as_u64();
                let hex = n.map_or_else(|| id.to_string(), |n| format!("0x{n:04X}"));
                match n.and_then(&name) {
                    Some(name) => html! {
                        <span class="v-chip is-id" title={hex.clone()}>
                            { name }<span class="v-chip-id">{ hex }</span>
                        </span>
                    },
                    None => html! { <span class="v-chip is-id is-unnamed">{ hex }</span> },
                }
            }) }
        </span>
    }
}

/// A list: its items in a row, or structs as a table.
fn list(items: &[Value], item: Option<&ValueType>, model: &DeviceModel) -> Html {
    if items.is_empty() {
        return html! { <span class="v-none">{ "empty" }</span> };
    }

    let objects = items.iter().all(Value::is_object);
    if objects && columns(items, item, model).len() > TABLE_COLUMNS {
        return html! {
            <div class="v-records">
                { for items.iter().map(|i| html! {
                    <div class="v-record">{ view(i, item, model) }</div>
                }) }
            </div>
        };
    }
    if objects {
        let columns = columns(items, item, model);

        return html! {
            <div class="v-table-wrap">
                <table class="v-table">
                    <thead>
                        <tr>{ for columns.iter().map(|c| html! { <th>{ c.clone() }</th> }) }</tr>
                    </thead>
                    <tbody>
                        { for items.iter().map(|i| html! {
                            <tr>
                                { for columns.iter().map(|c| html! {
                                    <td>{ match i.get(c) {
                                        Some(v) => field(c, v, item, model),
                                        None => html! {},
                                    } }</td>
                                }) }
                            </tr>
                        }) }
                    </tbody>
                </table>
            </div>
        };
    }

    html! {
        <span class="v-list">
            if items.len() > 3 {
                <span class="v-count">{ items.len() }</span>
            }
            { for items.iter().map(|i| html! { <span class="v-item">{ view(i, item, model) }</span> }) }
        </span>
    }
}

/// The columns of a table of structs: the struct's fields, else the keys as
/// they come; those that some struct has.
fn columns(items: &[Value], item: Option<&ValueType>, model: &DeviceModel) -> Vec<String> {
    let mut columns: Vec<String> = item
        .and_then(|t| info_fields(t, model))
        .map(|fields| fields.iter().map(|f| f.name.clone()).collect())
        .unwrap_or_default();
    for item in items {
        for key in item.as_object().into_iter().flat_map(Map::keys) {
            if !columns.contains(key) {
                columns.push(key.clone());
            }
        }
    }
    columns.retain(|c| items.iter().any(|i| i.get(c).is_some()));

    columns
}

/// A struct: its fields and their values, in the order the struct has them.
fn structure(fields: &Map<String, Value>, ty: Option<&ValueType>, model: &DeviceModel) -> Html {
    let mut names: Vec<&String> = fields.keys().collect();
    if let Some(order) = ty.and_then(|t| info_fields(t, model)) {
        names.sort_by_key(|n| order.iter().position(|f| f.name == **n).unwrap_or(usize::MAX));
    }

    html! {
        <dl class="v-struct">
            { for names.into_iter().map(|name| (name, &fields[name])).map(|(name, v)| html! {
                <>
                    <dt>{ name.clone() }</dt>
                    <dd>{ field(name, v, ty, model) }</dd>
                </>
            }) }
        </dl>
    }
}

/// Field `name` of a struct of type `of`, as its type says.
fn field(name: &str, value: &Value, of: Option<&ValueType>, model: &DeviceModel) -> Html {
    let ty = of
        .and_then(|t| info_fields(t, model))
        .and_then(|fields| fields.iter().find(|f| f.name == name))
        .map(|f| &f.value_type);
    view(value, ty, model)
}

/// The fields of the struct `ty` is.
fn info_fields<'m>(
    ty: &ValueType,
    model: &'m DeviceModel,
) -> Option<&'m [matter_peek_core::api::FieldType]> {
    let info = model.types.get(ty.schema? as usize)?;
    Some(info.fields.as_slice())
}

/// The name of cluster `id`, as an endpoint of the device has it.
fn cluster_name(model: &DeviceModel, id: u64) -> Option<String> {
    model
        .endpoints
        .iter()
        .flat_map(|e| &e.clusters)
        .find(|c| u64::from(c.id) == id)?
        .name
        .clone()
}

fn json_part(value: &Value, depth: usize) -> Html {
    let pad = |depth: usize| "  ".repeat(depth);

    match value {
        Value::Null => html! { <span class="j-null">{ "null" }</span> },
        Value::Bool(b) => html! { <span class="j-bool">{ b.to_string() }</span> },
        Value::Number(n) => html! { <span class="j-num">{ n.to_string() }</span> },
        Value::String(_) => html! { <span class="j-str">{ value.to_string() }</span> },
        Value::Array(items) if items.is_empty() => html! { "[]" },
        Value::Array(items)
            if items.len() <= JSON_INLINE_ITEMS
                && items.iter().all(|i| !i.is_array() && !i.is_object()) =>
        {
            html! {
                <>
                    { "[" }
                    { for items.iter().enumerate().map(|(i, item)| html! {
                        <>
                            if i > 0 { { ", " } }
                            { json_part(item, depth) }
                        </>
                    }) }
                    { "]" }
                </>
            }
        }
        Value::Array(items) => html! {
            <>
                { "[\n" }
                { for items.iter().enumerate().map(|(i, item)| html! {
                    <>
                        { pad(depth + 1) }
                        { json_part(item, depth + 1) }
                        { if i + 1 < items.len() { ",\n" } else { "\n" } }
                    </>
                }) }
                { pad(depth) }{ "]" }
            </>
        },
        Value::Object(fields) if fields.is_empty() => html! { "{}" },
        Value::Object(fields) => html! {
            <>
                { "{\n" }
                { for fields.iter().enumerate().map(|(i, (key, v))| html! {
                    <>
                        { pad(depth + 1) }
                        <span class="j-key">{ Value::String(key.clone()).to_string() }</span>
                        { ": " }
                        { json_part(v, depth + 1) }
                        { if i + 1 < fields.len() { ",\n" } else { "\n" } }
                    </>
                }) }
                { pad(depth) }{ "}" }
            </>
        },
    }
}
