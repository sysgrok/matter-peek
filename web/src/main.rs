//! matter-peek web UI (Yew/Wasm).
//!
//! A WS/JSON client over the [`matter_peek_proto`] contract. Phase 1 slice:
//! enter a peer address + passcode, run the full vertical (commission → health
//! → CrashLogs → decommission) on the RPC server, and render streamed step
//! events plus the final result.

use std::collections::HashMap;

use futures::{SinkExt, StreamExt};
use gloo_net::websocket::{futures::WebSocket, Message};
use wasm_bindgen_futures::spawn_local;
use yew::prelude::*;

use matter_peek_proto as proto;

/// Where the RPC server listens (see `matter-peek-rpc`'s default).
const WS_URL: &str = "ws://127.0.0.1:8087";

#[derive(Clone, PartialEq)]
struct StepLine {
    step: proto::VerticalStep,
    status: proto::StepStatus,
}

enum Msg {
    PeerAddr(String),
    Passcode(String),
    Discriminator(String),
    Persistent(bool),
    Run,
    Discover,
    PickDevice(String),
    ListDevices,
    Peek(u64),
    Identify(u64),
    Step(StepLine),
    Done(proto::Response),
    Error(String),
}

struct App {
    peer_addr: String,
    passcode: String,
    /// Optional discriminator filter for discovery (empty = any).
    discriminator: String,
    /// Commission persistently (keep the device on our fabric + in the store)
    /// rather than the throwaway ephemeral fabric.
    persistent: bool,
    running: bool,
    discovering: bool,
    /// A persistent-device op (list / peek / identify) is in flight.
    busy_stored: bool,
    discovered: Vec<proto::DiscoveredDevice>,
    /// The controller's persistently-commissioned devices (from the store).
    stored: Vec<proto::StoredDeviceInfo>,
    steps: Vec<StepLine>,
    result: Option<proto::Response>,
    /// Transient status line for stored-device ops (peek result / identify ack).
    stored_status: Option<String>,
    error: Option<String>,
}

impl Component for App {
    type Message = Msg;
    type Properties = ();

    fn create(_ctx: &Context<Self>) -> Self {
        Self {
            peer_addr: "[::1]:5540".into(),
            passcode: "20202021".into(),
            discriminator: String::new(),
            persistent: false,
            running: false,
            discovering: false,
            busy_stored: false,
            discovered: Vec::new(),
            stored: Vec::new(),
            steps: Vec::new(),
            result: None,
            stored_status: None,
            error: None,
        }
    }

    fn update(&mut self, ctx: &Context<Self>, msg: Msg) -> bool {
        match msg {
            Msg::PeerAddr(v) => {
                self.peer_addr = v;
                false
            }
            Msg::Passcode(v) => {
                self.passcode = v;
                false
            }
            Msg::Discriminator(v) => {
                self.discriminator = v;
                false
            }
            Msg::Persistent(v) => {
                self.persistent = v;
                false
            }
            Msg::Discover => {
                let discriminator = match self.discriminator.trim() {
                    "" => None,
                    s => match s.parse::<u16>() {
                        Ok(d) => Some(d),
                        Err(_) => {
                            self.error = Some("discriminator must be a u16 (or blank)".into());
                            return true;
                        }
                    },
                };
                self.discovering = true;
                self.discovered.clear();
                self.error = None;
                send_request(
                    ctx.link().clone(),
                    proto::Request::Discover {
                        discriminator,
                        max: 16,
                    },
                );
                true
            }
            Msg::PickDevice(addr) => {
                // Fill the peer address from the picked device; the user then
                // hits "Run vertical".
                self.peer_addr = addr;
                true
            }
            Msg::Run => {
                let passcode: u32 = match self.passcode.trim().parse() {
                    Ok(p) => p,
                    Err(_) => {
                        self.error = Some("passcode must be a u32".into());
                        return true;
                    }
                };
                self.running = true;
                self.steps.clear();
                self.result = None;
                self.error = None;

                let policy = if self.persistent {
                    proto::FabricPolicy::Persistent
                } else {
                    proto::FabricPolicy::Ephemeral
                };
                let req = proto::Request::RunVertical {
                    peer_addr: self.peer_addr.trim().to_string(),
                    passcode,
                    policy,
                };
                send_request(ctx.link().clone(), req);
                true
            }
            Msg::ListDevices => {
                self.busy_stored = true;
                self.stored_status = None;
                self.error = None;
                send_request(ctx.link().clone(), proto::Request::ListDevices);
                true
            }
            Msg::Peek(node_id) => {
                self.busy_stored = true;
                self.stored_status = Some(format!("Peeking node 0x{node_id:016x}…"));
                self.error = None;
                send_request(
                    ctx.link().clone(),
                    proto::Request::PeekDevice {
                        node_id,
                        with_logs: false,
                    },
                );
                true
            }
            Msg::Identify(node_id) => {
                self.busy_stored = true;
                self.stored_status = Some(format!("Identifying node 0x{node_id:016x} (blink 3s)…"));
                self.error = None;
                send_request(
                    ctx.link().clone(),
                    proto::Request::Identify { node_id, secs: 3 },
                );
                true
            }
            Msg::Step(line) => {
                self.steps.push(line);
                true
            }
            Msg::Done(resp) => {
                self.running = false;
                self.discovering = false;
                self.busy_stored = false;
                match resp {
                    proto::Response::Discovered { devices } => self.discovered = devices,
                    proto::Response::Devices { devices } => {
                        self.stored_status = Some(format!("{} stored device(s)", devices.len()));
                        self.stored = devices;
                    }
                    proto::Response::Peek {
                        handle,
                        health,
                        crash_log_bytes,
                    } => {
                        self.stored_status = Some(format_peek(&handle, &health, crash_log_bytes));
                    }
                    proto::Response::Ok => {
                        self.stored_status = Some("Identify acknowledged — device should blink.".into());
                    }
                    // A persistent commission populates the store; refresh the list.
                    other @ proto::Response::Vertical(_) => {
                        self.result = Some(other);
                        if self.persistent {
                            self.busy_stored = true;
                            send_request(ctx.link().clone(), proto::Request::ListDevices);
                        }
                    }
                    proto::Response::Error { message } => self.error = Some(message),
                }
                true
            }
            Msg::Error(e) => {
                self.running = false;
                self.busy_stored = false;
                self.error = Some(e);
                true
            }
        }
    }

    fn view(&self, ctx: &Context<Self>) -> Html {
        let link = ctx.link();
        html! {
            <>
                <h1>{ "matter-peek" }</h1>
                <p>{ "Discover a Matter device, then run the diagnostic vertical against it." }</p>

                <div class="row">
                    <label>{ "Discriminator" }</label>
                    <input placeholder="any" value={self.discriminator.clone()}
                        oninput={link.callback(|e: InputEvent| Msg::Discriminator(input_value(e)))} />
                    <button disabled={self.discovering || self.running}
                        onclick={link.callback(|_| Msg::Discover)}>
                        { if self.discovering { "Discovering…" } else { "Discover devices" } }
                    </button>
                </div>

                { self.view_discovered(link) }

                <div class="row">
                    <label>{ "Peer addr" }</label>
                    <input value={self.peer_addr.clone()}
                        oninput={link.callback(|e: InputEvent| Msg::PeerAddr(input_value(e)))} />
                </div>
                <div class="row">
                    <label>{ "Passcode" }</label>
                    <input value={self.passcode.clone()}
                        oninput={link.callback(|e: InputEvent| Msg::Passcode(input_value(e)))} />
                </div>
                <div class="row">
                    <label>{ "Persistent" }</label>
                    <input type="checkbox" checked={self.persistent}
                        onchange={link.callback(|e: Event| Msg::Persistent(checkbox_checked(e)))} />
                    <span class="hint">
                        { "keep the device on our fabric + in the store (else throwaway)" }
                    </span>
                </div>
                <div class="row">
                    <button disabled={self.running}
                        onclick={link.callback(|_| Msg::Run)}>
                        { if self.running { "Running…" } else { "Run vertical" } }
                    </button>
                </div>

                { self.view_steps() }
                { self.view_result() }

                { self.view_stored(link) }

                { for self.error.iter().map(|e| html!{ <p class="err">{ format!("Error: {e}") }</p> }) }
            </>
        }
    }
}

impl App {
    fn view_discovered(&self, link: &html::Scope<Self>) -> Html {
        if self.discovered.is_empty() {
            return html! {};
        }
        html! {
            <div class="steps">
                { for self.discovered.iter().map(|d| {
                    let addr = d.addr.clone();
                    let onclick = link.callback(move |_| Msg::PickDevice(addr.clone()));
                    html! {
                        <div class="step done">
                            { format!("{}  (id={:#018x})  ", d.addr, d.commissionable_id) }
                            <button {onclick}>{ "Use" }</button>
                        </div>
                    }
                }) }
            </div>
        }
    }

    fn view_steps(&self) -> Html {
        if self.steps.is_empty() {
            return html! {};
        }
        html! {
            <div class="steps">
                { for self.steps.iter().map(|s| {
                    let (cls, txt) = match &s.status {
                        proto::StepStatus::Started => ("started", "started".to_string()),
                        proto::StepStatus::Done => ("done", "done".to_string()),
                        proto::StepStatus::Failed { message } => ("failed", format!("failed: {message}")),
                    };
                    html! { <div class={classes!("step", cls)}>{ format!("{:?}: {txt}", s.step) }</div> }
                }) }
            </div>
        }
    }

    fn view_result(&self) -> Html {
        match &self.result {
            Some(proto::Response::Vertical(v)) => html! {
                <div class="result">{ format_result(v) }</div>
            },
            Some(proto::Response::Error { message }) => html! {
                <p class="err">{ format!("Server error: {message}") }</p>
            },
            // Stored-device responses (Devices / Peek / Ok) and Discovered are
            // routed to their own state in `Msg::Done`, not shown here.
            Some(_) | None => html! {},
        }
    }

    /// The persistently-commissioned device list, with per-device Peek / Identify
    /// actions. The basis for the persistent-fabric workflow: commission once
    /// (Persistent), then re-address devices over CASE without re-commissioning.
    fn view_stored(&self, link: &html::Scope<Self>) -> Html {
        html! {
            <div class="stored">
                <div class="row">
                    <h2>{ "Stored devices" }</h2>
                    <button disabled={self.busy_stored}
                        onclick={link.callback(|_| Msg::ListDevices)}>
                        { if self.busy_stored { "Working…" } else { "Refresh" } }
                    </button>
                </div>

                { for self.stored_status.iter().map(|s| html!{
                    <pre class="result">{ s.clone() }</pre>
                }) }

                {
                    if self.stored.is_empty() {
                        html! { <p class="hint">{ "No stored devices. Commission one with \"Persistent\" checked." }</p> }
                    } else {
                        html! {
                            <div class="steps">
                                { for self.stored.iter().map(|d| self.view_stored_one(d, link)) }
                            </div>
                        }
                    }
                }
            </div>
        }
    }

    fn view_stored_one(&self, d: &proto::StoredDeviceInfo, link: &html::Scope<Self>) -> Html {
        let node_id = d.handle.node_id;
        let peek = link.callback(move |_| Msg::Peek(node_id));
        let identify = link.callback(move |_| Msg::Identify(node_id));

        // Prefer a human label, falling back to the stable identifiers.
        let title = if !d.node_label.is_empty() {
            d.node_label.clone()
        } else if !d.unique_id.is_empty() {
            format!("uid {}", d.unique_id)
        } else if !d.serial_number.is_empty() {
            format!("sn {}", d.serial_number)
        } else {
            format!("node 0x{node_id:016x}")
        };

        html! {
            <div class="step done">
                <div>
                    <strong>{ title }</strong>
                    { format!("  (fab {} · node 0x{:016x})", d.handle.fabric_index, node_id) }
                </div>
                {
                    if d.tags.is_empty() {
                        html! {}
                    } else {
                        html! { <div class="hint">{ format!("tags: {}", d.tags.join(", ")) }</div> }
                    }
                }
                <div class="row">
                    <button disabled={self.busy_stored} onclick={peek}>{ "Peek" }</button>
                    <button disabled={self.busy_stored} onclick={identify}>{ "Identify" }</button>
                </div>
            </div>
        }
    }
}

/// One-line-per-field summary of a `PeekDevice` result, for the status pre.
fn format_peek(
    handle: &proto::DeviceHandle,
    health: &proto::DeviceHealth,
    crash_log_bytes: Option<usize>,
) -> String {
    let mut s = format!(
        "peek node 0x{:016x} (fab {})\n",
        handle.node_id, handle.fabric_index
    );
    if let Some(g) = &health.general {
        s.push_str(&format!(
            "general: up_time_secs={:?} reboot_count={:?} total_operational_hours={:?}\n",
            g.up_time_secs, g.reboot_count, g.total_operational_hours
        ));
    }
    if let Some(sw) = &health.software {
        s.push_str(&format!(
            "software: heap_free={:?} heap_used={:?} heap_high_watermark={:?}\n",
            sw.current_heap_free, sw.current_heap_used, sw.current_heap_high_watermark
        ));
    }
    if let Some(n) = crash_log_bytes {
        s.push_str(&format!("crash_log_bytes: {n}\n"));
    }
    s
}

fn format_result(v: &proto::VerticalResult) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "device: fabric_index={} node_id=0x{:016x}\n",
        v.handle.fabric_index, v.handle.node_id
    ));
    if let Some(g) = &v.health.general {
        s.push_str(&format!(
            "general: up_time_secs={:?} reboot_count={:?} total_operational_hours={:?}\n",
            g.up_time_secs, g.reboot_count, g.total_operational_hours
        ));
    }
    if let Some(sw) = &v.health.software {
        s.push_str(&format!(
            "software: heap_free={:?} heap_used={:?} heap_high_watermark={:?}\n",
            sw.current_heap_free, sw.current_heap_used, sw.current_heap_high_watermark
        ));
    }
    s.push_str(&format!("crash_log_bytes: {:?}\n", v.crash_log_bytes));
    s.push_str(&format!("decommissioned: {}\n", v.decommissioned));
    s
}

/// Open a WS, send the request, and pump server frames back as component
/// messages (step events, then the final response) until it completes.
fn send_request(link: html::Scope<App>, request: proto::Request) {
    spawn_local(async move {
        let ws = match WebSocket::open(WS_URL) {
            Ok(ws) => ws,
            Err(e) => {
                link.send_message(Msg::Error(format!("connect failed: {e}")));
                return;
            }
        };
        let (mut write, mut read) = ws.split();

        let req_id: proto::ReqId = 1;
        let client_msg = proto::ClientMsg { req_id, request };
        let json = serde_json::to_string(&client_msg).unwrap();
        if let Err(e) = write.send(Message::Text(json)).await {
            link.send_message(Msg::Error(format!("send failed: {e}")));
            return;
        }

        // `req_id`-keyed routing is trivial here (one in-flight request) but kept
        // explicit to mirror the multi-request server contract.
        let mut _pending: HashMap<proto::ReqId, ()> = HashMap::new();
        _pending.insert(req_id, ());

        while let Some(frame) = read.next().await {
            let text = match frame {
                Ok(Message::Text(t)) => t,
                Ok(Message::Bytes(_)) => continue,
                Err(e) => {
                    link.send_message(Msg::Error(format!("ws error: {e}")));
                    return;
                }
            };
            let server_msg: proto::ServerMsg = match serde_json::from_str(&text) {
                Ok(m) => m,
                Err(e) => {
                    link.send_message(Msg::Error(format!("bad server msg: {e}")));
                    return;
                }
            };
            match server_msg {
                proto::ServerMsg::Event { event } => {
                    let proto::Event::Step { step, status, .. } = event;
                    link.send_message(Msg::Step(StepLine { step, status }));
                }
                proto::ServerMsg::Response { response, .. } => {
                    link.send_message(Msg::Done(response));
                    return;
                }
            }
        }
    });
}

fn input_value(e: InputEvent) -> String {
    use wasm_bindgen::JsCast;
    use web_sys::HtmlInputElement;
    e.target()
        .and_then(|t| t.dyn_into::<HtmlInputElement>().ok())
        .map(|i| i.value())
        .unwrap_or_default()
}

fn checkbox_checked(e: Event) -> bool {
    use wasm_bindgen::JsCast;
    use web_sys::HtmlInputElement;
    e.target()
        .and_then(|t| t.dyn_into::<HtmlInputElement>().ok())
        .map(|i| i.checked())
        .unwrap_or_default()
}

fn main() {
    wasm_logger::init(wasm_logger::Config::default());
    yew::Renderer::<App>::new().render();
}
