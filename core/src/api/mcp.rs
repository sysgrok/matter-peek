//! The API as MCP (Model Context Protocol) tools, for AI agents: the handshake
//! (`initialize`), tool discovery (`tools/list`), and tool invocation
//! (`tools/call`), in [`jsonrpc`] messages.
//!
//! Transport- and controller-agnostic (`no_std` + `alloc`). A tool call maps to
//! a [`wire::Request`], which an injected [`Dispatch`] runs (the CLI's `mcp`
//! command does, over a connection to the API); the [`wire::Response`] comes
//! back as the tool result. Streamed progress is not surfaced (a tool call is
//! request/response), so neither are health subscriptions. The line reading and
//! writing is the shell's.
//!
//! **Safety:** read-only tools (discover / list / peek / logs / identify / QR
//! images / OTA images) are always offered. Mutating tools (commission /
//! decommission / remember / set_tags / sync / OTA upload, delete and update) are
//! offered only with [`Caps::allow_mutate`], so an agent cannot change devices
//! unless the human operator opted in.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::future::Future;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use serde_json::{json, Value};

use crate::api::{self, display, wire};

use jsonrpc::Response;

pub mod jsonrpc;

const SERVER_NAME: &str = "matter-peek";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
/// The MCP protocol revision we speak.
const PROTOCOL_VERSION: &str = "2024-11-05";

/// The tools that change devices or the stored state; see [`Caps`].
const MUTATING_TOOLS: &[&str] = &[
    "commission",
    "decommission",
    "remember",
    "set_tags",
    "sync",
    "ota_upload",
    "ota_delete",
    "ota_update",
];

/// How much of an OTA image goes in one upload request.
const UPLOAD_CHUNK_LEN: usize = 256 * 1024;

/// Runs one [`wire::Request`] against the controller and yields its single final
/// response. The protocol layer is agnostic to *how*: the CLI implements this
/// over a connection to the API, an embedded shell could over its own
/// controller.
pub trait Dispatch {
    /// `Err(message)` is for transport failures (becomes a JSON-RPC
    /// internal error); a controller-level failure is a normal
    /// [`wire::Response::Error`] carried in `Ok`.
    fn dispatch(
        &self,
        request: wire::Request,
    ) -> impl Future<Output = Result<wire::Response, String>>;

    /// Read a local file, for `ota_upload`. A shell without files keeps this.
    fn read_file(&self, path: &str) -> impl Future<Output = Result<Vec<u8>, String>> {
        let message = format!("cannot read {path}: no file access");

        async move { Err(message) }
    }
}

/// Whether the mutating tools ([`MUTATING_TOOLS`]) are exposed.
#[derive(Clone, Copy)]
pub struct Caps {
    pub allow_mutate: bool,
}

/// Dispatch one JSON-RPC request to its MCP handler, returning the response to
/// write back (or `None` for a notification, which gets no reply).
pub async fn handle<D: Dispatch>(
    req: jsonrpc::Request,
    dispatch: &D,
    caps: Caps,
) -> Option<Response> {
    // Notifications (no `id`) are acknowledged silently per JSON-RPC.
    let id = req.id.clone()?;

    let resp = match req.method.as_str() {
        "initialize" => Response::ok(id, initialize_result()),
        "tools/list" => Response::ok(id, json!({ "tools": tool_list(caps) })),
        "tools/call" => match tools_call(req.params, dispatch, caps).await {
            Ok(result) => Response::ok(id, result),
            Err((code, msg)) => Response::err(id, code, msg),
        },
        // `ping` is part of MCP; answer with an empty result.
        "ping" => Response::ok(id, json!({})),
        other => Response::err(
            id,
            jsonrpc::METHOD_NOT_FOUND,
            format!("unknown method: {other}"),
        ),
    };
    Some(resp)
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION },
        "instructions":
            "Vendor-agnostic Matter controller. Read-only tools \
             (discover, list_devices, peek_device, retrieve_logs, identify, \
             read_qr_image, ota_images) are always available. The tools that change devices \
             or the stored state (commission, decommission, remember, set_tags, \
             sync, ota_upload, ota_delete, ota_update) are available only if the \
             operator allowed them (e.g. `matter-peek mcp --allow-mutate`)."
    })
}

// ---------------------------------------------------------------------------
// Tool catalog
// ---------------------------------------------------------------------------

/// The tool catalog for `tools/list`, filtered by `caps`.
fn tool_list(caps: Caps) -> Vec<Value> {
    let mut tools = vec![
        tool(
            "discover",
            "Find the Matter devices in commissioning mode: on the local \
             network via mDNS (addresses + commissionable ids, for commission's \
             peer_addr) and over Bluetooth LE (addresses + discriminator, \
             vendor and product ids, for commission's ble_addr).",
            json!({
                "type": "object",
                "properties": {
                    "discriminator": {
                        "type": "integer",
                        "description": "Optional 12-bit discriminator to narrow to one device."
                    },
                    "setup_code": {
                        "type": "string",
                        "description": "Optional manual pairing code or MT: QR code payload to narrow to its device."
                    },
                    "max": { "type": "integer", "description": "Max devices to return (default 16)." }
                }
            }),
        ),
        tool(
            "list_devices",
            "List the devices on our fabric, with their identity, fingerprint, \
             tags, whether they are online and whether they are remembered. \
             Remembered devices are listed also while offline; the others only \
             once commissioned or found by a sync since the server started.",
            json!({ "type": "object", "properties": {} }),
        ),
        tool(
            "peek_device",
            "Re-address a listed device by its node id over a fresh CASE \
             session (no re-commissioning) and read its health diagnostics \
             (uptime, reboot count, heap).",
            json!({
                "type": "object",
                "properties": {
                    "node_id": { "type": "integer", "description": "The device's node id on our fabric." }
                },
                "required": ["node_id"]
            }),
        ),
        tool(
            "retrieve_logs",
            "Retrieve a diagnostic log from a listed device. Returns the log \
             text, or its size if it is binary.",
            json!({
                "type": "object",
                "properties": {
                    "node_id": { "type": "integer" },
                    "intent": {
                        "type": "string",
                        "enum": ["end_user_support", "network_diag", "crash_logs"],
                        "description": "Which log (default crash_logs)."
                    }
                },
                "required": ["node_id"]
            }),
        ),
        tool(
            "identify",
            "Ask a listed device to physically identify itself (blink/beep) for \
             a few seconds via the Identify cluster — to locate a unit.",
            json!({
                "type": "object",
                "properties": {
                    "node_id": { "type": "integer" },
                    "secs": { "type": "integer", "description": "Blink duration in seconds (default 3)." }
                },
                "required": ["node_id"]
            }),
        ),
        tool(
            "read_qr_image",
            "Read the setup code (MT: payload) from a picture of a device's QR \
             code: a PNG or JPEG file on this machine. The code can then be \
             passed to commission.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path of the image file." }
                },
                "required": ["path"]
            }),
        ),
        tool(
            "ota_images",
            "List the stored OTA images (name, vendor/product id, software \
             version) that devices can be updated to.",
            json!({ "type": "object", "properties": {} }),
        ),
    ];

    if caps.allow_mutate {
        tools.push(tool(
            "commission",
            "Commission a Matter device onto our fabric, reaching it over the \
             network at peer_addr, or over Bluetooth LE with ble=true (then \
             giving it the Wi-Fi or Thread network to join). It is listed from \
             then on. With remember=true a record of it is also kept (see \
             remember). MUTATING — alters the device's fabric table.",
            json!({
                "type": "object",
                "properties": {
                    "peer_addr": { "type": "string", "description": "Over the network: e.g. \"192.168.1.20:5540\" or \"[fe80::1]:5540\"." },
                    "ble": { "type": "boolean", "description": "Over Bluetooth LE instead (see discover). Default false." },
                    "ble_addr": { "type": "string", "description": "Over Bluetooth: the device's address (e.g. \"E8:F6:0A:60:E4:72\"); else the one matching the setup code." },
                    "wifi_ssid": { "type": "string", "description": "Over Bluetooth: the Wi-Fi network the device joins." },
                    "wifi_password": { "type": "string", "description": "The password of that Wi-Fi network." },
                    "thread_dataset": { "type": "string", "description": "Over Bluetooth: the Thread network the device joins, as its operational dataset in hex." },
                    "setup_code": { "type": "string", "description": "Setup passcode, manual pairing code or MT: QR code payload (default 20202021)." },
                    "remember": { "type": "boolean", "description": "Also keep a record of the device. Default false." }
                }
            }),
        ));
        tools.push(tool(
            "decommission",
            "Decommission a listed device: remove our fabric from it and drop it \
             from the list. A device that cannot be reached within 30 s is only \
             dropped from the list and still holds our fabric; sync lists it \
             again once it is online. With force=true the device is not \
             contacted at all. MUTATING — alters the device's fabric table.",
            json!({
                "type": "object",
                "properties": {
                    "node_id": { "type": "integer" },
                    "force": { "type": "boolean", "description": "Drop it from the list without contacting the device. Default false." }
                },
                "required": ["node_id"]
            }),
        ));
        tools.push(tool(
            "remember",
            "Keep a record of a listed device, so it can be tagged and stays \
             listed while offline and across restarts. MUTATING — changes the \
             stored device list.",
            json!({
                "type": "object",
                "properties": { "node_id": { "type": "integer" } },
                "required": ["node_id"]
            }),
        ));
        tools.push(tool(
            "set_tags",
            "Replace the local tags of a remembered device (controller-local labels \
             for grouping/filtering; never touches the device).",
            json!({
                "type": "object",
                "properties": {
                    "node_id": { "type": "integer" },
                    "tags": { "type": "array", "items": { "type": "string" } }
                },
                "required": ["node_id", "tags"]
            }),
        ));
        tools.push(tool(
            "sync",
            "Re-sync the stored device list with the devices on our fabric: add \
             the ones not listed and refresh the others' identity. MUTATING — \
             changes the stored device list.",
            json!({ "type": "object", "properties": {} }),
        ));
        tools.push(tool(
            "ota_upload",
            "Store an OTA image file (.ota) from a path on this machine, so \
             devices can be updated to it. MUTATING — changes the stored images.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path of the .ota file." }
                },
                "required": ["path"]
            }),
        ));
        tools.push(tool(
            "ota_delete",
            "Delete a stored OTA image. MUTATING — changes the stored images.",
            json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "The image name, as ota_images lists it." }
                },
                "required": ["name"]
            }),
        ));
        tools.push(tool(
            "ota_update",
            "Update a stored device to a stored OTA image: the device is told to \
             fetch it from us. Returns once the device is told; it then \
             downloads and applies the image on its own, which peek_device shows \
             as the new software version. MUTATING — flashes the device.",
            json!({
                "type": "object",
                "properties": {
                    "node_id": { "type": "integer" },
                    "name": { "type": "string", "description": "The image name, as ota_images lists it." }
                },
                "required": ["node_id", "name"]
            }),
        ));
    }

    tools
}

fn tool(name: &str, description: &str, input_schema: Value) -> Value {
    json!({ "name": name, "description": description, "inputSchema": input_schema })
}

// ---------------------------------------------------------------------------
// tools/call → wire::Request → Dispatch → wire::Response → tool result
// ---------------------------------------------------------------------------

type ToolErr = (i64, String);

async fn tools_call<D: Dispatch>(
    params: Value,
    dispatch: &D,
    caps: Caps,
) -> Result<Value, ToolErr> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or((jsonrpc::INVALID_PARAMS, "missing tool name".to_string()))?;
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    if MUTATING_TOOLS.contains(&name) && !caps.allow_mutate {
        return Err((
            jsonrpc::INVALID_PARAMS,
            format!(
                "tool '{name}' is disabled: the operator has not allowed changes (e.g. `matter-peek mcp --allow-mutate`)"
            ),
        ));
    }

    let response = if name == "ota_upload" {
        ota_upload(&args, dispatch).await?
    } else if name == "read_qr_image" {
        let path = arg_str(&args, "path")?;
        let image = dispatch
            .read_file(&path)
            .await
            .map_err(|e| (jsonrpc::INVALID_PARAMS, e))?;

        let image = BASE64.encode(image);
        call(dispatch, wire::Request::ReadQrImage { image }).await?
    } else {
        let request = build_request(name, &args)?;
        call(dispatch, request).await?
    };

    Ok(tool_result(&response))
}

/// Run one request through `dispatch`.
async fn call<D: Dispatch>(
    dispatch: &D,
    request: wire::Request,
) -> Result<wire::Response, ToolErr> {
    dispatch
        .dispatch(request)
        .await
        .map_err(|e| (jsonrpc::INTERNAL_ERROR, e))
}

/// Upload the OTA image file at the `path` argument, chunk by chunk. Answers
/// with the stored images, or with the first failure.
async fn ota_upload<D: Dispatch>(args: &Value, dispatch: &D) -> Result<wire::Response, ToolErr> {
    let path = arg_str(args, "path")?;
    let image = dispatch
        .read_file(&path)
        .await
        .map_err(|e| (jsonrpc::INVALID_PARAMS, e))?;

    let size = image.len() as u64;
    let upload_id = match call(dispatch, wire::Request::OtaUploadBegin { size }).await? {
        wire::Response::OtaUploadStarted { upload_id } => upload_id,
        other => return Ok(other),
    };

    let mut offset = 0;
    for chunk in image.chunks(UPLOAD_CHUNK_LEN) {
        let data = BASE64.encode(chunk);
        let request = wire::Request::OtaUploadChunk {
            upload_id,
            offset,
            data,
        };

        match call(dispatch, request).await? {
            wire::Response::Ok => offset += chunk.len() as u64,
            other => return Ok(other),
        }
    }

    call(dispatch, wire::Request::OtaUploadEnd { upload_id }).await
}

/// Map a single-request tool name + arguments to a [`wire::Request`].
fn build_request(name: &str, args: &Value) -> Result<wire::Request, ToolErr> {
    let req = match name {
        "discover" => wire::Request::Discover {
            discriminator: args
                .get("discriminator")
                .and_then(Value::as_u64)
                .map(|d| d as u16),
            setup_code: args
                .get("setup_code")
                .and_then(Value::as_str)
                .map(str::to_string),
            max: args.get("max").and_then(Value::as_u64).unwrap_or(16) as u8,
        },
        "list_devices" => wire::Request::ListDevices,
        "sync" => wire::Request::Sync,
        "decommission" => wire::Request::Decommission {
            node_id: arg_u64(args, "node_id")?,
            force: args.get("force").and_then(Value::as_bool).unwrap_or(false),
        },
        "ota_images" => wire::Request::OtaImages,
        "ota_delete" => wire::Request::OtaDelete {
            name: arg_str(args, "name")?,
        },
        "ota_update" => wire::Request::OtaUpdate {
            node_id: arg_u64(args, "node_id")?,
            name: arg_str(args, "name")?,
        },
        "peek_device" => wire::Request::PeekDevice {
            node_id: arg_u64(args, "node_id")?,
        },
        "retrieve_logs" => wire::Request::RetrieveLogs {
            node_id: arg_u64(args, "node_id")?,
            intent: match args.get("intent").and_then(Value::as_str) {
                None | Some("crash_logs") => api::LogIntent::CrashLogs,
                Some("end_user_support") => api::LogIntent::EndUserSupport,
                Some("network_diag") => api::LogIntent::NetworkDiag,
                Some(other) => {
                    return Err((
                        jsonrpc::INVALID_PARAMS,
                        format!("unknown log intent '{other}'"),
                    ))
                }
            },
        },
        "remember" => wire::Request::Remember {
            node_id: arg_u64(args, "node_id")?,
        },
        "identify" => wire::Request::Identify {
            node_id: arg_u64(args, "node_id")?,
            secs: args.get("secs").and_then(Value::as_u64).unwrap_or(3) as u16,
        },
        "commission" => wire::Request::Commission {
            via: commission_via(args)?,
            setup_code: args
                .get("setup_code")
                .and_then(Value::as_str)
                .unwrap_or("20202021")
                .to_string(),
            remember: args
                .get("remember")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        },
        "set_tags" => wire::Request::SetTags {
            node_id: arg_u64(args, "node_id")?,
            tags: args
                .get("tags")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
        },
        other => return Err((jsonrpc::METHOD_NOT_FOUND, format!("unknown tool: {other}"))),
    };
    Ok(req)
}

/// How `commission` reaches the device, from its arguments.
fn commission_via(args: &Value) -> Result<api::CommissionVia, ToolErr> {
    let opt_str = |key| args.get(key).and_then(Value::as_str).map(str::to_string);

    if !args.get("ble").and_then(Value::as_bool).unwrap_or(false) {
        return Ok(api::CommissionVia::Ip {
            addr: arg_str(args, "peer_addr")?,
        });
    }

    let network = match (opt_str("wifi_ssid"), opt_str("thread_dataset")) {
        (Some(ssid), _) => Some(api::NetworkCredentials::Wifi {
            ssid,
            password: opt_str("wifi_password").unwrap_or_default(),
        }),
        (None, Some(dataset)) => Some(api::NetworkCredentials::Thread { dataset }),
        (None, None) => None,
    };

    Ok(api::CommissionVia::Ble {
        addr: opt_str("ble_addr"),
        network,
    })
}

fn arg_u64(args: &Value, key: &str) -> Result<u64, ToolErr> {
    args.get(key).and_then(Value::as_u64).ok_or((
        jsonrpc::INVALID_PARAMS,
        format!("missing or non-integer '{key}'"),
    ))
}

fn arg_str(args: &Value, key: &str) -> Result<String, ToolErr> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or((
            jsonrpc::INVALID_PARAMS,
            format!("missing or non-string '{key}'"),
        ))
}

/// Render a [`wire::Response`] as an MCP `tools/call` result. The structured
/// payload goes into a single text content block as pretty JSON, a retrieved log
/// as its text (and `isError` is set for the error variant) — what MCP clients
/// render and feed to the model.
fn tool_result(response: &wire::Response) -> Value {
    let (text, is_error) = match response {
        wire::Response::Error { message } => (pretty(&json!({ "error": message })), true),
        wire::Response::Logs { intent, data, .. } => (log_text(*intent, data), false),
        other => (pretty(&labelled(other)), false),
    };

    json!({
        "content": [ { "type": "text", "text": text } ],
        "isError": is_error
    })
}

/// `response` as JSON, with the IDs, vendors, products and uptimes also as the
/// UI shows them (see [`display`]).
fn labelled(response: &wire::Response) -> Value {
    let mut value = serde_json::to_value(response).unwrap_or(json!({}));

    match response {
        wire::Response::Devices { devices, .. } => {
            for (d, v) in devices.iter().zip(entries(&mut value, "devices")) {
                v["node"] = json!(display::hex_u64(d.handle.node_id));
                v["vendor"] = json!(d.vendor());
                v["product"] = json!(d.product());
            }
        }
        wire::Response::Discovered { ble_devices, .. } => {
            for (d, v) in ble_devices.iter().zip(entries(&mut value, "ble_devices")) {
                v["vendor"] = json!(display::vendor_label(d.vendor_id, None));
                v["product"] = json!(display::product_label(d.vendor_id, d.product_id, None));
            }
        }
        wire::Response::OtaImages { images } => {
            for (i, v) in images.iter().zip(entries(&mut value, "images")) {
                v["vendor"] = json!(display::vendor_label(i.vendor_id, None));
                v["product"] = json!(display::product_label(i.vendor_id, i.product_id, None));
            }
        }
        wire::Response::Peek { health, .. } => {
            if let Some(secs) = health.general.and_then(|g| g.up_time_secs) {
                value["health"]["general"]["up_time"] = json!(display::duration(secs));
            }
        }
        _ => {}
    }

    value
}

/// The entries of array `key` of `value`.
fn entries<'a>(value: &'a mut Value, key: &str) -> impl Iterator<Item = &'a mut Value> {
    value
        .get_mut(key)
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
}

fn pretty(payload: &Value) -> String {
    serde_json::to_string_pretty(payload).unwrap_or_else(|_| payload.to_string())
}

/// A retrieved log for the model: its text, or its size if it is binary.
fn log_text(intent: api::LogIntent, data: &str) -> String {
    match BASE64.decode(data) {
        Ok(log) if log.is_empty() => format!("The device has no {intent:?} log."),
        Ok(log) => String::from_utf8(log).unwrap_or_else(|e| {
            format!(
                "The {intent:?} log is {} bytes of binary data.",
                e.as_bytes().len()
            )
        }),
        Err(e) => format!("The {intent:?} log is not valid base64: {e}"),
    }
}
