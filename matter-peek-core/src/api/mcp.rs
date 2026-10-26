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

use crate::api::{self, display, query, wire, FabricRef};

use jsonrpc::Response;

pub mod jsonrpc;

const SERVER_NAME: &str = "matter-peek";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
/// The MCP protocol revisions we speak, newest first: a client asking for
/// another gets the newest.
const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2024-11-05"];

/// The tools that change devices or the stored state; see [`Caps`].
const MUTATING_TOOLS: &[&str] = &[
    "commission",
    "decommission",
    "register_check_ins",
    "unregister_check_ins",
    "remember",
    "set_tags",
    "sync",
    "ota_upload",
    "ota_delete",
    "ota_update",
    "add_network",
    "remove_network",
    "connect_network",
    "reorder_network",
    "open_commissioning_window",
    "close_commissioning_window",
    "set_device_time",
    "remove_device_fabric",
    "create_fabric",
    "rename_fabric",
    "set_default_fabric",
    "delete_fabric",
    "write_attribute",
    "invoke_command",
];

/// What an agent should know about the tools, for its instructions.
pub(crate) const INSTRUCTIONS: &str = "Vendor-agnostic Matter controller. Read-only tools \
     (discover, list_devices, peek_device, read_model, read_events, read_network, retrieve_logs, identify, wake_device, \
     read_qr_image, ota_images, ota_updates, commissioning_window, \
     device_time, list_fabrics) are always available. The tools that change \
     devices or the stored state (commission, decommission, \
     register/unregister_check_ins, remember, set_tags, sync, ota_upload, ota_delete, ota_update, add_network, \
     remove_network, connect_network, reorder_network, \
     open/close_commissioning_window, set_device_time, remove_device_fabric, \
     create/rename/set_default/delete_fabric, write_attribute, invoke_command) are \
     available only if the operator allowed them (e.g. `matter-peek mcp \
     --allow-mutate`). Our devices are on fabrics of ours (\"homes\", see \
     list_fabrics); the tools that list or commission take an optional \
     `fabric`, else the default one. A device is named by its node_id \
     across all of them: the number list_devices gives, or its hex form \
     (\"0x1B67B\"). A failed tool says why in plain words, with \
     the error code in parentheses: pass that on to the user, with what they \
     can do about it.";

/// The tools that act on a device without changing what is stored: offered
/// without [`Caps::allow_mutate`], but not read-only, so an agent asks first.
const ACTING_TOOLS: &[&str] = &["identify", "wake_device"];

/// The mutating tools whose harm may be hard to undo: they cost a device its
/// fabric, its network or its firmware, or lose an image.
const DESTRUCTIVE_TOOLS: &[&str] = &[
    "decommission",
    "invoke_command",
    "delete_fabric",
    "remove_device_fabric",
    "ota_delete",
    "ota_update",
    "add_network",
    "remove_network",
    "connect_network",
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
        "initialize" => Response::ok(id, initialize_result(&req.params)),
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

fn initialize_result(params: &Value) -> Value {
    let asked = params.get("protocolVersion").and_then(Value::as_str);
    let version = PROTOCOL_VERSIONS
        .iter()
        .find(|version| Some(**version) == asked)
        .unwrap_or(&PROTOCOL_VERSIONS[0]);

    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION },
        "instructions": INSTRUCTIONS
    })
}

// ---------------------------------------------------------------------------
// Tool catalog
// ---------------------------------------------------------------------------

/// The tool catalog for `tools/list`, filtered by `caps`.
pub(crate) fn tool_list(caps: Caps) -> Vec<Value> {
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
            "advertised_devices",
            "List the commissioned Matter devices the local network advertises \
             (operational mDNS), on our fabrics or other controllers' (Google, \
             Apple, Home Assistant, ...), one entry per device: its addresses \
             and, per fabric, the device's node id on it, our name for the \
             fabric if it is ours, and the vendor and label of its controller \
             when a device of ours is on that fabric too. A device with ours \
             false can be commissioned by us only once its controller opens a \
             commissioning window on it (in that controller's app: share, or \
             pair with another app); nothing else can be read from it. Takes a \
             few seconds.",
            json!({ "type": "object", "properties": {} }),
        ),
        tool(
            "list_devices",
            "List the devices on a fabric of ours (the default one unless \
             `fabric` names another, or all), with the fabric they are on, \
             their identity, versions, fingerprint, tags, whether they are \
             online and whether they are remembered. Remembered devices are \
             listed also while offline; the others only once commissioned or \
             found by a sync since the server started. Each has its `details`: \
             Matter version, clusters served, endpoints, power, ICD mode, and \
             network (SSID or Thread network name, Thread role). `filter` \
             narrows the list, e.g. to the devices to act on.",
            json!({ "type": "object", "properties": {
                "fabric": fabric_schema(),
                "filter": { "type": "string", "description": query::help() },
            } }),
        ),
        tool(
            "list_fabrics",
            "List our fabrics: the \"homes\" devices are commissioned into, each \
             with a root CA of its own: name, index, fabric ID, our node ID on \
             it, how many devices are on it, and which is the default (what a \
             tool naming no fabric gets).",
            json!({ "type": "object", "properties": {} }),
        ),
        tool(
            "read_model",
            "Read the data model of a listed device: its endpoints and their \
             device types, each endpoint's clusters with their attribute \
             values by name (enums by name, bitmaps as the names of the bits \
             set, temperatures in 0.01 degC) and which are writable, and the \
             commands they accept with their fields' types. Narrow it with \
             endpoint and cluster (an id or a name, e.g. \"Thermostat\").",
            json!({
                "type": "object",
                "properties": {
                    "node_id": node_id_schema(),
                    "endpoint": { "type": "integer", "description": "Only this endpoint." },
                    "cluster": { "type": ["string", "integer"], "description": "Only this cluster: its id or name." }
                },
                "required": ["node_id"]
            }),
        ),
        tool(
            "read_events",
            "Read the events a listed device keeps (switch presses, alarms, \
             faults, state changes, boots, ...), oldest first: each with its \
             number, priority, time (UTC, or since the device started), \
             endpoint, cluster and event by name, and its fields by name. \
             Narrow it with endpoint and cluster (an id or a name).",
            json!({
                "type": "object",
                "properties": {
                    "node_id": node_id_schema(),
                    "endpoint": { "type": "integer", "description": "Only this endpoint." },
                    "cluster": { "type": ["string", "integer"], "description": "Only this cluster: its id or name." }
                },
                "required": ["node_id"]
            }),
        ),
        tool(
            "peek_device",
            "Re-address a listed device by its node id over a fresh CASE \
             session (no re-commissioning) and read its health diagnostics \
             (uptime, reboot count, heap, and the threads/tasks of its \
             software with their stack size, peak use and free stack, where \
             the device reports them), and the fabrics it is on: the \
             controllers (ecosystems: Google, Apple, Home Assistant, ...) it \
             belongs to, ours marked, of how many it supports.",
            json!({
                "type": "object",
                "properties": {
                    "node_id": node_id_schema()
                },
                "required": ["node_id"]
            }),
        ),
        tool(
            "read_network",
            "Read a listed device's networking: its network interfaces and their \
             addresses, the networks it knows (Wi-Fi SSIDs / Thread networks) and \
             which one it is on, the outcome of its last network operation, and \
             the diagnostics of its network (Wi-Fi: access point, signal, channel; \
             Thread: role, channel, PAN, neighbors with their signal; Ethernet: \
             speed, carrier).",
            json!({
                "type": "object",
                "properties": {
                    "node_id": node_id_schema()
                },
                "required": ["node_id"]
            }),
        ),
        tool(
            "thread_mesh",
            "What the Thread mesh holds, as our Thread devices see it: for \
             questions like how many sleepy devices there are, which nodes \
             route, whose parent a device is. `counts` sums the nodes up by \
             role. Each node has its role (leader, router, reed, end_device: \
             listens all the time, sleepy_end_device: sleeps and polls its \
             parent), `sleepy`, how it sleeps (`sleep`: ICD long or short idle \
             time, its idle and active poll intervals) and its parent. \
             `lan_nodes` are Matter nodes on Thread of other fabrics the LAN \
             advertises, with how they sleep: where they sit is unknown, and \
             some may also be among the nodes. Also the networks, the links \
             (quality 0-3 and \
             signal each way), the routes to routers with no known link, the \
             border routers and the findings. Only nodes our Thread devices \
             report or the LAN advertises are known; whether a sleepy device \
             uses CSL is not. Reads each Thread device of `fabric` (default: \
             the default fabric; or all) in turn: a few seconds.",
            json!({ "type": "object", "properties": { "fabric": fabric_schema() } }),
        ),
        tool(
            "thread_findings",
            "What is wrong with the Thread mesh, in plain words: competing or \
             split networks, weak or one-way links, routers some devices depend \
             on, a single border router, devices that keep changing parent or \
             losing the network, a Thread channel on top of busy Wi-Fi, default \
             credentials, no route from this machine to the Thread network. \
             With the networks and the nodes counted by role; for the nodes \
             themselves (which are sleepy, whose parent is whose), use \
             thread_mesh. Of the Thread devices of `fabric` (default: the \
             default fabric; or all).",
            json!({ "type": "object", "properties": { "fabric": fabric_schema() } }),
        ),
        tool(
            "retrieve_logs",
            "Retrieve a diagnostic log from a listed device. Returns the log \
             text, or its size if it is binary.",
            json!({
                "type": "object",
                "properties": {
                    "node_id": node_id_schema(),
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
            "Make a listed device blink or beep for a few seconds (Identify \
             cluster), so that the user can find it; a relay may click, switching \
             what is connected to it. Only when the user asks to locate or \
             identify a device: it is not a way to check one (that is \
             peek_device or read_network).",
            json!({
                "type": "object",
                "properties": {
                    "node_id": node_id_schema(),
                    "secs": { "type": "integer", "description": "Blink duration in seconds (default 3)." }
                },
                "required": ["node_id"]
            }),
        ),
        tool(
            "wake_device",
            "Keep a listed device awake for a while, so that it answers. One \
             that sleeps long (details.icd.mode lit) answers only while awake: \
             this waits for it to wake up, up to its idle time (minutes; \
             details.icd.idle_mode_secs), unless the user wakes it as its \
             wake-up hint says, then keeps it awake for about 30 s. Use it \
             before reading or acting on such a device; call it again to keep \
             it awake longer.",
            json!({
                "type": "object",
                "properties": { "node_id": node_id_schema() },
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
        tool(
            "ota_updates",
            "List the OTA updates devices asked us for in the last hour, the \
             latest per device: the image offered, the stage (offered, \
             downloading with its percent and bytes, applying, applied, or not \
             offered), how long it has run, how long since its last progress, \
             and the time left estimated from the pace so far. A download with \
             no progress for minutes has likely stalled. An update started with \
             ota_update shows once the device asks for it.",
            json!({ "type": "object", "properties": {} }),
        ),
        tool(
            "commissioning_window",
            "Read a device's commissioning window: closed, or open (enhanced, \
             or basic with its own setup code) and by the controller of which \
             vendor; the pairing code while it is a window we opened, with the \
             time left; and the fabrics the device is on, of how many it \
             supports.",
            json!({
                "type": "object",
                "properties": { "node_id": node_id_schema() },
                "required": ["node_id"]
            }),
        ),
        tool(
            "device_time",
            "Read a device's clock (its Time Synchronization cluster): its UTC \
             time and how far it is from ours, how accurate it claims to be and \
             where its time comes from, the node it takes its time from (we \
             are a time source for our devices), its time zone, daylight \
             saving times and local time, and what it can do (keep a time \
             zone, NTP client or server, take time from a node). A device \
             without the cluster keeps no clock we can see.",
            json!({
                "type": "object",
                "properties": { "node_id": node_id_schema() },
                "required": ["node_id"]
            }),
        ),
    ];

    if caps.allow_mutate {
        tools.push(tool(
            "write_attribute",
            "MUTATING: write an attribute of a listed device (one read_model \
             lists as writable) and answer with its cluster read again. \
             value is JSON: an enum by name or number, a bitmap as a list of \
             bit names, a struct as an object of fields.",
            json!({
                "type": "object",
                "properties": {
                    "node_id": node_id_schema(),
                    "endpoint": { "type": "integer" },
                    "cluster": { "type": ["string", "integer"], "description": "Its id or name." },
                    "attribute": { "type": ["string", "integer"], "description": "Its id or name." },
                    "value": { "description": "The value, as read_model shows them." }
                },
                "required": ["node_id", "endpoint", "cluster", "attribute", "value"]
            }),
        ));
        tools.push(tool(
            "invoke_command",
            "MUTATING: invoke a command of a listed device (one read_model lists \
             for the cluster), with its fields as an object by field name; \
             answers with the response's fields, if it has a response. A \
             command can do anything the device offers, even remove a fabric: \
             say what it will do before invoking it.",
            json!({
                "type": "object",
                "properties": {
                    "node_id": node_id_schema(),
                    "endpoint": { "type": "integer" },
                    "cluster": { "type": ["string", "integer"], "description": "Its id or name." },
                    "command": { "type": ["string", "integer"], "description": "Its id or name." },
                    "fields": { "type": "object", "description": "The command's fields, by name." }
                },
                "required": ["node_id", "endpoint", "cluster", "command"]
            }),
        ));
        tools.push(tool(
            "commission",
            "Commission a Matter device onto a fabric of ours (`fabric`, else \
             the default one), reaching it over the \
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
                    "remember": { "type": "boolean", "description": "Also keep a record of the device. Default false." },
                    "fabric": fabric_schema()
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
                    "node_id": node_id_schema(),
                    "force": { "type": "boolean", "description": "Drop it from the list without contacting the device. Default false." }
                },
                "required": ["node_id"]
            }),
        ));
        tools.push(tool(
            "register_check_ins",
            "Register for the Check-Ins of a listed device that can idle long \
             (details.icd.long_idle and check_in in list_devices): it then sleeps \
             long (LIT) between active periods, answers only while awake, and \
             checks in with us when it wakes and our subscription is gone. A \
             commission does this by itself. MUTATING — changes how the device \
             idles.",
            json!({
                "type": "object",
                "properties": { "node_id": node_id_schema() },
                "required": ["node_id"]
            }),
        ));
        tools.push(tool(
            "unregister_check_ins",
            "Drop our registration for the Check-Ins of a listed device \
             (check_ins in list_devices): with no other client registered, it \
             idles short again (SIT), reachable within seconds. MUTATING — \
             changes how the device idles.",
            json!({
                "type": "object",
                "properties": { "node_id": node_id_schema() },
                "required": ["node_id"]
            }),
        ));
        tools.push(tool(
            "remove_device_fabric",
            "Remove another controller's fabric from a device, by the device's \
             index of it (`index` of the fabrics peek_device lists): that \
             controller (Google Home, Home Assistant, Apple Home, ...) loses the \
             device until it is shared with it again. Frees a fabric slot. Not \
             our own fabric: that is decommission. Returns the device's fabrics \
             then. MUTATING — alters the device's fabric table.",
            json!({
                "type": "object",
                "properties": {
                    "node_id": node_id_schema(),
                    "fabric_index": { "type": "integer", "description": "The device's index of the fabric to remove." }
                },
                "required": ["node_id", "fabric_index"]
            }),
        ));
        tools.push(tool(
            "remember",
            "Keep a record of a listed device, so it can be tagged and stays \
             listed while offline and across restarts. MUTATING — changes the \
             stored device list.",
            json!({
                "type": "object",
                "properties": { "node_id": node_id_schema() },
                "required": ["node_id"]
            }),
        ));
        let network = json!({
            "node_id": node_id_schema(),
            "endpoint": {
                "type": "integer",
                "description": "Endpoint of the network interface (default 0, the primary one)."
            },
            "ssid": { "type": "string", "description": "The Wi-Fi network, by SSID." },
            "network_id_hex": {
                "type": "string",
                "description": "Or the network by the id_hex read_network gives (e.g. a Thread extended PAN ID)."
            }
        });
        tools.push(tool(
            "add_network",
            "Give a listed device a Wi-Fi network (ssid + password) or a Thread network \
             (thread_dataset, hex), or new credentials for one it knows. Committed under a \
             fail-safe; new credentials for the network the device is connected to are \
             proven by reconnecting with them, else the device returns to its old ones. \
             MUTATING — changes the device's networks.",
            json!({
                "type": "object",
                "properties": {
                    "node_id": node_id_schema(),
                    "endpoint": { "type": "integer", "description": "Default 0." },
                    "ssid": { "type": "string" },
                    "password": { "type": "string" },
                    "thread_dataset": { "type": "string", "description": "Thread operational dataset, hex." }
                },
                "required": ["node_id"]
            }),
        ));
        tools.push(tool(
            "remove_network",
            "Make a listed device forget a network (not the one it is connected to). \
             MUTATING — changes the device's networks.",
            json!({ "type": "object", "properties": network.clone(), "required": ["node_id"] }),
        ));
        tools.push(tool(
            "connect_network",
            "Connect a listed device to a network it knows. Committed only once the device \
             answers on that network; otherwise it returns to its previous network by itself. \
             MUTATING — moves the device to another network.",
            json!({ "type": "object", "properties": network.clone(), "required": ["node_id"] }),
        ));
        let mut reorder = network;
        reorder["index"] = json!({
            "type": "integer",
            "description": "The network's new place in the order the device tries them, from 0."
        });
        tools.push(tool(
            "reorder_network",
            "Move a network of a listed device to another place in the order the device tries them. \
             MUTATING — changes the device's networks.",
            json!({ "type": "object", "properties": reorder, "required": ["node_id", "index"] }),
        ));
        tools.push(tool(
            "set_tags",
            "Replace the local tags of a remembered device (controller-local labels \
             for grouping/filtering; never touches the device). MUTATING — changes \
             our record of the device.",
            json!({
                "type": "object",
                "properties": {
                    "node_id": node_id_schema(),
                    "tags": { "type": "array", "items": { "type": "string" } }
                },
                "required": ["node_id", "tags"]
            }),
        ));
        tools.push(tool(
            "sync",
            "Re-sync the stored device list with the devices on our fabrics: \
             add the ones not listed and refresh the others' identity; returns \
             the devices on `fabric` (default: the default fabric; or all). \
             MUTATING — changes the stored device list.",
            json!({ "type": "object", "properties": { "fabric": fabric_schema() } }),
        ));
        tools.push(tool(
            "create_fabric",
            "Create a fabric of ours (a \"home\"), with a root CA of its own, to \
             commission devices into. MUTATING.",
            json!({
                "type": "object",
                "properties": { "name": { "type": "string", "description": "Its name, unique among ours (1 to 32 characters)." } },
                "required": ["name"]
            }),
        ));
        tools.push(tool(
            "rename_fabric",
            "Rename a fabric of ours. MUTATING.",
            json!({
                "type": "object",
                "properties": {
                    "fabric": fabric_schema(),
                    "name": { "type": "string", "description": "Its new name." }
                },
                "required": ["fabric", "name"]
            }),
        ));
        tools.push(tool(
            "set_default_fabric",
            "Make a fabric of ours the default: what a tool naming no fabric \
             gets. MUTATING.",
            json!({
                "type": "object",
                "properties": { "fabric": fabric_schema() },
                "required": ["fabric"]
            }),
        ));
        tools.push(tool(
            "delete_fabric",
            "Delete a fabric of ours, with its root CA; not the default. Its \
             devices: refused while there are any, unless devices=decommission \
             (remove the fabric from each, as decommission) or devices=forget \
             (drop them from the list; they keep the dead fabric in a slot \
             until another controller removes it). MUTATING — can't be undone.",
            json!({
                "type": "object",
                "properties": {
                    "fabric": fabric_schema(),
                    "devices": { "type": "string", "enum": ["refuse", "decommission", "forget"], "description": "What becomes of its devices (default refuse)." }
                },
                "required": ["fabric"]
            }),
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
             downloads and applies the image on its own: ota_updates follows \
             it, and peek_device shows the new software version. MUTATING — flashes the device.",
            json!({
                "type": "object",
                "properties": {
                    "node_id": node_id_schema(),
                    "name": { "type": "string", "description": "The image name, as ota_images lists it." }
                },
                "required": ["node_id", "name"]
            }),
        ));
        tools.push(tool(
            "open_commissioning_window",
            "Open a commissioning window on a device on our fabric, so that \
             another controller (Google Home, Home Assistant, Apple Home, \
             SmartThings...) can commission it too: returns the 11-digit \
             manual pairing code and the QR code text to give the user, and how \
             long the window stays open. The code is new for each window and \
             works only while it is open. A window we opened that is still open \
             is returned again. Refused while another controller's window is \
             open, or when the device is on as many fabrics as it supports. \
             MUTATING — lets another controller onto the device.",
            json!({
                "type": "object",
                "properties": {
                    "node_id": node_id_schema(),
                    "minutes": { "type": "integer", "description": "How long the window stays open, 3 to 15 minutes (default 15)." }
                },
                "required": ["node_id"]
            }),
        ));
        tools.push(tool(
            "set_device_time",
            "Set a device's clock from ours (this host's): its UTC time; its \
             time zone and daylight saving time, if it keeps them; and us as \
             its trusted time source, if it takes its time from a node \
             (replacing another controller's). Returns each step, set or why \
             not (a device may keep its own time), and the clock after. Done \
             on its own after commissioning. MUTATING.",
            json!({
                "type": "object",
                "properties": { "node_id": node_id_schema() },
                "required": ["node_id"]
            }),
        ));
        tools.push(tool(
            "close_commissioning_window",
            "Close the commissioning window on a device, whoever opened it. \
             MUTATING.",
            json!({
                "type": "object",
                "properties": { "node_id": node_id_schema() },
                "required": ["node_id"]
            }),
        ));
    }

    tools
}

fn tool(name: &str, description: &str, input_schema: Value) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": input_schema,
        "annotations": {
            "readOnlyHint": !MUTATING_TOOLS.contains(&name) && !ACTING_TOOLS.contains(&name),
            "destructiveHint": DESTRUCTIVE_TOOLS.contains(&name),
        }
    })
}

/// A device, as the tools name it.
/// A fabric of ours: by name, index (`#2`) or, where a tool takes several,
/// `all`.
fn fabric_schema() -> Value {
    json!({ "type": "string", "description": "A fabric of ours: its name or index (\"#2\"); \"all\" for every one, where it makes sense. Default: the default fabric." })
}

fn node_id_schema() -> Value {
    json!({
        "type": ["integer", "string"],
        "description": "The device's node id: the number list_devices gives, or its hex form as shown (e.g. \"0x1B67B\")."
    })
}

// ---------------------------------------------------------------------------
// tools/call → wire::Request → Dispatch → wire::Response → tool result
// ---------------------------------------------------------------------------

pub(crate) type ToolErr = (i64, String);

pub(crate) async fn tools_call<D: Dispatch>(
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

    if let ("thread_findings", wire::Response::ThreadMesh { mesh }) = (name, &response) {
        return Ok(text_result(compact(findings_only(mesh)).to_string()));
    }

    Ok(tool_result(&response))
}

/// The findings of `mesh`, with its networks and the devices not read.
fn findings_only(mesh: &api::ThreadMesh) -> Value {
    let mut value = json!({
        "networks": mesh.networks,
        "counts": mesh.counts,
        "findings": mesh.findings,
        "unread": mesh.unread,
    });
    for (net, v) in mesh.networks.iter().zip(entries(&mut value, "networks")) {
        v["xpan"] = json!(hex64(net.extended_pan_id));
    }

    value
}

fn text_result(text: String) -> Value {
    json!({
        "content": [ { "type": "text", "text": text } ],
        "isError": false
    })
}

/// 64 bits as 16 hex digits, as Thread tools show addresses and PAN IDs.
fn hex64(value: u64) -> String {
    format!("{value:016X}")
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
        "advertised_devices" => wire::Request::AdvertisedDevices,
        "list_devices" => wire::Request::ListDevices {
            fabric: arg_fabric(args),
            filter: args
                .get("filter")
                .and_then(Value::as_str)
                .map(str::to_string),
        },
        "list_fabrics" => wire::Request::Fabrics,
        "create_fabric" => wire::Request::CreateFabric {
            name: arg_str(args, "name")?,
        },
        "rename_fabric" => wire::Request::RenameFabric {
            fabric: arg_one_fabric(args)?,
            name: arg_str(args, "name")?,
        },
        "set_default_fabric" => wire::Request::SetDefaultFabric {
            fabric: arg_one_fabric(args)?,
        },
        "delete_fabric" => wire::Request::DeleteFabric {
            fabric: arg_one_fabric(args)?,
            deletion: match args.get("devices").and_then(Value::as_str) {
                None | Some("refuse") => api::FabricDeletion::Refuse,
                Some("decommission") => api::FabricDeletion::Decommission,
                Some("forget") => api::FabricDeletion::Forget,
                Some(other) => {
                    return Err((
                        jsonrpc::INVALID_PARAMS,
                        format!("'devices' is refuse, decommission or forget, not {other}"),
                    ))
                }
            },
        },
        "sync" => wire::Request::Sync {
            fabric: arg_fabric(args),
        },
        "remove_device_fabric" => wire::Request::RemoveDeviceFabric {
            node_id: arg_node(args)?,
            fabric_index: u8::try_from(arg_u64(args, "fabric_index")?).map_err(|_| {
                (
                    jsonrpc::INVALID_PARAMS,
                    "'fabric_index' is 1 to 254".to_string(),
                )
            })?,
        },
        "decommission" => wire::Request::Decommission {
            node_id: arg_node(args)?,
            force: args.get("force").and_then(Value::as_bool).unwrap_or(false),
        },
        "register_check_ins" => wire::Request::RegisterCheckIns {
            node_id: arg_node(args)?,
        },
        "unregister_check_ins" => wire::Request::UnregisterCheckIns {
            node_id: arg_node(args)?,
        },
        "ota_images" => wire::Request::OtaImages,
        "ota_updates" => wire::Request::OtaUpdates,
        "open_commissioning_window" => wire::Request::OpenCommissioningWindow {
            node_id: arg_node(args)?,
            timeout_secs: args
                .get("minutes")
                .and_then(Value::as_u64)
                .map(|minutes| (minutes * 60).min(u16::MAX.into()) as u16),
        },
        "close_commissioning_window" => wire::Request::CloseCommissioningWindow {
            node_id: arg_node(args)?,
        },
        "commissioning_window" => wire::Request::CommissioningWindow {
            node_id: arg_node(args)?,
        },
        "device_time" => wire::Request::DeviceTime {
            node_id: arg_node(args)?,
        },
        "set_device_time" => wire::Request::SetDeviceTime {
            node_id: arg_node(args)?,
        },
        "ota_delete" => wire::Request::OtaDelete {
            name: arg_str(args, "name")?,
        },
        "ota_update" => wire::Request::OtaUpdate {
            node_id: arg_node(args)?,
            name: arg_str(args, "name")?,
        },
        "read_model" => wire::Request::ReadModel {
            node_id: arg_node(args)?,
            endpoint: args
                .get("endpoint")
                .and_then(Value::as_u64)
                .map(|e| e as u16),
            cluster: args.get("cluster").map(id_or_name),
        },
        "read_events" => wire::Request::ReadEvents {
            node_id: arg_node(args)?,
            endpoint: args
                .get("endpoint")
                .and_then(Value::as_u64)
                .map(|e| e as u16),
            cluster: args.get("cluster").map(id_or_name),
        },
        "write_attribute" => wire::Request::WriteAttribute {
            node_id: arg_node(args)?,
            endpoint: arg_endpoint(args)?,
            cluster: arg_id_or_name(args, "cluster")?,
            attribute: arg_id_or_name(args, "attribute")?,
            value: args
                .get("value")
                .ok_or((jsonrpc::INVALID_PARAMS, "missing 'value'".into()))?
                .to_string(),
        },
        "invoke_command" => wire::Request::InvokeCommand {
            node_id: arg_node(args)?,
            endpoint: arg_endpoint(args)?,
            cluster: arg_id_or_name(args, "cluster")?,
            command: arg_id_or_name(args, "command")?,
            fields: args.get("fields").map(Value::to_string).unwrap_or_default(),
        },
        "peek_device" => wire::Request::PeekDevice {
            node_id: arg_node(args)?,
        },
        "read_network" => wire::Request::ReadNetwork {
            node_id: arg_node(args)?,
        },
        "thread_mesh" | "thread_findings" => wire::Request::ReadThreadMesh {
            fabric: arg_fabric(args),
        },
        "add_network" => wire::Request::ChangeNetwork {
            node_id: arg_node(args)?,
            endpoint: arg_endpoint(args)?,
            change: api::NetworkChange::AddOrUpdate {
                credentials: match args.get("thread_dataset").and_then(Value::as_str) {
                    Some(dataset) => api::NetworkCredentials::Thread {
                        dataset: dataset.to_string(),
                    },
                    None => api::NetworkCredentials::Wifi {
                        ssid: arg_str(args, "ssid")?,
                        password: arg_str(args, "password")?,
                    },
                },
            },
        },
        "remove_network" => wire::Request::ChangeNetwork {
            node_id: arg_node(args)?,
            endpoint: arg_endpoint(args)?,
            change: api::NetworkChange::Remove {
                network_id: arg_network(args)?,
            },
        },
        "connect_network" => wire::Request::ChangeNetwork {
            node_id: arg_node(args)?,
            endpoint: arg_endpoint(args)?,
            change: api::NetworkChange::Connect {
                network_id: arg_network(args)?,
            },
        },
        "reorder_network" => wire::Request::ChangeNetwork {
            node_id: arg_node(args)?,
            endpoint: arg_endpoint(args)?,
            change: api::NetworkChange::Reorder {
                network_id: arg_network(args)?,
                index: u8::try_from(arg_u64(args, "index")?)
                    .map_err(|_| (jsonrpc::INVALID_PARAMS, "'index' is too big".to_string()))?,
            },
        },
        "retrieve_logs" => wire::Request::RetrieveLogs {
            node_id: arg_node(args)?,
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
            node_id: arg_node(args)?,
        },
        "wake_device" => wire::Request::WakeDevice {
            node_id: arg_node(args)?,
        },
        "identify" => wire::Request::Identify {
            node_id: arg_node(args)?,
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
            fabric: arg_fabric(args),
        },
        "set_tags" => wire::Request::SetTags {
            node_id: arg_node(args)?,
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

/// The `fabric` argument: a name, an index (`#2`) or `all`; the default
/// fabric without one.
fn arg_fabric(args: &Value) -> FabricRef {
    args.get("fabric")
        .and_then(Value::as_str)
        .map(FabricRef::parse)
        .unwrap_or_default()
}

/// The `fabric` argument, which a tool needs.
fn arg_one_fabric(args: &Value) -> Result<FabricRef, ToolErr> {
    match arg_fabric(args) {
        FabricRef::Default => Err((
            jsonrpc::INVALID_PARAMS,
            "missing 'fabric': its name or index".to_string(),
        )),
        fabric => Ok(fabric),
    }
}

fn arg_u64(args: &Value, key: &str) -> Result<u64, ToolErr> {
    args.get(key).and_then(Value::as_u64).ok_or((
        jsonrpc::INVALID_PARAMS,
        format!("missing or non-integer '{key}'"),
    ))
}

/// The `node_id` argument: a number, or a hex string ("0x1B67B").
fn arg_node(args: &Value) -> Result<u64, ToolErr> {
    let value = args.get("node_id");

    let id = value.and_then(Value::as_u64).or_else(|| {
        let text = value?.as_str()?.trim();
        match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
            Some(hex) => u64::from_str_radix(hex, 16).ok(),
            None => text.parse().ok(),
        }
    });

    id.ok_or((
        jsonrpc::INVALID_PARAMS,
        "missing or bad 'node_id': give the number list_devices gives, or its hex form (\"0x1B67B\")"
            .to_string(),
    ))
}

/// The `endpoint` argument, `0` by default.
fn arg_endpoint(args: &Value) -> Result<u16, ToolErr> {
    match args.get("endpoint").and_then(Value::as_u64) {
        Some(endpoint) => u16::try_from(endpoint)
            .map_err(|_| (jsonrpc::INVALID_PARAMS, "'endpoint' is too big".to_string())),
        None => Ok(0),
    }
}

/// The network ID named by `ssid` or `network_id_hex`.
fn arg_network(args: &Value) -> Result<Vec<u8>, ToolErr> {
    if let Some(ssid) = args.get("ssid").and_then(Value::as_str) {
        return Ok(ssid.as_bytes().to_vec());
    }

    let hex = arg_str(args, "network_id_hex").map_err(|_| {
        (
            jsonrpc::INVALID_PARAMS,
            "name the network by 'ssid' or 'network_id_hex'".to_string(),
        )
    })?;
    let digits = hex.trim_start_matches("0x").as_bytes();

    digits
        .chunks(2)
        .map(|pair| {
            core::str::from_utf8(pair)
                .ok()
                .filter(|pair| pair.len() == 2)
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
        })
        .collect::<Option<Vec<u8>>>()
        .ok_or((jsonrpc::INVALID_PARAMS, format!("'{hex}' is not hex")))
}

/// A cluster, attribute or command given by its id (an integer) or name.
fn arg_id_or_name(args: &Value, key: &str) -> Result<String, ToolErr> {
    args.get(key)
        .map(id_or_name)
        .ok_or((jsonrpc::INVALID_PARAMS, format!("missing '{key}'")))
}

fn id_or_name(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
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
        wire::Response::Error { message } => (message.clone(), true),
        wire::Response::Logs { intent, data, .. } => (log_text(*intent, data), false),
        other => (compact(labelled(other)).to_string(), false),
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
                v["hardware_version"] = json!(d.hardware_version());
                v["software_version"] = json!(d.software_version());
                v["device_type"] = json!(d.device_type().map(display::device_type_label));
            }
        }
        wire::Response::Discovered { ble_devices, .. } => {
            for (d, v) in ble_devices.iter().zip(entries(&mut value, "ble_devices")) {
                v["vendor"] = json!(display::vendor_label(d.vendor_id, None));
                v["product"] = json!(display::product_label(d.vendor_id, d.product_id, None));
            }
        }
        wire::Response::Model { model } => value = model_json(model),
        wire::Response::Events { events } => value = events_json(events),
        wire::Response::CommandOutcome { outcome } => {
            if let Some(fields) = outcome.fields.as_deref() {
                value["outcome"]["fields"] = serde_json::from_str(fields).unwrap_or(json!(fields));
            }
        }
        wire::Response::AdvertisedDevices { devices } => {
            for (d, v) in devices.iter().zip(entries(&mut value, "devices")) {
                v["ours"] = json!(d.ours());
                for (f, v) in d.fabrics.iter().zip(entries(v, "fabrics")) {
                    v["compressed_fabric_id"] = json!(display::hex_u64(f.compressed_fabric_id));
                    v["node_id"] = json!(display::hex_u64(f.node_id));
                    if let Some(vid) = f.vendor_id {
                        v["vendor"] = json!(display::vendor_label(vid, None));
                    }
                }
            }
        }
        wire::Response::Fabrics { fabrics } => {
            for (f, v) in fabrics.iter().zip(entries(&mut value, "fabrics")) {
                v["fabric_id"] = json!(display::hex_u64(f.fabric_id));
                v["compressed_fabric_id"] = json!(display::hex_u64(f.compressed_fabric_id));
                v["node_id"] = json!(display::hex_u64(f.node_id));
                v["vendor"] = json!(display::vendor_label(f.vendor_id, None));
            }
        }
        wire::Response::CommissioningWindow { window } => {
            let v = &mut value["window"];
            v["node"] = json!(display::hex_u64(window.node_id));
            if let Some(code) = &window.manual_code {
                v["manual_code"] = json!(display::pairing_code_label(code));
            }
            if let Some(secs) = window.remaining_secs {
                v["remaining"] = json!(display::duration(secs));
            }
            if let Some(vid) = window.opened_by_vendor {
                v["opened_by"] = json!(display::vendor_label(vid, None));
            }
            // For drawing; the QR code text says the same.
            if let Some(window) = v.as_object_mut() {
                window.remove("qr_rows");
            }
        }
        wire::Response::OtaUpdates { updates } => {
            for (u, v) in updates.iter().zip(entries(&mut value, "updates")) {
                v["node"] = json!(display::hex_u64(u.node_id));
                v["in_progress"] = json!(u.in_progress());
                v["elapsed"] = json!(display::duration(u.elapsed_secs));
                v["since_last_progress"] = json!(display::duration(u.idle_secs));
                if let Some(secs) = u.remaining_secs() {
                    v["remaining_estimate"] = json!(display::duration(secs));
                }
            }
        }
        wire::Response::OtaImages { images } => {
            for (i, v) in images.iter().zip(entries(&mut value, "images")) {
                v["vendor"] = json!(display::vendor_label(i.vendor_id, None));
                v["product"] = json!(display::product_label(i.vendor_id, i.product_id, None));
                v["software_version"] = json!(display::software_version_label(
                    i.version,
                    Some(i.version_str.clone())
                ));
            }
        }
        wire::Response::Network { network, .. } => {
            for (config, c) in network
                .configs
                .iter()
                .zip(entries(&mut value["network"], "configs"))
            {
                for (known, k) in config.networks.iter().zip(entries(c, "networks")) {
                    k["id_hex"] = json!(known
                        .id
                        .iter()
                        .map(|b| format!("{b:02X}"))
                        .collect::<String>());
                }
            }
        }
        wire::Response::ThreadMesh { mesh } => {
            let label = |ext: u64| {
                mesh.nodes
                    .iter()
                    .find(|node| node.ext_address == ext)
                    .map(|node| node.label.clone())
            };
            for (node, v) in mesh.nodes.iter().zip(entries(&mut value["mesh"], "nodes")) {
                v["ext"] = json!(hex64(node.ext_address));
                v["sleepy"] = json!(node.sleepy());
                if let Some(parent) = node.parent {
                    v["parent_label"] = json!(label(parent));
                }
            }
            for (link, v) in mesh.links.iter().zip(entries(&mut value["mesh"], "links")) {
                v["a_label"] = json!(label(link.a));
                v["b_label"] = json!(label(link.b));
            }
            for (route, v) in mesh
                .routes
                .iter()
                .zip(entries(&mut value["mesh"], "routes"))
            {
                v["from_label"] = json!(label(route.from));
                v["to_label"] = json!(label(route.to));
                if let Some(via) = route.via {
                    v["via_label"] = json!(label(via));
                }
            }
            for (node, v) in mesh
                .lan_nodes
                .iter()
                .zip(entries(&mut value["mesh"], "lan_nodes"))
            {
                v["sleepy"] = json!(node.sleepy());
                for (fabric, f) in node.fabrics.iter().zip(entries(v, "fabrics")) {
                    f["fabric"] = json!(hex64(fabric.compressed_fabric_id));
                }
            }
            for (net, v) in mesh
                .networks
                .iter()
                .zip(entries(&mut value["mesh"], "networks"))
            {
                v["xpan"] = json!(hex64(net.extended_pan_id));
            }
        }
        wire::Response::Peek { health, .. } => {
            if let Some(secs) = health.general.as_ref().and_then(|g| g.up_time_secs) {
                value["health"]["general"]["up_time"] = json!(display::duration(secs));
            }
            let fabrics = health.fabrics.iter().flatten();
            for (fabric, v) in fabrics.zip(entries(&mut value["health"], "fabrics")) {
                v["name"] = json!(fabric.name());
                v["vendor"] = json!(display::vendor_label(fabric.vendor_id, None));
                v["fabric"] = json!(display::hex_u64(fabric.fabric_id));
                v["node"] = json!(display::hex_u64(fabric.node_id));
                // Says who issued it, which `ours` and `name` tell more plainly.
                if let Some(fabric) = v.as_object_mut() {
                    fabric.remove("root_key");
                }
            }
        }
        wire::Response::DeviceTime { time } => label_time(time, &mut value["time"]),
        wire::Response::TimeSetup { setup } => label_time(&setup.time, &mut value["setup"]["time"]),
        _ => {}
    }

    value
}

/// A device's clock, with its times, offsets and sources also in words.
fn label_time(time: &api::DeviceTime, v: &mut Value) {
    v["node"] = json!(display::hex_u64(time.node_id));
    if let Some(ms) = time.utc_unix_ms {
        v["utc"] = json!(display::utc_label(ms));
    }
    if let Some(ms) = time.offset_ms {
        v["from_ours"] = json!(display::clock_offset_label(ms));
    }
    v["accuracy"] = json!(time.granularity.label());
    if let Some(source) = time.time_source {
        v["source"] = json!(display::time_source_label(source));
    }
    if let Some(source) = time.trusted_source_label() {
        v["takes_time_from"] = json!(source);
    }
    if let Some(ms) = time.local_unix_ms {
        v["local"] = json!(display::local_time_label(ms));
    }
    for (z, v) in time.time_zones.iter().zip(entries(v, "time_zones")) {
        v["offset"] = json!(display::utc_offset_label(z.offset_secs));
        if let Some(ms) = z.valid_at_unix_ms {
            v["from"] = json!(display::utc_label(ms));
        }
    }
    for (d, v) in time.dst_offsets.iter().zip(entries(v, "dst_offsets")) {
        v["offset"] = json!(display::utc_offset_label(d.offset_secs));
        if let Some(ms) = d.from_unix_ms {
            v["from"] = json!(display::utc_label(ms));
        }
        if let Some(ms) = d.until_unix_ms {
            v["until"] = json!(display::utc_label(ms));
        }
    }
}

/// The entries of array `key` of `value`.
/// Events for an agent: their times readable, their fields as objects.
fn events_json(events: &[api::EventModel]) -> Value {
    let events: Vec<Value> = events
        .iter()
        .map(|e| {
            let time = match (e.utc_ms, e.uptime_ms) {
                (Some(ms), _) => display::utc_label(ms),
                (None, Some(ms)) => format!("{} after start", display::duration(ms / 1000)),
                (None, None) => String::new(),
            };
            json!({
                "number": e.number,
                "priority": e.priority,
                "time": time,
                "endpoint": e.endpoint,
                "cluster": e.cluster_name.clone().unwrap_or_else(|| display::hex_u32(e.cluster)),
                "event": e.name.clone().unwrap_or_else(|| display::hex_u32(e.event)),
                "fields": match (&e.fields, &e.error) {
                    (Some(fields), _) => serde_json::from_str(fields).unwrap_or(Value::Null),
                    (None, Some(error)) => json!({ "error": error }),
                    (None, None) => Value::Null,
                },
            })
        })
        .collect();

    json!({ "events": events })
}

/// A device model for an agent: per endpoint its device types, per cluster
/// its attribute values by name, which are writable, and its commands with
/// their fields' types.
fn model_json(model: &api::DeviceModel) -> Value {
    let endpoints: Vec<Value> = model
        .endpoints
        .iter()
        .map(|e| {
            let clusters: Vec<Value> = e
                .clusters
                .iter()
                .map(|c| {
                    let mut attributes = serde_json::Map::new();
                    let mut writable = Vec::new();
                    for a in c.attributes.iter().filter(|a| a.id < 0xFFF8) {
                        let name = a.name.clone().unwrap_or_else(|| display::hex_u32(a.id));
                        let value = match (&a.value, &a.error) {
                            (Some(v), _) => serde_json::from_str(v).unwrap_or(Value::Null),
                            (None, Some(e)) => json!({ "error": e }),
                            (None, None) => Value::Null,
                        };
                        if a.writable {
                            writable.push(name.clone());
                        }
                        attributes.insert(name, value);
                    }
                    let commands: serde_json::Map<String, Value> = c
                        .commands
                        .iter()
                        .map(|cmd| {
                            let fields: serde_json::Map<String, Value> = cmd
                                .fields
                                .iter()
                                .map(|f| (f.name.clone(), json!(type_text(model, &f.value_type))))
                                .collect();
                            let name = cmd.name.clone().unwrap_or_else(|| display::hex_u32(cmd.id));
                            (name, Value::Object(fields))
                        })
                        .collect();
                    json!({
                        "cluster": c.name.clone().unwrap_or_else(|| display::hex_u32(c.id)),
                        "id": display::hex_u32(c.id),
                        "attributes": attributes,
                        "writable": writable,
                        "commands": commands,
                    })
                })
                .collect();
            let device_types: Vec<String> = e
                .device_types
                .iter()
                .map(|t| display::device_type_label(t.id))
                .collect();
            json!({ "endpoint": e.id, "device_types": device_types, "clusters": clusters })
        })
        .collect();

    json!({ "node": display::hex_u64(model.node_id), "endpoints": endpoints })
}

/// A field's type for an agent: its IDL name, an enum's values, optional and
/// nullable.
fn type_text(model: &api::DeviceModel, ty: &api::ValueType) -> String {
    let mut text = ty.idl.clone();
    if let Some(info) = ty.schema.and_then(|i| model.types.get(i as usize)) {
        if !info.values.is_empty() {
            let names: Vec<&str> = info.values.iter().map(|v| v.name.as_str()).collect();
            text = format!("{text}: {}", names.join("|"));
        }
    }
    if ty.list {
        text = format!("list of {text}");
    }
    if ty.nullable {
        text.push_str(", nullable");
    }
    if ty.optional {
        text.push_str(", optional");
    }
    text
}

fn entries<'a>(value: &'a mut Value, key: &str) -> impl Iterator<Item = &'a mut Value> {
    value
        .get_mut(key)
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
}

/// The result `value` without what an agent need not read: nulls, empty
/// strings and, below its own fields, empty lists or objects. Its own lists
/// stay, though empty: an empty one answers "none".
fn compact(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, value)| (key, prune(value)))
                .filter(|(_, value)| !value.is_null() && value.as_str() != Some(""))
                .collect(),
        ),
        other => prune(other),
    }
}

/// `value` without its nulls, empty strings and empty lists or objects.
fn prune(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, value)| (key, prune(value)))
                .filter(|(_, value)| !is_empty(value))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(prune).collect()),
        other => other,
    }
}

fn is_empty(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(text) => text.is_empty(),
        Value::Array(items) => items.is_empty(),
        Value::Object(map) => map.is_empty(),
        _ => false,
    }
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn node_id_as_number_or_hex() {
        assert_eq!(arg_node(&json!({ "node_id": 112251 })).unwrap(), 112251);
        assert_eq!(arg_node(&json!({ "node_id": "0x1B67B" })).unwrap(), 112251);
        assert_eq!(
            arg_node(&json!({ "node_id": "0x000000000001b67b" })).unwrap(),
            112251
        );
        assert_eq!(arg_node(&json!({ "node_id": "112251" })).unwrap(), 112251);
        assert!(arg_node(&json!({ "node_id": "0xZZ" })).is_err());
        assert!(arg_node(&json!({})).is_err());
    }

    #[test]
    fn compact_drops_what_says_nothing() {
        let value = compact(json!({
            "name": "light",
            "label": "",
            "online": null,
            "devices": [{
                "node": "0x1",
                "serial": "",
                "tags": [],
                "fingerprint": { "vendor_id": null }
            }],
            "count": 0,
            "remembered": false
        }));

        assert_eq!(
            value,
            json!({
                "name": "light",
                "devices": [{ "node": "0x1" }],
                "count": 0,
                "remembered": false
            })
        );
    }

    #[test]
    fn compact_keeps_an_empty_answer() {
        let value = compact(json!({ "result": "devices", "devices": [], "notice": null }));

        assert_eq!(value, json!({ "result": "devices", "devices": [] }));
    }

    #[test]
    fn protocol_version_negotiated() {
        let version = |asked: Value| initialize_result(&asked)["protocolVersion"].clone();

        assert_eq!(
            version(json!({ "protocolVersion": "2024-11-05" })),
            "2024-11-05"
        );
        assert_eq!(
            version(json!({ "protocolVersion": "2025-06-18" })),
            "2025-06-18"
        );
        assert_eq!(
            version(json!({ "protocolVersion": "2099-01-01" })),
            PROTOCOL_VERSIONS[0]
        );
        assert_eq!(version(json!({})), PROTOCOL_VERSIONS[0]);
    }

    #[test]
    fn tools_annotated() {
        let tools = tool_list(Caps { allow_mutate: true });

        for tool in &tools {
            let name = tool["name"].as_str().unwrap();
            let read_only = tool["annotations"]["readOnlyHint"].as_bool().unwrap();
            let mutating = MUTATING_TOOLS.contains(&name);

            assert_eq!(
                read_only,
                !mutating && !ACTING_TOOLS.contains(&name),
                "{name}"
            );
            if mutating {
                assert!(
                    tool["description"].as_str().unwrap().contains("MUTATING"),
                    "{name} does not say it changes things"
                );
            }
        }
    }
}
