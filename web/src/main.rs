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

/// UI state for one live health subscription (per device).
#[derive(Clone, PartialEq)]
struct LiveSub {
    /// The running, merged health view (successive report deltas merged in).
    health: proto::DeviceHealth,
    /// Connection state of this subscription (live / reconnecting).
    conn: SubConn,
    /// How many health updates have arrived over this subscription's lifetime.
    updates: u64,
}

impl Default for LiveSub {
    fn default() -> Self {
        Self {
            health: proto::DeviceHealth::default(),
            // Optimistic until the server confirms `Live`; shows "connecting…".
            conn: SubConn::Reconnecting,
            updates: 0,
        }
    }
}

/// Connection state of a live subscription, mirrored from the wire.
#[derive(Clone, Copy, PartialEq)]
enum SubConn {
    Live { max_int_secs: u16 },
    Reconnecting,
}

/// Which top-level tab is showing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Discover,
    Commission,
    Devices,
}

/// How long a commission may run before the UI auto-cancels it.
const COMMISSION_TIMEOUT_SECS: u32 = 90;

enum Msg {
    SelectTab(Tab),
    PeerAddr(String),
    Passcode(String),
    Discriminator(String),
    Persistent(bool),
    Run,
    /// Proceed with a commission despite the "already commissioned this address"
    /// warning (the user confirmed).
    RunAnyway,
    /// Dismiss the re-commission warning without running.
    DismissRecommission,
    /// User clicked Cancel (or the timeout fired) — request server-side cancel.
    Cancel,
    Discover,
    PickDevice(String),
    ListDevices,
    /// Re-read a stored device's health now (one-shot) and update its card panel.
    Refresh(u64),
    Identify(u64),
    /// Decommission a stored device (RemoveFabric + drop the local record).
    Decommission(u64),
    /// A live health update arrived for a subscribed device (node_id, health).
    /// Deltas are merged into that device's running view.
    HealthReport(u64, proto::DeviceHealth),
    /// A subscription's connection state changed (node_id, state).
    SubState(u64, proto::SubscriptionState),
    /// The persistent subscription WS is open and ready; its writer sink is
    /// attached. Any subscriptions queued before it opened are (re)sent.
    SubWsReady(futures::channel::mpsc::UnboundedSender<String>),
    /// The persistent subscription WS closed (server gone / network) — all live
    /// subscriptions are dropped; the UI shows them as ended.
    SubWsClosed,
    /// Edit the tag-input text for a given device (node_id).
    TagsInput(u64, String),
    /// Persist the edited tags for a given device.
    SaveTags(u64),
    /// Filter the stored list by tag substring.
    TagFilter(String),
    Step(StepLine),
    Done(proto::Response),
    Error(String),
}

struct App {
    tab: Tab,
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
    /// Per-device tag-input text (node_id → comma-separated edit buffer). Seeded
    /// from the device's current tags when the list arrives.
    tag_inputs: HashMap<u64, String>,
    /// Filter the stored list to devices whose tags contain this substring.
    tag_filter: String,
    steps: Vec<StepLine>,
    result: Option<proto::Response>,
    /// Transient status line for stored-device ops (peek result / identify ack).
    stored_status: Option<String>,
    /// Live health subscriptions, keyed by device `node_id`. Multiple devices can
    /// be watched at once, each with its own running panel. An entry exists for as
    /// long as the device is being watched (removed on stop).
    subs: HashMap<u64, LiveSub>,
    /// The persistent WS to the server over which all subscriptions are driven
    /// (Subscribe/Unsubscribe out, HealthReport/SubscriptionState in). Opened
    /// lazily on the first subscribe; a `None` here means no live WS yet. The
    /// value is the sink to its writer task.
    sub_ws: Option<futures::channel::mpsc::UnboundedSender<String>>,
    /// A neutral info notice (e.g. cancellation outcome).
    notice: Option<String>,
    /// The address we most recently commissioned successfully (and kept). Used to
    /// warn before a likely-doomed re-commission of the same (now closed-window)
    /// device.
    last_commissioned_addr: Option<String>,
    /// True while showing the "you just commissioned this — re-commission anyway?"
    /// confirmation, instead of immediately running.
    confirm_recommission: bool,
    error: Option<String>,
}

impl Component for App {
    type Message = Msg;
    type Properties = ();

    fn create(_ctx: &Context<Self>) -> Self {
        Self {
            tab: Tab::Discover,
            peer_addr: "[::1]:5540".into(),
            passcode: "20202021".into(),
            discriminator: String::new(),
            persistent: false,
            running: false,
            discovering: false,
            busy_stored: false,
            discovered: Vec::new(),
            stored: Vec::new(),
            tag_inputs: HashMap::new(),
            tag_filter: String::new(),
            steps: Vec::new(),
            result: None,
            stored_status: None,
            subs: HashMap::new(),
            sub_ws: None,
            notice: None,
            last_commissioned_addr: None,
            confirm_recommission: false,
            error: None,
        }
    }

    fn update(&mut self, ctx: &Context<Self>, msg: Msg) -> bool {
        match msg {
            Msg::SelectTab(tab) => {
                self.tab = tab;
                // Lazily refresh the device list the first time the user opens
                // the Devices tab with an empty list.
                if tab == Tab::Devices && self.stored.is_empty() && !self.busy_stored {
                    self.busy_stored = true;
                    self.stored_status = None;
                    send_request(ctx.link().clone(), proto::Request::ListDevices);
                }
                true
            }
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
                // Fill the peer address from the picked device and jump to the
                // Commission tab so the user can run the vertical.
                self.peer_addr = addr;
                self.tab = Tab::Commission;
                true
            }
            Msg::Run => {
                // Guard: re-commissioning the device we *just* commissioned is
                // almost always a mistake — its commissioning window has closed,
                // so PASE will hang. Warn + require confirmation instead.
                let addr = self.peer_addr.trim().to_string();
                if self.last_commissioned_addr.as_deref() == Some(addr.as_str()) {
                    self.confirm_recommission = true;
                    return true;
                }
                self.start_commission(ctx)
            }
            Msg::RunAnyway => {
                self.confirm_recommission = false;
                self.start_commission(ctx)
            }
            Msg::DismissRecommission => {
                self.confirm_recommission = false;
                true
            }
            Msg::Cancel => {
                // Only meaningful while a vertical is in flight.
                if !self.running {
                    return false;
                }
                // Fire-and-forget the cancel on its own WS connection; the
                // running vertical replies `Cancelled` on its original one.
                send_cancel();
                true
            }
            Msg::ListDevices => {
                self.busy_stored = true;
                self.stored_status = None;
                self.error = None;
                send_request(ctx.link().clone(), proto::Request::ListDevices);
                true
            }
            Msg::Refresh(node_id) => {
                // One-shot re-read — the result updates this device's card panel
                // (not a status line). Useful for the values the subscription
                // doesn't push on every change (e.g. the live-computed uptime).
                self.busy_stored = true;
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
            Msg::Decommission(node_id) => {
                self.busy_stored = true;
                self.stored_status = Some(format!(
                    "Decommissioning node 0x{node_id:016x} (RemoveFabric)…"
                ));
                self.error = None;
                // Stop watching it locally (the server tears the subscription down too).
                self.subs.remove(&node_id);
                send_request(
                    ctx.link().clone(),
                    proto::Request::Decommission {
                        node_id,
                        force: false,
                    },
                );
                true
            }
            Msg::HealthReport(node_id, health) => {
                // Merge into that device's running view (ignore if not watching).
                if let Some(sub) = self.subs.get_mut(&node_id) {
                    sub.updates += 1;
                    let prev = core::mem::take(&mut sub.health);
                    sub.health = merge_health(prev, health);
                    true
                } else {
                    false
                }
            }
            Msg::SubState(node_id, state) => {
                match state {
                    proto::SubscriptionState::Live { max_int_secs } => {
                        if let Some(sub) = self.subs.get_mut(&node_id) {
                            sub.conn = SubConn::Live { max_int_secs };
                        }
                    }
                    proto::SubscriptionState::Reconnecting => {
                        if let Some(sub) = self.subs.get_mut(&node_id) {
                            sub.conn = SubConn::Reconnecting;
                        }
                    }
                    // Terminal. If the device is still stored we want it watched,
                    // so mark it reconnecting and re-subscribe; only drop the panel
                    // if the device is gone from the store.
                    proto::SubscriptionState::Stopped => {
                        let still_stored = self.stored.iter().any(|d| d.handle.node_id == node_id);
                        if still_stored {
                            if let Some(sub) = self.subs.get_mut(&node_id) {
                                sub.conn = SubConn::Reconnecting;
                            }
                            self.sub_send(&proto::Request::Subscribe { node_id });
                        } else {
                            self.subs.remove(&node_id);
                        }
                    }
                }
                true
            }
            Msg::SubWsReady(tx) => {
                self.sub_ws = Some(tx);
                // (Re)send Subscribe for every device we intend to watch — covers
                // the initial open and any reconnect.
                let nodes: Vec<u64> = self.subs.keys().copied().collect();
                for node_id in nodes {
                    self.sub_send(&proto::Request::Subscribe { node_id });
                }
                true
            }
            Msg::SubWsClosed => {
                self.sub_ws = None;
                // The connection dropped → every subscription is dead. Mark them
                // reconnecting and reopen the WS (which re-subscribes on ready), so
                // the dashboard self-heals without user action.
                for sub in self.subs.values_mut() {
                    sub.conn = SubConn::Reconnecting;
                }
                if !self.subs.is_empty() {
                    self.ensure_sub_ws(ctx);
                }
                true
            }
            Msg::TagsInput(node_id, text) => {
                self.tag_inputs.insert(node_id, text);
                false
            }
            Msg::TagFilter(text) => {
                self.tag_filter = text;
                true
            }
            Msg::SaveTags(node_id) => {
                let tags = self
                    .tag_inputs
                    .get(&node_id)
                    .map(|s| {
                        s.split(',')
                            .map(|t| t.trim().to_string())
                            .filter(|t| !t.is_empty())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                self.busy_stored = true;
                self.stored_status = Some(format!("Saving tags for node 0x{node_id:016x}…"));
                self.error = None;
                send_request(
                    ctx.link().clone(),
                    proto::Request::SetTags { node_id, tags },
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
                        // Seed each device's tag-edit buffer from its (normalized)
                        // stored tags, so the input reflects server truth.
                        self.tag_inputs = devices
                            .iter()
                            .map(|d| (d.handle.node_id, d.tags.join(", ")))
                            .collect();
                        self.stored = devices;
                        // A dashboard watches everything: auto-subscribe every
                        // stored device (and drop panels for any that vanished).
                        self.sync_subscriptions(ctx);
                    }
                    proto::Response::Peek { handle, health, .. } => {
                        // A Refresh result: merge the fresh read into the device's
                        // card panel so its values (uptime, heap, …) update in place.
                        let sub = self.subs.entry(handle.node_id).or_default();
                        let prev = core::mem::take(&mut sub.health);
                        sub.health = merge_health(prev, health);
                        sub.updates += 1;
                    }
                    proto::Response::Ok => {
                        self.stored_status =
                            Some("Identify acknowledged — device should blink.".into());
                    }
                    // `Subscribed` / streamed subscription frames arrive on the
                    // dedicated persistent subscription WS, not this one-shot path.
                    proto::Response::Subscribed { .. } => {}
                    proto::Response::Cancelled { decommissioned } => {
                        self.notice = Some(if decommissioned {
                            "Commission cancelled — the device had been added to our fabric and was cleaned up (decommissioned).".into()
                        } else {
                            "Commission cancelled before the device was modified.".into()
                        });
                    }
                    // A persistent commission populates the store; refresh the
                    // list and surface it on the Devices tab.
                    other @ proto::Response::Vertical(_) => {
                        // A persistent (kept) device's commissioning window is now
                        // closed — remember its address to guard a re-commission.
                        if self.persistent {
                            if let proto::Response::Vertical(v) = &other {
                                if !v.decommissioned {
                                    self.last_commissioned_addr =
                                        Some(self.peer_addr.trim().to_string());
                                }
                            }
                        }
                        self.result = Some(other);
                        if self.persistent {
                            self.busy_stored = true;
                            send_request(ctx.link().clone(), proto::Request::ListDevices);
                            self.tab = Tab::Devices;
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
                <section class="hero is-primary is-small">
                    <div class="hero-body">
                        <p class="title">{ "matter-peek" }</p>
                        <p class="subtitle">
                            { "The Matter diagnostics tool" }
                        </p>
                    </div>
                </section>

                <main class="app container section">
                    { self.view_tabs(link) }
                    { for self.notice.iter().map(|n| html!{
                        <div class="notification is-warning is-light">{ n.clone() }</div>
                    }) }
                    { for self.error.iter().map(|e| html!{
                        <div class="notification is-danger is-light">
                            <strong>{ "Error: " }</strong>{ e.clone() }
                        </div>
                    }) }

                    {
                        match self.tab {
                            Tab::Discover => self.view_discover_tab(link),
                            Tab::Commission => self.view_commission_tab(link),
                            Tab::Devices => self.view_devices_tab(link),
                        }
                    }
                </main>
            </>
        }
    }
}

impl App {
    /// Send a request over the **persistent subscription WS** (Subscribe /
    /// Unsubscribe). No-op if the WS isn't open yet — `SubWsReady` re-sends the
    /// active subscriptions once it opens.
    fn sub_send(&self, request: &proto::Request) {
        if let Some(tx) = &self.sub_ws {
            let msg = proto::ClientMsg {
                req_id: 0,
                request: request.clone(),
            };
            if let Ok(json) = serde_json::to_string(&msg) {
                let _ = tx.unbounded_send(json);
            }
        }
    }

    /// Open the persistent subscription WS if it isn't already, spawning its
    /// reader (server frames → `Msg`) and writer (outbound requests) tasks. All
    /// live subscriptions share this one connection; it stays open while any
    /// device is being watched. The server ties each subscription to this
    /// connection, so closing it stops them all server-side too.
    fn ensure_sub_ws(&mut self, ctx: &Context<Self>) {
        if self.sub_ws.is_some() {
            return;
        }
        let link = ctx.link().clone();
        spawn_local(async move {
            let ws = match WebSocket::open(WS_URL) {
                Ok(ws) => ws,
                Err(e) => {
                    link.send_message(Msg::Error(format!("subscription connect failed: {e}")));
                    return;
                }
            };
            let (mut write, mut read) = ws.split();

            // Outbound: a channel the UI writes request-JSON into; drain to the WS.
            let (out_tx, mut out_rx) = futures::channel::mpsc::unbounded::<String>();
            link.send_message(Msg::SubWsReady(out_tx));

            let writer = async {
                while let Some(json) = out_rx.next().await {
                    if write.send(Message::Text(json)).await.is_err() {
                        break;
                    }
                }
            };

            // Inbound: parse server frames, route the subscription events to `Msg`.
            let reader = async {
                while let Some(frame) = read.next().await {
                    let text = match frame {
                        Ok(Message::Text(t)) => t,
                        Ok(Message::Bytes(_)) => continue,
                        Err(_) => break,
                    };
                    let Ok(msg) = serde_json::from_str::<proto::ServerMsg>(&text) else {
                        continue;
                    };
                    match msg {
                        proto::ServerMsg::Event { event } => match event {
                            proto::Event::HealthReport {
                                node_id, health, ..
                            } => link.send_message(Msg::HealthReport(node_id, health)),
                            proto::Event::SubscriptionState { node_id, state } => {
                                link.send_message(Msg::SubState(node_id, state))
                            }
                            proto::Event::Step { .. } => {}
                        },
                        // `Subscribed` acks arrive here; the panel already exists,
                        // so nothing to do — the streamed events drive the UI.
                        proto::ServerMsg::Response { .. } => {}
                    }
                }
            };

            // Run until either half ends (connection closed), then tell the UI.
            futures::pin_mut!(reader, writer);
            futures::future::select(reader, writer).await;
            link.send_message(Msg::SubWsClosed);
        });
    }

    /// Reconcile live subscriptions with the current stored-device list: watch
    /// every stored device (a dashboard is live by default), and drop any panel
    /// for a device that's no longer stored. Opens the persistent WS if needed.
    fn sync_subscriptions(&mut self, ctx: &Context<Self>) {
        let stored_ids: Vec<u64> = self.stored.iter().map(|d| d.handle.node_id).collect();
        if stored_ids.is_empty() {
            return;
        }

        self.ensure_sub_ws(ctx);

        // Subscribe to any stored device we're not already watching.
        for node_id in &stored_ids {
            if !self.subs.contains_key(node_id) {
                self.subs.insert(*node_id, LiveSub::default());
                self.sub_send(&proto::Request::Subscribe { node_id: *node_id });
            }
        }
        // Drop subscriptions for devices no longer in the store.
        let gone: Vec<u64> = self
            .subs
            .keys()
            .copied()
            .filter(|id| !stored_ids.contains(id))
            .collect();
        for node_id in gone {
            self.sub_send(&proto::Request::Unsubscribe { node_id });
            self.subs.remove(&node_id);
        }
    }

    /// Kick off the commission vertical (shared by `Run` and `RunAnyway`).
    fn start_commission(&mut self, ctx: &Context<Self>) -> bool {
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
        self.notice = None;
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

        // Safety net: auto-cancel if the vertical runs too long. When the timer
        // fires we only act if still `running` (a completed run cleared it).
        let link = ctx.link().clone();
        spawn_local(async move {
            gloo_timers::future::TimeoutFuture::new(COMMISSION_TIMEOUT_SECS * 1000).await;
            link.send_message(Msg::Cancel);
        });
        true
    }

    /// The top-level Bulma tab bar.
    fn view_tabs(&self, link: &html::Scope<Self>) -> Html {
        let tab = |t: Tab, label: &str| {
            let active = self.tab == t;
            let onclick = link.callback(move |_| Msg::SelectTab(t));
            html! {
                <li class={classes!(active.then_some("is-active"))}>
                    <a {onclick}>{ label.to_string() }</a>
                </li>
            }
        };
        html! {
            <div class="tabs is-boxed is-medium">
                <ul>
                    { tab(Tab::Discover, "Discover") }
                    { tab(Tab::Commission, "Commission") }
                    { tab(Tab::Devices, "Devices") }
                </ul>
            </div>
        }
    }

    // --- Discover tab -----------------------------------------------------

    fn view_discover_tab(&self, link: &html::Scope<Self>) -> Html {
        html! {
            <div class="box">
                <h2 class="title is-5">{ "Find commissionable devices" }</h2>
                <div class="field is-grouped">
                    <div class="control">
                        <input class="input" placeholder="discriminator (any)"
                            value={self.discriminator.clone()}
                            oninput={link.callback(|e: InputEvent| Msg::Discriminator(input_value(e)))} />
                    </div>
                    <div class="control">
                        <button class={classes!("button", "is-primary", self.discovering.then_some("is-loading"))}
                            disabled={self.discovering || self.running}
                            onclick={link.callback(|_| Msg::Discover)}>
                            { "Discover" }
                        </button>
                    </div>
                </div>

                { self.view_discovered(link) }
            </div>
        }
    }

    fn view_discovered(&self, link: &html::Scope<Self>) -> Html {
        if self.discovered.is_empty() {
            return html! {
                <p class="has-text-grey">{ "No devices found yet. Click Discover to scan the local network." }</p>
            };
        }
        html! {
            <table class="table is-fullwidth is-hoverable">
                <thead>
                    <tr><th>{ "Address" }</th><th>{ "Commissionable id" }</th><th></th></tr>
                </thead>
                <tbody>
                    { for self.discovered.iter().map(|d| {
                        let addr = d.addr.clone();
                        let onclick = link.callback(move |_| Msg::PickDevice(addr.clone()));
                        html! {
                            <tr>
                                <td class="mono">{ d.addr.clone() }</td>
                                <td class="mono">{ format!("{:#018x}", d.commissionable_id) }</td>
                                <td class="has-text-right">
                                    <button class="button is-small is-link is-light" {onclick}>{ "Use" }</button>
                                </td>
                            </tr>
                        }
                    }) }
                </tbody>
            </table>
        }
    }

    // --- Commission tab ---------------------------------------------------

    fn view_commission_tab(&self, link: &html::Scope<Self>) -> Html {
        html! {
            <>
                { self.view_recommission_warning(link) }
                <div class="box">
                    <h2 class="title is-5">{ "Commission a device" }</h2>

                    <div class="field">
                        <label class="label">{ "Peer address" }</label>
                        <div class="control">
                            <input class="input mono" value={self.peer_addr.clone()}
                                oninput={link.callback(|e: InputEvent| Msg::PeerAddr(input_value(e)))} />
                        </div>
                    </div>

                    <div class="field">
                        <label class="label">{ "Passcode" }</label>
                        <div class="control">
                            <input class="input mono" value={self.passcode.clone()}
                                oninput={link.callback(|e: InputEvent| Msg::Passcode(input_value(e)))} />
                        </div>
                    </div>

                    <div class="field">
                        <label class="checkbox">
                            <input type="checkbox" checked={self.persistent}
                                onchange={link.callback(|e: Event| Msg::Persistent(checkbox_checked(e)))} />
                            { " Persistent" }
                        </label>
                        <p class="help">
                            { "Keep the device on our fabric + in the store (else commission on a throwaway fabric and self-destruct)." }
                        </p>
                    </div>

                    <div class="field">
                        <div class="field is-grouped">
                            <div class="control">
                                <button class={classes!("button", "is-primary", self.running.then_some("is-loading"))}
                                    disabled={self.running}
                                    onclick={link.callback(|_| Msg::Run)}>
                                    { "Run vertical" }
                                </button>
                            </div>
                            {
                                if self.running {
                                    html! {
                                        <div class="control">
                                            <button class="button is-danger is-light"
                                                onclick={link.callback(|_| Msg::Cancel)}>
                                                { "Cancel" }
                                            </button>
                                        </div>
                                    }
                                } else { html! {} }
                            }
                        </div>
                    </div>
                </div>

                { self.view_progress() }
                { self.view_result() }
            </>
        }
    }

    /// The "you just commissioned this device" guard: re-commissioning a device
    /// whose window has closed will hang, so steer the user to Peek/Identify or
    /// require an explicit "Commission anyway".
    fn view_recommission_warning(&self, link: &html::Scope<Self>) -> Html {
        if !self.confirm_recommission {
            return html! {};
        }
        html! {
            <div class="notification is-warning">
                <p class="mb-3">
                    <strong>{ "This device was just commissioned." }</strong>
                    { " Its commissioning window has most likely closed, so commissioning \
                       it again will hang. To inspect it, use " }
                    <strong>{ "Peek" }</strong>{ " or " }<strong>{ "Identify" }</strong>
                    { " on the Devices tab. To re-add it, first put the device back into \
                       pairing mode." }
                </p>
                <div class="buttons">
                    <button class="button is-link"
                        onclick={link.callback(|_| Msg::SelectTab(Tab::Devices))}>
                        { "Go to Devices" }
                    </button>
                    <button class="button is-warning is-light"
                        onclick={link.callback(|_| Msg::RunAnyway)}>
                        { "Commission anyway" }
                    </button>
                    <button class="button is-text"
                        onclick={link.callback(|_| Msg::DismissRecommission)}>
                        { "Dismiss" }
                    </button>
                </div>
            </div>
        }
    }

    /// A real progress bar with the active-phase label behind it. The vertical
    /// emits a `Step` per phase; we count finished phases for the bar value and
    /// name the in-flight phase.
    fn view_progress(&self) -> Html {
        if self.steps.is_empty() {
            return html! {};
        }

        // Total phases: ephemeral runs 4 (incl. Decommission), persistent 3.
        let total: u32 = if self.persistent { 3 } else { 4 };
        let done = self
            .steps
            .iter()
            .filter(|s| s.status == proto::StepStatus::Done)
            .count() as u32;
        let failed = self
            .steps
            .iter()
            .any(|s| matches!(s.status, proto::StepStatus::Failed { .. }));

        // The phase label: the last step's name + state.
        let (label, bar_class) = match self.steps.last() {
            Some(s) => {
                let phase = phase_name(s.step);
                match &s.status {
                    proto::StepStatus::Started => (format!("{phase}…"), "is-primary"),
                    proto::StepStatus::Done if done >= total && !self.running => {
                        ("Done".to_string(), "is-success")
                    }
                    proto::StepStatus::Done => (format!("{phase} done"), "is-primary"),
                    proto::StepStatus::Failed { message } => {
                        (format!("{phase} failed: {message}"), "is-danger")
                    }
                }
            }
            None => ("Working…".to_string(), "is-primary"),
        };
        let bar_class = if failed { "is-danger" } else { bar_class };
        let value = done.min(total);

        html! {
            <div class="box">
                <div class="level is-mobile mb-2">
                    <div class="level-left"><span class="has-text-weight-medium">{ label }</span></div>
                    <div class="level-right"><span class="has-text-grey is-size-7">{ format!("{value}/{total}") }</span></div>
                </div>
                <progress class={classes!("progress", bar_class)}
                    value={value.to_string()} max={total.to_string()}>
                    { format!("{value}/{total}") }
                </progress>
            </div>
        }
    }

    fn view_result(&self) -> Html {
        match &self.result {
            Some(proto::Response::Vertical(v)) => html! {
                <div class="box">
                    <h3 class="title is-6">{ "Result" }</h3>
                    { for v.rematch_candidate.map(|node_id| html!{
                        <div class="notification is-info is-light">
                            { format!("↳ Possible match: same model as stored device 0x{node_id:016x} — open Devices to confirm/link.") }
                        </div>
                    }) }
                    <pre class="result">{ format_result(v) }</pre>
                </div>
            },
            Some(proto::Response::Error { message }) => html! {
                <div class="notification is-danger is-light">{ format!("Server error: {message}") }</div>
            },
            // Stored-device responses (Devices / Peek / Ok) and Discovered are
            // routed to their own state in `Msg::Done`, not shown here.
            Some(_) | None => html! {},
        }
    }

    // --- Devices tab ------------------------------------------------------

    /// The persistently-commissioned device list, with per-device Peek / Identify
    /// actions. The basis for the persistent-fabric workflow: commission once
    /// (Persistent), then re-address devices over CASE without re-commissioning.
    fn view_devices_tab(&self, link: &html::Scope<Self>) -> Html {
        let filter = self.tag_filter.trim().to_lowercase();
        let visible: Vec<&proto::StoredDeviceInfo> = self
            .stored
            .iter()
            .filter(|d| {
                filter.is_empty() || d.tags.iter().any(|t| t.to_lowercase().contains(&filter))
            })
            .collect();

        html! {
            <div class="box">
                <div class="level is-mobile">
                    <div class="level-left">
                        <h2 class="title is-5">{ "Stored devices" }</h2>
                    </div>
                    <div class="level-right">
                        <button class={classes!("button", "is-small", self.busy_stored.then_some("is-loading"))}
                            disabled={self.busy_stored}
                            onclick={link.callback(|_| Msg::ListDevices)}>
                            { "Refresh" }
                        </button>
                    </div>
                </div>

                {
                    if self.stored.is_empty() {
                        html! {}
                    } else {
                        html! {
                            <div class="field">
                                <div class="control has-icons-left">
                                    <input class="input" placeholder="filter by tag…"
                                        value={self.tag_filter.clone()}
                                        oninput={link.callback(|e: InputEvent| Msg::TagFilter(input_value(e)))} />
                                    <span class="icon is-small is-left">{ "🔎" }</span>
                                </div>
                            </div>
                        }
                    }
                }

                { for self.stored_status.iter().map(|s| html!{
                    <div class="notification is-light"><pre class="result">{ s.clone() }</pre></div>
                }) }

                {
                    if self.stored.is_empty() {
                        html! {
                            <p class="has-text-grey">
                                { "No stored devices yet. Commission one with \"Persistent\" checked." }
                            </p>
                        }
                    } else if visible.is_empty() {
                        html! {
                            <p class="has-text-grey">
                                { format!("No stored devices tagged like \"{}\".", self.tag_filter.trim()) }
                            </p>
                        }
                    } else {
                        html! { <>{ for visible.into_iter().map(|d| self.view_device_card(d, link)) }</> }
                    }
                }
            </div>
        }
    }

    fn view_device_card(&self, d: &proto::StoredDeviceInfo, link: &html::Scope<Self>) -> Html {
        let node_id = d.handle.node_id;
        let refresh = link.callback(move |_| Msg::Refresh(node_id));
        let identify = link.callback(move |_| Msg::Identify(node_id));
        let decommission = link.callback(move |_| Msg::Decommission(node_id));
        let save_tags = link.callback(move |_| Msg::SaveTags(node_id));
        let on_tags = link.callback(move |e: InputEvent| Msg::TagsInput(node_id, input_value(e)));

        // This device's live subscription (auto-created; `None` only very briefly).
        let live = self.subs.get(&node_id);

        // Prefer a human label: the user-set NodeLabel, else the device's own
        // ProductName, else the stable identifiers, else the raw node id.
        let title = if !d.node_label.is_empty() {
            d.node_label.clone()
        } else if !d.product_name.is_empty() {
            d.product_name.clone()
        } else if !d.unique_id.is_empty() {
            format!("uid {}", d.unique_id)
        } else if !d.serial_number.is_empty() {
            format!("sn {}", d.serial_number)
        } else {
            format!("node 0x{node_id:016x}")
        };

        let tag_text = self
            .tag_inputs
            .get(&node_id)
            .cloned()
            .unwrap_or_else(|| d.tags.join(", "));

        html! {
            <div class="card mb-4">
                <div class="card-content">
                    <div class="level is-mobile mb-2">
                        <div class="level-left">
                            <p class="title is-6 mb-0">{ title }</p>
                        </div>
                        <div class="level-right">
                            <span class="tag is-light mono">
                                { format!("fab {} · 0x{:016x}", d.handle.fabric_index, node_id) }
                            </span>
                        </div>
                    </div>

                    <div class="mb-3">
                        <p class="is-size-7 mb-0">
                            <span class="has-text-weight-semibold">{ d.vendor_display.clone() }</span>
                            { if d.is_test_vendor {
                                html!{ <span class="tag is-warning is-light is-rounded ml-2">{ "TEST" }</span> }
                              } else { html!{} } }
                            { if !d.product_name.is_empty() {
                                html!{ <span class="has-text-grey">{ format!(" · {}", d.product_name) }</span> }
                              } else { html!{} } }
                        </p>
                        <p class="is-size-7 has-text-grey mono">{ fingerprint_line(&d.fingerprint) }</p>
                    </div>

                    {
                        if d.tags.is_empty() {
                            html! {}
                        } else {
                            html! {
                                <div class="tags mb-2">
                                    { for d.tags.iter().map(|t| html!{
                                        <span class="tag is-info is-light">{ t.clone() }</span>
                                    }) }
                                </div>
                            }
                        }
                    }

                    <div class="field has-addons mb-2">
                        <div class="control is-expanded">
                            <input class="input is-small" placeholder="comma,separated tags"
                                value={tag_text} oninput={on_tags} />
                        </div>
                        <div class="control">
                            <button class="button is-small" disabled={self.busy_stored} onclick={save_tags}>
                                { "Save tags" }
                            </button>
                        </div>
                    </div>

                    <div class="buttons are-small">
                        <button
                            class={classes!("button", "is-link", "is-light", self.busy_stored.then_some("is-loading"))}
                            disabled={self.busy_stored} onclick={refresh}>
                            { "Refresh" }
                        </button>
                        <button class="button is-warning is-light" disabled={self.busy_stored} onclick={identify}>
                            { "Identify" }
                        </button>
                        <button class="button is-danger is-light" disabled={self.busy_stored} onclick={decommission}>
                            { "Decommission" }
                        </button>
                    </div>

                    // Every stored device is watched automatically — the panel is
                    // always shown (it renders "connecting…" until the first report).
                    { self.view_live_health(live) }
                </div>
            </div>
        }
    }

    /// The live-health panel shown under every device card: a small grid of the
    /// diagnostic metrics from the merged running view, plus a live/reconnecting
    /// status and the update count. `None` while the subscription is still being
    /// set up (shown as connecting).
    fn view_live_health(&self, sub: Option<&LiveSub>) -> Html {
        let default = LiveSub::default();
        let sub = sub.unwrap_or(&default);
        // Each metric as a labelled cell; `—` when the device doesn't report it.
        let metric = |label: &str, value: String| -> Html {
            html! {
                <div class="level-item has-text-centered">
                    <div>
                        <p class="heading">{ label.to_string() }</p>
                        <p class="title is-6 mono">{ value }</p>
                    </div>
                </div>
            }
        };
        let g = sub.health.general.unwrap_or_default();
        let sw = sub.health.software.unwrap_or_default();

        let (status, status_class) = match sub.conn {
            SubConn::Live { max_int_secs } => (
                format!("● live · {} update(s) · ≤{max_int_secs}s", sub.updates),
                "tag is-success is-light",
            ),
            SubConn::Reconnecting => (
                format!("◌ reconnecting… · {} update(s)", sub.updates),
                "tag is-warning is-light",
            ),
        };

        html! {
            <div class="box mt-2 p-3">
                <div class="level is-mobile mb-2">
                    <div class="level-left">
                        <p class="heading mb-0">{ "Live health" }</p>
                    </div>
                    <div class="level-right">
                        <span class={status_class}>{ status }</span>
                    </div>
                </div>
                <nav class="level is-mobile">
                    { metric("Uptime (s)", opt_str(g.up_time_secs)) }
                    { metric("Reboots", opt_str(g.reboot_count)) }
                    { metric("Op. hours", opt_str(g.total_operational_hours)) }
                    { metric("Heap free", opt_str(sw.current_heap_free)) }
                    { metric("Heap used", opt_str(sw.current_heap_used)) }
                    { metric("Heap peak", opt_str(sw.current_heap_high_watermark)) }
                </nav>
            </div>
        }
    }
}

/// A metric value or an em-dash placeholder when the device doesn't report it.
fn opt_str<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map(|x| x.to_string()).unwrap_or_else(|| "—".into())
}

/// Merge a live health delta into the running view: a subscription report may
/// carry only the attributes that changed, so we overlay the new report's
/// present fields onto the previous snapshot rather than replacing it wholesale.
fn merge_health(mut prev: proto::DeviceHealth, delta: proto::DeviceHealth) -> proto::DeviceHealth {
    if let Some(g) = delta.general {
        let base = prev.general.get_or_insert_with(Default::default);
        if g.up_time_secs.is_some() {
            base.up_time_secs = g.up_time_secs;
        }
        if g.reboot_count.is_some() {
            base.reboot_count = g.reboot_count;
        }
        if g.total_operational_hours.is_some() {
            base.total_operational_hours = g.total_operational_hours;
        }
    }
    if let Some(s) = delta.software {
        let base = prev.software.get_or_insert_with(Default::default);
        if s.current_heap_free.is_some() {
            base.current_heap_free = s.current_heap_free;
        }
        if s.current_heap_used.is_some() {
            base.current_heap_used = s.current_heap_used;
        }
        if s.current_heap_high_watermark.is_some() {
            base.current_heap_high_watermark = s.current_heap_high_watermark;
        }
    }
    prev
}

/// Compact one-line fingerprint summary for a stored-device card.
fn fingerprint_line(fp: &proto::Fingerprint) -> String {
    fn dec<T: core::fmt::Display>(v: Option<T>) -> String {
        v.map(|x| x.to_string()).unwrap_or_else(|| "—".to_string())
    }
    // vid/pid in hex (how they're written in the CSA registry / on data sheets);
    // hw/sw versions in decimal.
    let hex16 = |v: Option<u16>| {
        v.map(|x| format!("0x{x:04X}"))
            .unwrap_or_else(|| "—".into())
    };
    format!(
        "vid:{} pid:{} hw:{} sw:{}",
        hex16(fp.vendor_id),
        hex16(fp.product_id),
        dec(fp.hardware_version),
        dec(fp.software_version),
    )
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
    if let Some(node_id) = v.rematch_candidate {
        s.push_str(&format!(
            "↳ possible match: same model as stored device 0x{node_id:016x} — \
             check Stored devices to confirm/link\n"
        ));
    }
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
                proto::ServerMsg::Event { event } => match event {
                    proto::Event::Step { step, status, .. } => {
                        link.send_message(Msg::Step(StepLine { step, status }));
                    }
                    // Subscription events flow on the dedicated persistent WS, not
                    // this one-shot connection — ignore any that land here.
                    proto::Event::HealthReport { .. } | proto::Event::SubscriptionState { .. } => {}
                },
                proto::ServerMsg::Response { response, .. } => {
                    link.send_message(Msg::Done(response));
                    return;
                }
            }
        }
    });
}

/// Fire-and-forget a `Cancel` on its own short-lived WS connection. The running
/// vertical (on its original connection) replies `Cancelled` when it next polls
/// the server's cancel flag at a safepoint.
fn send_cancel() {
    spawn_local(async move {
        let Ok(ws) = WebSocket::open(WS_URL) else {
            return;
        };
        let (mut write, _read) = ws.split();
        let client_msg = proto::ClientMsg {
            req_id: 1,
            request: proto::Request::Cancel,
        };
        if let Ok(json) = serde_json::to_string(&client_msg) {
            let _ = write.send(Message::Text(json)).await;
        }
    });
}

/// Human-readable name for a vertical phase.
fn phase_name(step: proto::VerticalStep) -> &'static str {
    match step {
        proto::VerticalStep::Commission => "Commissioning",
        proto::VerticalStep::ReadHealth => "Reading health",
        proto::VerticalStep::RetrieveLogs => "Retrieving logs",
        proto::VerticalStep::Decommission => "Decommissioning",
    }
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
