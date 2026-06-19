//! The MCP method layer: handshake (`initialize`), tool discovery
//! (`tools/list`), and tool invocation (`tools/call`).
//!
//! Transport- and controller-agnostic (`no_std` + `alloc`). A tool call maps to
//! a [`matter_peek_proto::Request`], which is run by an injected [`Dispatch`]
//! (the desktop shell implements it over the `matter-peek-rpc` actor). The
//! single final [`matter_peek_proto::Response`] is rendered back as the tool
//! result; streamed progress is not surfaced over MCP (a tool call is
//! request/response).
//!
//! **Safety:** read-only tools (discover / list / peek / identify) are always
//! registered. Mutating tools (commission / set_tags) are registered only when
//! [`Caps::allow_mutate`] is set — so an agent cannot commission or alter devices
//! unless the human operator opted in (the shell sets this from `--allow-mutate`).

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::future::Future;

use serde_json::{json, Value};

use matter_peek_proto as proto;

use crate::jsonrpc::{self, Response};

const SERVER_NAME: &str = "matter-peek";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
/// The MCP protocol revision we speak.
const PROTOCOL_VERSION: &str = "2024-11-05";

/// Runs one [`proto::Request`] against the controller and yields its single final
/// response. The protocol layer is agnostic to *how* — the desktop shell
/// implements this over the `matter-peek-rpc` actor channel; a future esp shell
/// would implement it over its own in-process controller.
pub trait Dispatch {
    /// `Err(message)` is for transport/actor failures (becomes a JSON-RPC
    /// internal error); a controller-level failure is a normal
    /// [`proto::Response::Error`] carried in `Ok`.
    fn dispatch(
        &self,
        request: proto::Request,
    ) -> impl Future<Output = Result<proto::Response, String>>;
}

/// Whether mutating tools (commission / set_tags) are exposed.
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
            "Vendor-agnostic Matter diagnostic controller. Read-only tools \
             (discover, list_devices, peek_device, identify) are always \
             available. Commissioning and tag edits require the server to have \
             been started with --allow-mutate."
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
            "Browse the local network for commissionable Matter devices via \
             mDNS. Returns a list of addresses + commissionable ids.",
            json!({
                "type": "object",
                "properties": {
                    "discriminator": {
                        "type": "integer",
                        "description": "Optional 12-bit discriminator to narrow to one device."
                    },
                    "max": { "type": "integer", "description": "Max devices to return (default 16)." }
                }
            }),
        ),
        tool(
            "list_devices",
            "List the devices this controller has persistently commissioned \
             (from the local store), with their identity, fingerprint, and tags.",
            json!({ "type": "object", "properties": {} }),
        ),
        tool(
            "peek_device",
            "Re-address an already-commissioned device by its node id over a \
             fresh CASE session (no re-commissioning) and read its health \
             diagnostics (uptime, reboot count, heap), plus optional crash-log \
             size.",
            json!({
                "type": "object",
                "properties": {
                    "node_id": { "type": "integer", "description": "The device's node id on our fabric." },
                    "with_logs": { "type": "boolean", "description": "Also retrieve crash logs (default false)." }
                },
                "required": ["node_id"]
            }),
        ),
        tool(
            "identify",
            "Ask a stored device to physically identify itself (blink/beep) for \
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
    ];

    if caps.allow_mutate {
        tools.push(tool(
            "commission",
            "Commission a Matter device at the given address. With persistent=true \
             the device is kept on our fabric and recorded in the store; with \
             persistent=false it is commissioned on a throwaway fabric, diagnosed, \
             then removed. MUTATING — alters the device's fabric table.",
            json!({
                "type": "object",
                "properties": {
                    "peer_addr": { "type": "string", "description": "e.g. \"192.168.1.20:5540\" or \"[fe80::1]:5540\"." },
                    "passcode": { "type": "integer", "description": "Setup passcode (default 20202021)." },
                    "persistent": { "type": "boolean", "description": "Keep the device (true) or self-destruct after (false). Default false." }
                },
                "required": ["peer_addr"]
            }),
        ));
        tools.push(tool(
            "set_tags",
            "Replace the local tags of a stored device (controller-local labels \
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
    }

    tools
}

fn tool(name: &str, description: &str, input_schema: Value) -> Value {
    json!({ "name": name, "description": description, "inputSchema": input_schema })
}

// ---------------------------------------------------------------------------
// tools/call → proto::Request → Dispatch → proto::Response → tool result
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

    let request = build_request(name, &args, caps)?;
    let response = dispatch
        .dispatch(request)
        .await
        .map_err(|e| (jsonrpc::INTERNAL_ERROR, e))?;
    Ok(tool_result(&response))
}

/// Map a tool name + arguments to a [`proto::Request`], enforcing the mutate gate.
fn build_request(name: &str, args: &Value, caps: Caps) -> Result<proto::Request, ToolErr> {
    let mutating = matches!(name, "commission" | "set_tags");
    if mutating && !caps.allow_mutate {
        return Err((
            jsonrpc::INVALID_PARAMS,
            format!("tool '{name}' is disabled; start the server with --allow-mutate to enable it"),
        ));
    }

    let req = match name {
        "discover" => proto::Request::Discover {
            discriminator: args
                .get("discriminator")
                .and_then(Value::as_u64)
                .map(|d| d as u16),
            max: args.get("max").and_then(Value::as_u64).unwrap_or(16) as u8,
        },
        "list_devices" => proto::Request::ListDevices,
        "peek_device" => proto::Request::PeekDevice {
            node_id: arg_u64(args, "node_id")?,
            with_logs: args
                .get("with_logs")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        },
        "identify" => proto::Request::Identify {
            node_id: arg_u64(args, "node_id")?,
            secs: args.get("secs").and_then(Value::as_u64).unwrap_or(3) as u16,
        },
        "commission" => proto::Request::RunVertical {
            peer_addr: args
                .get("peer_addr")
                .and_then(Value::as_str)
                .ok_or((jsonrpc::INVALID_PARAMS, "missing peer_addr".to_string()))?
                .to_string(),
            passcode: args
                .get("passcode")
                .and_then(Value::as_u64)
                .unwrap_or(20202021) as u32,
            policy: if args
                .get("persistent")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                proto::FabricPolicy::Persistent
            } else {
                proto::FabricPolicy::Ephemeral
            },
        },
        "set_tags" => proto::Request::SetTags {
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

fn arg_u64(args: &Value, key: &str) -> Result<u64, ToolErr> {
    args.get(key).and_then(Value::as_u64).ok_or((
        jsonrpc::INVALID_PARAMS,
        format!("missing or non-integer '{key}'"),
    ))
}

/// Render a [`proto::Response`] as an MCP `tools/call` result. The structured
/// payload goes into a single text content block as pretty JSON (and `isError`
/// is set for the error variant) — what MCP clients render and feed to the model.
fn tool_result(response: &proto::Response) -> Value {
    let (payload, is_error) = match response {
        proto::Response::Error { message } => (json!({ "error": message }), true),
        other => (serde_json::to_value(other).unwrap_or(json!({})), false),
    };

    let text = serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string());
    json!({
        "content": [ { "type": "text", "text": text } ],
        "isError": is_error
    })
}
