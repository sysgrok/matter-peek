//! matter-peek web UI (Yew/Wasm).
//!
//! A WS/JSON client over the [`matter_peek_proto`] contract: the devices on our
//! fabric with their live health, logs and updates; commissioning (with mDNS
//! discovery); and the OTA images.

use std::collections::HashMap;
use std::net::SocketAddr;

use futures::{SinkExt, StreamExt};
use gloo_net::websocket::{futures::WebSocket, Message};
use wasm_bindgen_futures::spawn_local;
use yew::prelude::*;

use matter_peek_proto as proto;

/// Where the RPC server listens (see `matter-peek-rpc`'s default).
const WS_URL: &str = "ws://127.0.0.1:8087";

#[derive(Clone, PartialEq)]
struct StepLine {
    step: proto::CommissionStep,
    status: proto::StepStatus,
}

/// UI state for one live health subscription (per device).
#[derive(Clone, PartialEq)]
struct LiveSub {
    /// The running, merged health view (successive report deltas merged in).
    health: proto::DeviceHealth,
    /// Connection state of this subscription (live / offline / …).
    conn: SubConn,
    /// How many reports the device's subscription has delivered.
    updates: u64,
}

impl Default for LiveSub {
    fn default() -> Self {
        Self {
            health: proto::DeviceHealth::default(),
            conn: SubConn::Connecting,
            updates: 0,
        }
    }
}

/// Connection state of a live subscription, mirrored from the wire.
#[derive(Clone, Copy, PartialEq)]
enum SubConn {
    /// Until the server reports the subscription live.
    Connecting,
    Live {
        max_int_secs: u16,
    },
    /// The device stopped reporting or cannot be reached; the server keeps
    /// retrying.
    Offline,
    /// The connection to the server dropped and is being reopened.
    Reconnecting,
}

/// Which top-level tab is showing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Devices,
    Commission,
    Updates,
}

/// How long a commission may run before the UI auto-cancels it.
const COMMISSION_TIMEOUT_SECS: u32 = 90;
/// Likewise over Bluetooth: scanning for the device, and its joining its
/// network, add to it.
const BLE_COMMISSION_TIMEOUT_SECS: u32 = 300;

/// The network a device commissioned over Bluetooth joins.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Network {
    Wifi,
    Thread,
    /// Already on its network (Ethernet).
    None,
}

enum Msg {
    SelectTab(Tab),
    PeerAddr(String),
    SetupCode(String),
    DiscoverFilter(String),
    /// Whether to remember the device being commissioned.
    SetRemember(bool),
    Commission,
    /// Proceed with a commission despite the "already commissioned this address"
    /// warning (the user confirmed).
    CommissionAnyway,
    /// Dismiss the re-commission warning without commissioning.
    DismissRecommission,
    /// User clicked Cancel (or the timeout fired) — request server-side cancel.
    Cancel,
    /// Open the discovery popup and scan.
    OpenDiscover,
    CloseDiscover,
    Discover,
    PickDevice(String),
    /// Commission the device at this Bluetooth address.
    PickBleDevice(String),
    /// Commission over Bluetooth (else over the network).
    SetViaBle(bool),
    BleAddr(String),
    SetNetwork(Network),
    WifiSsid(String),
    WifiPassword(String),
    ThreadDataset(String),
    ListDevices,
    /// Re-sync the device list with the devices on our fabric.
    Sync,
    /// Re-read a device's health now (one-shot) and update its card panel.
    Refresh(u64),
    Identify(u64),
    /// Keep a record of a device, so it can be tagged.
    Remember(u64),
    /// Decommission a device (RemoveFabric + drop it from the list).
    Decommission(u64),
    /// Open the logs popup of a device.
    OpenLogs(u64),
    CloseLogs,
    /// Retrieve a log of the device whose logs popup is open.
    FetchLogs(proto::LogIntent),
    /// A live health update arrived for a subscribed device (node_id, the
    /// subscription's report count, health). Deltas are merged into that
    /// device's running view.
    HealthReport(u64, u64, proto::DeviceHealth),
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
    /// Fetch the stored OTA images.
    OtaImages,
    /// The user picked an OTA image file to upload.
    OtaFile(web_sys::File),
    /// The user picked a picture of a QR code, to read the setup code from.
    QrImage(web_sys::File),
    /// How an OTA image upload goes (percent).
    OtaUploading(u8),
    /// Delete stored OTA image `name`.
    OtaDelete(String),
    /// The user chose the image to update a device (node_id) to.
    OtaChoose(u64, String),
    /// Update a device (node_id) to its chosen image.
    OtaUpdate(u64),
    /// How a device's (node_id) update goes.
    OtaProgress(u64, proto::OtaProgress),
    Step(StepLine),
    Done(proto::Response),
    Error(String),
}

struct App {
    tab: Tab,
    peer_addr: String,
    /// Passcode, manual pairing code or `MT:` QR code payload.
    setup_code: String,
    /// Discovery filter: a discriminator or a setup code (empty = any).
    discover_filter: String,
    /// Remember the device being commissioned (keep a record of it).
    remember: bool,
    /// Commission over Bluetooth LE, rather than over the network.
    via_ble: bool,
    /// The Bluetooth address of the device to commission; empty for the one
    /// matching the setup code.
    ble_addr: String,
    /// The network a device commissioned over Bluetooth joins.
    network: Network,
    wifi_ssid: String,
    wifi_password: String,
    /// The Thread operational dataset, hex.
    thread_dataset: String,
    running: bool,
    /// The discovery popup is showing.
    discover_open: bool,
    discovering: bool,
    /// A device op (list / peek / identify / …) is in flight.
    busy_stored: bool,
    discovered: Vec<proto::DiscoveredDevice>,
    discovered_ble: Vec<proto::BleDevice>,
    /// The devices on our fabric.
    stored: Vec<proto::DeviceInfo>,
    /// Per-device tag-input text (node_id → comma-separated edit buffer). Seeded
    /// from the device's current tags when the list arrives.
    tag_inputs: HashMap<u64, String>,
    /// Filter the device list to devices whose tags contain this substring.
    tag_filter: String,
    steps: Vec<StepLine>,
    result: Option<proto::Response>,
    /// Transient status line for device ops (list size / identify ack).
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
    /// The stored OTA images.
    ota_images: Vec<proto::OtaImageInfo>,
    /// The image chosen to update each device to (node_id → image name).
    ota_choice: HashMap<u64, String>,
    /// How each device's update goes (node_id → status line).
    ota_status: HashMap<u64, String>,
    /// The logs popup, when open.
    logs: Option<LogsView>,
    error: Option<String>,
}

/// The logs popup of one device.
struct LogsView {
    node_id: u64,
    /// The log being retrieved or shown.
    intent: Option<proto::LogIntent>,
    loading: bool,
    /// The retrieved log, base64-encoded as it came.
    data: Option<String>,
}

impl Component for App {
    type Message = Msg;
    type Properties = ();

    fn create(_ctx: &Context<Self>) -> Self {
        Self {
            tab: Tab::Devices,
            peer_addr: String::new(),
            setup_code: "20202021".into(),
            discover_filter: String::new(),
            remember: false,
            via_ble: false,
            ble_addr: String::new(),
            network: Network::Wifi,
            wifi_ssid: String::new(),
            wifi_password: String::new(),
            thread_dataset: String::new(),
            running: false,
            discover_open: false,
            discovering: false,
            busy_stored: false,
            discovered: Vec::new(),
            discovered_ble: Vec::new(),
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
            ota_images: Vec::new(),
            ota_choice: HashMap::new(),
            ota_status: HashMap::new(),
            logs: None,
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
                // Devices offer updates to the images.
                if matches!(tab, Tab::Devices | Tab::Updates) {
                    send_request(ctx.link().clone(), proto::Request::OtaImages);
                }
                true
            }
            Msg::OtaImages => {
                send_request(ctx.link().clone(), proto::Request::OtaImages);
                false
            }
            Msg::QrImage(file) => {
                self.error = None;
                send_qr_image(ctx.link().clone(), file);
                false
            }
            Msg::OtaFile(file) => {
                self.notice = Some(upload_notice(0));
                send_ota_upload(ctx.link().clone(), file);
                true
            }
            Msg::OtaUploading(percent) => {
                self.notice = Some(upload_notice(percent));
                true
            }
            Msg::OtaDelete(name) => {
                send_request(ctx.link().clone(), proto::Request::OtaDelete { name });
                false
            }
            Msg::OtaChoose(node_id, name) => {
                self.ota_choice.insert(node_id, name);
                true
            }
            Msg::OtaUpdate(node_id) => {
                let Some(name) = self.ota_choice.get(&node_id).cloned().or_else(|| {
                    let device = self.stored.iter().find(|d| d.handle.node_id == node_id)?;
                    self.applicable(device)
                        .next()
                        .map(|image| image.name.clone())
                }) else {
                    return false;
                };

                self.ota_status
                    .insert(node_id, format!("Announcing {name} to the device…"));
                send_ota_update(ctx.link().clone(), node_id, name);
                true
            }
            Msg::OtaProgress(node_id, progress) => {
                self.ota_status
                    .insert(node_id, ota_progress_text(&progress));
                true
            }
            Msg::PeerAddr(v) => {
                self.peer_addr = v;
                true
            }
            Msg::SetupCode(v) => {
                self.setup_code = v;
                true
            }
            Msg::DiscoverFilter(v) => {
                self.discover_filter = v;
                false
            }
            Msg::SetRemember(v) => {
                self.remember = v;
                false
            }
            Msg::OpenDiscover => {
                self.discover_open = true;
                if !self.discovering {
                    self.start_discover(ctx);
                }
                true
            }
            Msg::CloseDiscover => {
                self.discover_open = false;
                true
            }
            Msg::Discover => {
                self.start_discover(ctx);
                true
            }
            Msg::PickDevice(addr) => {
                self.via_ble = false;
                self.peer_addr = addr;
                self.discover_open = false;
                true
            }
            Msg::PickBleDevice(addr) => {
                self.via_ble = true;
                self.ble_addr = addr;
                self.discover_open = false;
                true
            }
            Msg::SetViaBle(v) => {
                self.via_ble = v;
                true
            }
            Msg::BleAddr(v) => {
                self.ble_addr = v;
                true
            }
            Msg::SetNetwork(v) => {
                self.network = v;
                true
            }
            Msg::WifiSsid(v) => {
                self.wifi_ssid = v;
                true
            }
            Msg::WifiPassword(v) => {
                self.wifi_password = v;
                true
            }
            Msg::ThreadDataset(v) => {
                self.thread_dataset = v;
                true
            }
            Msg::Commission => {
                // Guard: re-commissioning the device we *just* commissioned is
                // almost always a mistake — its commissioning window has closed,
                // so PASE will hang. Warn + require confirmation instead.
                let addr = self.peer_addr.trim().to_string();
                if !self.via_ble && self.last_commissioned_addr.as_deref() == Some(addr.as_str()) {
                    self.confirm_recommission = true;
                    return true;
                }
                self.start_commission(ctx)
            }
            Msg::CommissionAnyway => {
                self.confirm_recommission = false;
                self.start_commission(ctx)
            }
            Msg::DismissRecommission => {
                self.confirm_recommission = false;
                true
            }
            Msg::Cancel => {
                // Only meaningful while a commission is in flight.
                if !self.running {
                    return false;
                }
                // Fire-and-forget the cancel on its own WS connection; the
                // running commission replies `Cancelled` on its original one.
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
            Msg::Sync => {
                self.busy_stored = true;
                self.stored_status = Some("Syncing with the devices on our fabric…".into());
                self.error = None;
                self.notice = None;
                send_request(ctx.link().clone(), proto::Request::Sync);
                true
            }
            Msg::Refresh(node_id) => {
                // One-shot re-read — the result updates this device's card panel
                // (not a status line). Useful for the values the subscription
                // doesn't push on every change (e.g. the live-computed uptime).
                self.busy_stored = true;
                self.error = None;
                send_request(ctx.link().clone(), proto::Request::PeekDevice { node_id });
                true
            }
            Msg::Remember(node_id) => {
                self.busy_stored = true;
                self.error = None;
                send_request(ctx.link().clone(), proto::Request::Remember { node_id });
                true
            }
            Msg::OpenLogs(node_id) => {
                self.error = None;
                self.logs = Some(LogsView {
                    node_id,
                    intent: None,
                    loading: false,
                    data: None,
                });
                true
            }
            Msg::CloseLogs => {
                self.logs = None;
                true
            }
            Msg::FetchLogs(intent) => {
                let Some(logs) = self.logs.as_mut() else {
                    return false;
                };
                logs.intent = Some(intent);
                logs.loading = true;
                logs.data = None;
                self.error = None;
                send_request(
                    ctx.link().clone(),
                    proto::Request::RetrieveLogs {
                        node_id: logs.node_id,
                        intent,
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
                // Clear any notice from a previous decommission, so what remains
                // on screen describes *this* one.
                self.notice = None;
                // The "you just commissioned this" guard is now stale: we are
                // giving the device up, so re-commissioning it is the expected
                // next step, not a mistake to warn about. Left set, `Msg::Commission`
                // would keep swallowing the click and only show the warning.
                self.last_commissioned_addr = None;
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
            Msg::HealthReport(node_id, seq, health) => {
                // Merge into that device's running view (ignore if not watching).
                if let Some(sub) = self.subs.get_mut(&node_id) {
                    sub.updates = seq;
                    sub.health.merge(health);
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
                    proto::SubscriptionState::Offline => {
                        if let Some(sub) = self.subs.get_mut(&node_id) {
                            sub.conn = SubConn::Offline;
                        }
                    }
                    // Terminal. A device we still watch and still store gets
                    // re-subscribed; one we stopped watching (e.g. being
                    // decommissioned) or that is gone from the store is dropped.
                    proto::SubscriptionState::Stopped => {
                        let still_stored = self.stored.iter().any(|d| d.handle.node_id == node_id);
                        match self.subs.get_mut(&node_id) {
                            Some(sub) if still_stored => {
                                sub.conn = SubConn::Reconnecting;
                                self.sub_send(&proto::Request::Subscribe { node_id });
                            }
                            _ => {
                                self.subs.remove(&node_id);
                            }
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
                if let Some(logs) = self.logs.as_mut() {
                    logs.loading = false;
                }
                match resp {
                    proto::Response::Discovered {
                        devices,
                        ble_devices,
                    } => {
                        self.discovered = devices;
                        self.discovered_ble = ble_devices;
                    }
                    proto::Response::SetupCode { setup_code } => self.setup_code = setup_code,
                    proto::Response::Devices { devices, notice } => {
                        self.stored_status = Some(format!("{} device(s)", devices.len()));
                        // A non-fatal remark about whatever produced this list —
                        // e.g. a decommission that dropped the record locally but
                        // could not reach the device to free its fabric slot.
                        if notice.is_some() {
                            self.notice = notice;
                        }
                        // Seed each device's tag-edit buffer from its (normalized)
                        // tags, so the input reflects server truth.
                        self.tag_inputs = devices
                            .iter()
                            .map(|d| (d.handle.node_id, d.tags.join(", ")))
                            .collect();
                        self.stored = devices;
                        // A dashboard watches everything: auto-subscribe every
                        // listed device (and drop panels for any that vanished).
                        self.sync_subscriptions(ctx);
                    }
                    proto::Response::Peek { handle, health, .. } => {
                        // A Refresh result: merge the fresh read into the device's
                        // card panel so its values (uptime, heap, …) update in place.
                        self.subs
                            .entry(handle.node_id)
                            .or_default()
                            .health
                            .merge(health);
                    }
                    proto::Response::Ok => {
                        self.stored_status =
                            Some("Identify acknowledged — device should blink.".into());
                    }
                    // `Subscribed` / streamed subscription frames arrive on the
                    // dedicated persistent subscription WS, not this one-shot path.
                    proto::Response::Subscribed { .. } => {}
                    proto::Response::OtaImages { images } => {
                        if self
                            .notice
                            .as_deref()
                            .is_some_and(|notice| notice.starts_with(UPLOAD_NOTICE))
                        {
                            self.notice = None;
                        }
                        self.ota_images = images;
                    }
                    // Uploads run on their own connection (`send_ota_upload`).
                    proto::Response::OtaUploadStarted { .. } => {}
                    proto::Response::Cancelled { decommissioned } => {
                        self.notice = Some(if decommissioned {
                            "Commission cancelled — the device had been added to our fabric and was cleaned up (decommissioned).".into()
                        } else {
                            "Commission cancelled before the device was modified.".into()
                        });
                    }
                    proto::Response::Logs { node_id, data, .. } => {
                        if let Some(logs) = self.logs.as_mut().filter(|l| l.node_id == node_id) {
                            logs.data = Some(data);
                        }
                    }
                    // The device is listed now: refresh the list and surface it on
                    // the Devices tab.
                    other @ proto::Response::Commissioned(_) => {
                        // Its commissioning window is closed now — remember its
                        // address to guard a re-commission.
                        if !self.via_ble {
                            self.last_commissioned_addr = Some(self.peer_addr.trim().to_string());
                        }
                        if let proto::Response::Commissioned(result) = &other {
                            if let Some(node_id) = result.rematch_candidate {
                                self.notice = Some(rematch_notice(node_id));
                            }
                        }
                        self.result = Some(other);
                        self.busy_stored = true;
                        send_request(ctx.link().clone(), proto::Request::ListDevices);
                        self.tab = Tab::Devices;
                    }
                    proto::Response::Error { message } => self.error = Some(message),
                }
                true
            }
            Msg::Error(e) => {
                self.running = false;
                self.discovering = false;
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
                            Tab::Devices => self.view_devices_tab(link),
                            Tab::Commission => self.view_commission_tab(link),
                            Tab::Updates => self.view_updates_tab(link),
                        }
                    }
                </main>
            </>
        }
    }

    fn rendered(&mut self, ctx: &Context<Self>, first_render: bool) {
        // The app opens on the Devices tab; load it as if the user picked it.
        if first_render {
            ctx.link().send_message(Msg::SelectTab(Tab::Devices));
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
                                node_id,
                                seq,
                                health,
                            } => link.send_message(Msg::HealthReport(node_id, seq, health)),
                            proto::Event::SubscriptionState { node_id, state } => {
                                link.send_message(Msg::SubState(node_id, state))
                            }
                            proto::Event::OtaProgress { node_id, progress } => {
                                link.send_message(Msg::OtaProgress(node_id, progress))
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

    /// Scan for commissionable devices, narrowed by the discovery filter.
    fn start_discover(&mut self, ctx: &Context<Self>) {
        // Up to 4 digits is a discriminator; anything else a setup code, which
        // the commission form then uses too.
        let filter = self.discover_filter.trim();
        let (discriminator, setup_code) = if filter.is_empty() {
            (None, None)
        } else if filter.len() <= 4 && filter.bytes().all(|b| b.is_ascii_digit()) {
            (filter.parse::<u16>().ok(), None)
        } else {
            self.setup_code = filter.to_string();
            (None, Some(filter.to_string()))
        };
        self.discovering = true;
        self.discovered.clear();
        self.discovered_ble.clear();
        self.error = None;
        send_request(
            ctx.link().clone(),
            proto::Request::Discover {
                discriminator,
                setup_code,
                max: 16,
            },
        );
    }

    /// Whether the commission form is filled in usably: how to reach the device,
    /// and its setup code.
    fn can_commission(&self) -> bool {
        let reach = if self.via_ble {
            let addr = self.ble_addr.trim();
            let network = match self.network {
                Network::Wifi => !self.wifi_ssid.is_empty(),
                Network::Thread => valid_hex(&self.thread_dataset),
                Network::None => true,
            };

            (addr.is_empty() || valid_bt_addr(addr)) && network
        } else {
            self.peer_addr.trim().parse::<SocketAddr>().is_ok()
        };

        reach && valid_setup_code(&self.setup_code)
    }

    /// How the commission reaches the device, from the form.
    fn commission_via(&self) -> proto::CommissionVia {
        if !self.via_ble {
            return proto::CommissionVia::Ip {
                addr: self.peer_addr.trim().to_string(),
            };
        }

        let addr = self.ble_addr.trim();
        let network = match self.network {
            Network::Wifi => Some(proto::NetworkCredentials::Wifi {
                ssid: self.wifi_ssid.clone(),
                password: self.wifi_password.clone(),
            }),
            Network::Thread => Some(proto::NetworkCredentials::Thread {
                dataset: self.thread_dataset.split_whitespace().collect(),
            }),
            Network::None => None,
        };

        proto::CommissionVia::Ble {
            addr: (!addr.is_empty()).then(|| addr.to_string()),
            network,
        }
    }

    /// Kick off a commission (shared by `Commission` and `CommissionAnyway`).
    fn start_commission(&mut self, ctx: &Context<Self>) -> bool {
        let setup_code = self.setup_code.trim().to_string();
        if setup_code.is_empty() {
            self.error = Some("enter the device's setup code".into());
            return true;
        }
        self.running = true;
        self.steps.clear();
        self.result = None;
        self.notice = None;
        self.error = None;

        let req = proto::Request::Commission {
            via: self.commission_via(),
            setup_code,
            remember: self.remember,
        };
        send_request(ctx.link().clone(), req);

        // Safety net: auto-cancel if the commission runs too long. When the timer
        // fires we only act if still `running` (a completed run cleared it).
        let timeout_secs = if self.via_ble {
            BLE_COMMISSION_TIMEOUT_SECS
        } else {
            COMMISSION_TIMEOUT_SECS
        };
        let link = ctx.link().clone();
        spawn_local(async move {
            gloo_timers::future::TimeoutFuture::new(timeout_secs * 1000).await;
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
                    { tab(Tab::Devices, "Devices") }
                    { tab(Tab::Commission, "Commission") }
                    { tab(Tab::Updates, "Updates") }
                </ul>
            </div>
        }
    }

    // --- Discover popup ---------------------------------------------------

    /// Full-screen list of the commissionable devices: picking one on the
    /// network fills the peer address, one over Bluetooth the Bluetooth device.
    fn view_discover_modal(&self, link: &html::Scope<Self>) -> Html {
        if !self.discover_open {
            return html! {};
        }
        html! {
            <div class="modal is-active">
                <div class="modal-background"></div>
                <div class="modal-card is-fullscreen">
                    <header class="modal-card-head">
                        <p class="modal-card-title">{ "Commissionable devices" }</p>
                        <button class="delete is-large" aria-label="close"
                            onclick={link.callback(|_| Msg::CloseDiscover)}></button>
                    </header>
                    <section class="modal-card-body">
                        <div class="app container">
                            <div class="field is-grouped">
                                <div class="control is-expanded">
                                    <input class="input" placeholder="discriminator or setup code (any)"
                                        value={self.discover_filter.clone()}
                                        oninput={link.callback(|e: InputEvent| Msg::DiscoverFilter(input_value(e)))} />
                                </div>
                                <div class="control">
                                    <button class={classes!("button", "is-primary", self.discovering.then_some("is-loading"))}
                                        disabled={self.discovering || self.running}
                                        onclick={link.callback(|_| Msg::Discover)}>
                                        { "Rescan" }
                                    </button>
                                </div>
                            </div>

                            // The page's error banner is hidden behind the popup.
                            { for self.error.iter().map(|e| html!{
                                <div class="notification is-danger is-light">
                                    <strong>{ "Error: " }</strong>{ e.clone() }
                                </div>
                            }) }
                            { self.view_discovered(link) }
                        </div>
                    </section>
                </div>
            </div>
        }
    }

    fn view_discovered(&self, link: &html::Scope<Self>) -> Html {
        if self.discovered.is_empty() && self.discovered_ble.is_empty() {
            let text = if self.discovering {
                "Scanning the network and Bluetooth…"
            } else {
                "No commissionable devices found."
            };
            return html! { <p class="has-text-grey">{ text }</p> };
        }

        html! {
            <>
                { self.view_discovered_ip(link) }
                { self.view_discovered_ble(link) }
            </>
        }
    }

    fn view_discovered_ble(&self, link: &html::Scope<Self>) -> Html {
        if self.discovered_ble.is_empty() {
            return html! {};
        }
        html! {
            <>
                <h3 class="title is-6">{ "Over Bluetooth" }</h3>
                <table class="table is-fullwidth is-hoverable">
                    <thead>
                        <tr><th>{ "Bluetooth address" }</th><th>{ "Discriminator" }</th><th>{ "Vendor:product" }</th></tr>
                    </thead>
                    <tbody>
                        { for self.discovered_ble.iter().map(|d| {
                            let addr = d.addr.clone();
                            let onclick = link.callback(move |_| Msg::PickBleDevice(addr.clone()));
                            html! {
                                <tr class="is-clickable" title="Commission this device over Bluetooth" {onclick}>
                                    <td class="mono">{ d.addr.clone() }</td>
                                    <td class="mono">{ d.discriminator }</td>
                                    <td class="mono">{ format!("{:04x}:{:04x}", d.vendor_id, d.product_id) }</td>
                                </tr>
                            }
                        }) }
                    </tbody>
                </table>
            </>
        }
    }

    fn view_discovered_ip(&self, link: &html::Scope<Self>) -> Html {
        if self.discovered.is_empty() {
            return html! {};
        }
        html! {
            <>
            <h3 class="title is-6">{ "On the network" }</h3>
            <table class="table is-fullwidth is-hoverable">
                <thead>
                    <tr><th>{ "Address" }</th><th>{ "Commissionable id" }</th></tr>
                </thead>
                <tbody>
                    { for self.discovered.iter().map(|d| {
                        let addr = d.addr.clone();
                        let onclick = link.callback(move |_| Msg::PickDevice(addr.clone()));
                        html! {
                            <tr class="is-clickable" title="Use this device" {onclick}>
                                <td class="mono">{ d.addr.clone() }</td>
                                <td class="mono">{ format!("{:#018x}", d.commissionable_id) }</td>
                            </tr>
                        }
                    }) }
                </tbody>
            </table>
            </>
        }
    }

    // --- Commission tab ---------------------------------------------------

    fn view_commission_tab(&self, link: &html::Scope<Self>) -> Html {
        // Flag what is typed but unusable; leave an empty field neutral.
        let addr_invalid = !self.peer_addr.trim().is_empty()
            && self.peer_addr.trim().parse::<SocketAddr>().is_err();
        let code_invalid =
            !self.setup_code.trim().is_empty() && !valid_setup_code(&self.setup_code);

        html! {
            <>
                { self.view_recommission_warning(link) }
                { self.view_discover_modal(link) }
                <div class="box">
                    <h2 class="title is-5">{ "Commission a device" }</h2>

                    <div class="field">
                        <label class="label">{ "Reach the device" }</label>
                        <div class="buttons has-addons">
                            <button class={classes!("button", (!self.via_ble).then_some("is-link"), (!self.via_ble).then_some("is-selected"))}
                                onclick={link.callback(|_| Msg::SetViaBle(false))}>
                                { "Over the network" }
                            </button>
                            <button class={classes!("button", self.via_ble.then_some("is-link"), self.via_ble.then_some("is-selected"))}
                                onclick={link.callback(|_| Msg::SetViaBle(true))}>
                                { "Over Bluetooth" }
                            </button>
                        </div>
                    </div>

                    {
                        if self.via_ble {
                            self.view_ble_fields(link)
                        } else {
                            html! {
                                <div class="field">
                                    <label class="label">{ "Peer address" }</label>
                                    <div class="field has-addons">
                                        <div class="control is-expanded">
                                            <input class={classes!("input", "mono", addr_invalid.then_some("is-danger"))}
                                                placeholder="[IPv6]:port or IPv4:port"
                                                value={self.peer_addr.clone()}
                                                oninput={link.callback(|e: InputEvent| Msg::PeerAddr(input_value(e)))} />
                                        </div>
                                        <div class="control">{ self.discover_button(link) }</div>
                                    </div>
                                </div>
                            }
                        }
                    }

                    <div class="field">
                        <label class="label">{ "Setup code" }</label>
                        <div class="field has-addons">
                            <div class="control is-expanded">
                                <input class={classes!("input", "mono", code_invalid.then_some("is-danger"))}
                                    value={self.setup_code.clone()}
                                    oninput={link.callback(|e: InputEvent| Msg::SetupCode(input_value(e)))} />
                            </div>
                            <div class="control">
                                <div class="file">
                                    <label class="file-label"
                                        title="Read the setup code from a picture of the device's QR code">
                                        <input class="file-input" type="file" accept="image/png,image/jpeg"
                                            onchange={link.batch_callback(|e: Event| picked_file(e).map(Msg::QrImage))} />
                                        <span class="file-cta">
                                            <span class="file-label">{ "From QR image" }</span>
                                        </span>
                                    </label>
                                </div>
                            </div>
                        </div>
                        <p class="help">{ "Passcode, manual pairing code or MT: QR code payload." }</p>
                    </div>

                    <div class="field">
                        <label class="checkbox">
                            <input type="checkbox" checked={self.remember}
                                onchange={link.callback(|e: Event| Msg::SetRemember(checkbox_checked(e)))} />
                            { " Remember" }
                        </label>
                        <p class="help">
                            { "Keep a record of the device, so it can be tagged and stays listed while offline. Either way, it stays on our fabric." }
                        </p>
                    </div>

                    <div class="field">
                        <div class="field is-grouped">
                            <div class="control">
                                <button class={classes!("button", "is-primary", self.running.then_some("is-loading"))}
                                    disabled={self.running || !self.can_commission()}
                                    onclick={link.callback(|_| Msg::Commission)}>
                                    { "Commission" }
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

    /// The Discover button, opening the popup of commissionable devices.
    fn discover_button(&self, link: &html::Scope<Self>) -> Html {
        html! {
            <button class="button is-link"
                title="Find commissionable devices, on the network and over Bluetooth"
                disabled={self.running}
                onclick={link.callback(|_| Msg::OpenDiscover)}>
                <span class="icon">
                    <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"
                        width="18" height="18" fill="none" stroke="currentColor"
                        stroke-width="2.5" stroke-linecap="round">
                        <circle cx="10.5" cy="10.5" r="6.5" />
                        <line x1="15.5" y1="15.5" x2="21" y2="21" />
                    </svg>
                </span>
                <span>{ "Discover" }</span>
            </button>
        }
    }

    /// Commissioning over Bluetooth: which device, and the network it joins.
    fn view_ble_fields(&self, link: &html::Scope<Self>) -> Html {
        let addr = self.ble_addr.trim();
        let addr_invalid = !addr.is_empty() && !valid_bt_addr(addr);
        let dataset_invalid =
            !self.thread_dataset.trim().is_empty() && !valid_hex(&self.thread_dataset);

        let network = |network: Network, label: &str| {
            html! {
                <label class="radio mr-4">
                    <input type="radio" name="network" checked={self.network == network}
                        onchange={link.callback(move |_| Msg::SetNetwork(network))} />
                    { format!(" {label}") }
                </label>
            }
        };

        html! {
            <>
                <div class="field">
                    <label class="label">{ "Bluetooth device" }</label>
                    <div class="field has-addons">
                        <div class="control is-expanded">
                            <input class={classes!("input", "mono", addr_invalid.then_some("is-danger"))}
                                placeholder="any device matching the setup code"
                                value={self.ble_addr.clone()}
                                oninput={link.callback(|e: InputEvent| Msg::BleAddr(input_value(e)))} />
                        </div>
                        <div class="control">{ self.discover_button(link) }</div>
                    </div>
                </div>

                <div class="field">
                    <label class="label">{ "Network to join" }</label>
                    <div class="control">
                        { network(Network::Wifi, "Wi-Fi") }
                        { network(Network::Thread, "Thread") }
                        { network(Network::None, "None (already on its network)") }
                    </div>
                </div>

                {
                    match self.network {
                        Network::Wifi => html! {
                            <div class="field is-grouped">
                                <div class="control is-expanded">
                                    <input class="input" placeholder="Wi-Fi network (SSID)"
                                        value={self.wifi_ssid.clone()}
                                        oninput={link.callback(|e: InputEvent| Msg::WifiSsid(input_value(e)))} />
                                </div>
                                <div class="control is-expanded">
                                    <input class="input" type="password" placeholder="password"
                                        value={self.wifi_password.clone()}
                                        oninput={link.callback(|e: InputEvent| Msg::WifiPassword(input_value(e)))} />
                                </div>
                            </div>
                        },
                        Network::Thread => html! {
                            <div class="field">
                                <div class="control">
                                    <textarea class={classes!("textarea", "mono", dataset_invalid.then_some("is-danger"))} rows="2"
                                        placeholder="Thread operational dataset (hex)"
                                        value={self.thread_dataset.clone()}
                                        oninput={link.callback(|e: InputEvent| Msg::ThreadDataset(textarea_value(e)))} />
                                </div>
                            </div>
                        },
                        Network::None => html! {},
                    }
                }
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
                        onclick={link.callback(|_| Msg::CommissionAnyway)}>
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

    /// The commission's progress: the in-flight step, or how it ended.
    fn view_progress(&self) -> Html {
        let Some(last) = self.steps.last() else {
            return html! {};
        };

        let phase = phase_name(last.step);
        let (label, bar_class) = match &last.status {
            proto::StepStatus::Failed { message } => {
                (format!("{phase} failed: {message}"), "is-danger")
            }
            _ if self.running => (format!("{phase}…"), "is-primary"),
            _ => ("Done".to_string(), "is-success"),
        };

        html! {
            <div class="box">
                <p class="has-text-weight-medium mb-2">{ label }</p>
                {
                    if self.running {
                        // No value: an indeterminate (animated) bar.
                        html! { <progress class={classes!("progress", bar_class)} /> }
                    } else {
                        html! { <progress class={classes!("progress", bar_class)} value="1" max="1" /> }
                    }
                }
            </div>
        }
    }

    fn view_result(&self) -> Html {
        match &self.result {
            Some(proto::Response::Commissioned(result)) => html! {
                <div class="notification is-success is-light">
                    { format!(
                        "Commissioned as node 0x{:016x} — see the Devices tab.",
                        result.handle.node_id
                    ) }
                </div>
            },
            Some(proto::Response::Error { message }) => html! {
                <div class="notification is-danger is-light">{ format!("Server error: {message}") }</div>
            },
            // Device responses (Devices / Peek / Ok) and Discovered are routed to
            // their own state in `Msg::Done`, not shown here.
            Some(_) | None => html! {},
        }
    }

    // --- Devices tab ------------------------------------------------------

    /// The devices on our fabric, each with its live health and its actions.
    fn view_devices_tab(&self, link: &html::Scope<Self>) -> Html {
        let filter = self.tag_filter.trim().to_lowercase();
        let visible: Vec<&proto::DeviceInfo> = self
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
                        <h2 class="title is-5">{ "Devices" }</h2>
                    </div>
                    <div class="level-right buttons">
                        <button class={classes!("button", "is-small", self.busy_stored.then_some("is-loading"))}
                            disabled={self.busy_stored}
                            onclick={link.callback(|_| Msg::ListDevices)}>
                            { "Refresh" }
                        </button>
                        <button class="button is-small is-link is-light"
                            disabled={self.busy_stored}
                            title="Add the devices on our fabric missing from the list, and refresh the others"
                            onclick={link.callback(|_| Msg::Sync)}>
                            { "Sync" }
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
                                { "No devices on our fabric yet. Commission one on the Commission tab." }
                            </p>
                        }
                    } else if visible.is_empty() {
                        html! {
                            <p class="has-text-grey">
                                { format!("No devices tagged like \"{}\".", self.tag_filter.trim()) }
                            </p>
                        }
                    } else {
                        html! { <>{ for visible.into_iter().map(|d| self.view_device_card(d, link)) }</> }
                    }
                }

                { self.view_logs_modal(link) }
            </div>
        }
    }

    fn view_device_card(&self, d: &proto::DeviceInfo, link: &html::Scope<Self>) -> Html {
        let node_id = d.handle.node_id;
        let refresh = link.callback(move |_| Msg::Refresh(node_id));
        let identify = link.callback(move |_| Msg::Identify(node_id));
        let logs = link.callback(move |_| Msg::OpenLogs(node_id));
        let remember = link.callback(move |_| Msg::Remember(node_id));
        let decommission = link.callback(move |_| Msg::Decommission(node_id));
        let save_tags = link.callback(move |_| Msg::SaveTags(node_id));
        let on_tags = link.callback(move |e: InputEvent| Msg::TagsInput(node_id, input_value(e)));

        // This device's live subscription (auto-created; `None` only very briefly).
        let live = self.subs.get(&node_id);

        // Offline per its subscription; until that is live or offline, per the
        // last sync. Only Refresh and the actions not needing the device remain.
        let offline = match live.map(|sub| sub.conn) {
            Some(SubConn::Live { .. }) => false,
            Some(SubConn::Offline) => true,
            _ => d.online == Some(false),
        };

        let title = device_title(d);

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
                        <div class="level-right tags">
                            { if !d.remembered {
                                html!{ <span class="tag is-light"
                                    title="No record is kept: it cannot be tagged, and is listed only while on our fabric">
                                    { "not remembered" }</span> }
                              } else { html!{} } }
                            { if offline {
                                html!{ <span class="tag is-danger is-light"
                                    title="Not answering; Refresh checks it now">
                                    { "offline" }</span> }
                              } else { html!{} } }
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

                    {
                        if d.remembered {
                            html! {
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
                            }
                        } else {
                            html! {}
                        }
                    }

                    <div class="buttons are-small">
                        <button
                            class={classes!("button", "is-link", "is-light", self.busy_stored.then_some("is-loading"))}
                            disabled={self.busy_stored} onclick={refresh}>
                            { "Refresh" }
                        </button>
                        <button class="button is-warning is-light" disabled={self.busy_stored || offline}
                            onclick={identify}>
                            { "Identify" }
                        </button>
                        <button class="button is-light" disabled={offline} onclick={logs}>
                            { "Logs" }
                        </button>
                        { if d.remembered { html!{} } else { html!{
                            <button class="button is-info is-light" disabled={self.busy_stored}
                                title="Keep a record of it, so it can be tagged and stays listed while offline"
                                onclick={remember}>
                                { "Remember" }
                            </button>
                        } } }
                        <button class="button is-danger is-light" disabled={self.busy_stored} onclick={decommission}>
                            { "Decommission" }
                        </button>
                    </div>

                    { self.view_device_update(d, offline, link) }

                    // Every listed device is watched automatically — the panel is
                    // always shown (it renders "connecting…" until the first report).
                    { self.view_live_health(live) }
                </div>
            </div>
        }
    }

    /// The images a device can update to: for its vendor and product, and newer
    /// than what it runs.
    fn applicable<'a>(
        &'a self,
        d: &'a proto::DeviceInfo,
    ) -> impl Iterator<Item = &'a proto::OtaImageInfo> + 'a {
        let fp = &d.fingerprint;

        self.ota_images.iter().filter(move |image| {
            fp.vendor_id
                .is_none_or(|vid| image.vendor_id == 0 || image.vendor_id == vid)
                && fp
                    .product_id
                    .is_none_or(|pid| image.product_id == 0 || image.product_id == pid)
                && fp.software_version.is_none_or(|v| image.version > v)
        })
    }

    /// A device card's OTA update controls, when there is an image for it.
    fn view_device_update(
        &self,
        d: &proto::DeviceInfo,
        offline: bool,
        link: &html::Scope<Self>,
    ) -> Html {
        let node_id = d.handle.node_id;
        let images: Vec<_> = self.applicable(d).collect();
        let status = self.ota_status.get(&node_id);

        if images.is_empty() && status.is_none() {
            return html! {};
        }

        let chosen = self
            .ota_choice
            .get(&node_id)
            .cloned()
            .or_else(|| images.first().map(|image| image.name.clone()));

        let on_choose = link.callback(move |e: Event| Msg::OtaChoose(node_id, select_value(e)));
        let update = link.callback(move |_| Msg::OtaUpdate(node_id));

        html! {
            <div class="mb-2">
                if !images.is_empty() {
                    <div class="field has-addons mb-1">
                        <div class="control">
                            <div class="select is-small">
                                <select onchange={on_choose}>
                                    { for images.iter().map(|image| html! {
                                        <option value={image.name.clone()}
                                            selected={chosen.as_deref() == Some(image.name.as_str())}>
                                            { format!("{} ({})", image.version_str, image.version) }
                                        </option>
                                    }) }
                                </select>
                            </div>
                        </div>
                        <div class="control">
                            <button class="button is-small is-primary is-light" disabled={offline}
                                onclick={update}>
                                { "Update" }
                            </button>
                        </div>
                    </div>
                }
                { for status.map(|status| html! {
                    <p class="is-size-7 has-text-grey">{ status.clone() }</p>
                }) }
            </div>
        }
    }

    /// The Updates tab: upload, list and delete OTA images.
    fn view_updates_tab(&self, link: &html::Scope<Self>) -> Html {
        let on_file = link.batch_callback(|e: Event| picked_file(e).map(Msg::OtaFile));
        let refresh = link.callback(|_| Msg::OtaImages);

        html! {
            <div class="box">
                <div class="field">
                    <label class="label">{ "Upload an OTA image (.ota)" }</label>
                    <div class="control">
                        <input class="input" type="file" accept=".ota" onchange={on_file} />
                    </div>
                    <p class="help">
                        { "Its header and digest are checked; devices then find it under Update, if it is newer than what they run." }
                    </p>
                </div>

                <table class="table is-fullwidth is-narrow">
                    <thead>
                        <tr>
                            <th>{ "Image" }</th>
                            <th>{ "Vendor / product" }</th>
                            <th>{ "Version" }</th>
                            <th>{ "Size" }</th>
                            <th></th>
                        </tr>
                    </thead>
                    <tbody>
                        { for self.ota_images.iter().map(|image| {
                            let name = image.name.clone();
                            let delete = link.callback(move |_| Msg::OtaDelete(name.clone()));

                            html! {
                                <tr>
                                    <td>{ image.name.clone() }</td>
                                    <td>{ format!("0x{:04X} / 0x{:04X}", image.vendor_id, image.product_id) }</td>
                                    <td>{ format!("{} ({})", image.version_str, image.version) }</td>
                                    <td>{ format!("{} KiB", image.size / 1024) }</td>
                                    <td>
                                        <button class="button is-small is-danger is-light" onclick={delete}>
                                            { "Delete" }
                                        </button>
                                    </td>
                                </tr>
                            }
                        }) }
                    </tbody>
                </table>

                <button class="button is-small" onclick={refresh}>{ "Refresh" }</button>
            </div>
        }
    }

    /// The live-health panel shown under every device card: a small grid of the
    /// diagnostic metrics from the merged running view, plus a live/reconnecting
    /// status and the update count. `None` while the subscription is still being
    /// set up (shown as connecting).
    /// Full-screen popup to retrieve, read and download a device's logs.
    fn view_logs_modal(&self, link: &html::Scope<Self>) -> Html {
        let Some(logs) = &self.logs else {
            return html! {};
        };

        let title = self
            .stored
            .iter()
            .find(|d| d.handle.node_id == logs.node_id)
            .map(device_title)
            .unwrap_or_else(|| format!("node 0x{:016x}", logs.node_id));

        let intent_button = |intent: proto::LogIntent| {
            let active = logs.intent == Some(intent);
            html! {
                <button
                    class={classes!("button", active.then_some("is-link"), (active && logs.loading).then_some("is-loading"))}
                    disabled={logs.loading}
                    onclick={link.callback(move |_| Msg::FetchLogs(intent))}>
                    { log_intent_name(intent) }
                </button>
            }
        };

        html! {
            <div class="modal is-active">
                <div class="modal-background"></div>
                <div class="modal-card is-fullscreen">
                    <header class="modal-card-head">
                        <p class="modal-card-title">{ format!("Logs — {title}") }</p>
                        <button class="delete is-large" aria-label="close"
                            onclick={link.callback(|_| Msg::CloseLogs)}></button>
                    </header>
                    <section class="modal-card-body">
                        <div class="app container">
                            <div class="buttons">
                                { intent_button(proto::LogIntent::EndUserSupport) }
                                { intent_button(proto::LogIntent::NetworkDiag) }
                                { intent_button(proto::LogIntent::CrashLogs) }
                            </div>

                            // The page's error banner is hidden behind the popup.
                            { for self.error.iter().map(|e| html!{
                                <div class="notification is-danger is-light">
                                    <strong>{ "Error: " }</strong>{ e.clone() }
                                </div>
                            }) }
                            { self.view_log(logs) }
                        </div>
                    </section>
                </div>
            </div>
        }
    }

    /// A retrieved log: its text, or its size if it is binary, with a download.
    fn view_log(&self, logs: &LogsView) -> Html {
        use base64::Engine as _;

        let (Some(intent), Some(data)) = (logs.intent, &logs.data) else {
            let hint = if logs.loading {
                "Retrieving the log…"
            } else {
                "Pick the log to retrieve."
            };
            return html! { <p class="has-text-grey">{ hint }</p> };
        };

        let log = base64::engine::general_purpose::STANDARD
            .decode(data)
            .unwrap_or_default();
        if log.is_empty() {
            return html! {
                <p class="has-text-grey">{ format!("The device has no {} log.", log_intent_name(intent)) }</p>
            };
        }

        let size = log.len();
        let body = match String::from_utf8(log) {
            Ok(text) => html! { <pre class="result">{ text }</pre> },
            Err(_) => {
                html! { <p>{ format!("{size} bytes of binary data — download it to inspect.") }</p> }
            }
        };
        let file_name = format!("node-{:016x}-{}.log", logs.node_id, log_intent_file(intent));

        html! {
            <>
                <div class="level is-mobile">
                    <div class="level-left">
                        <span class="has-text-grey">{ format!("{} log, {size} bytes", log_intent_name(intent)) }</span>
                    </div>
                    <div class="level-right">
                        <a class="button is-small" download={file_name}
                            href={format!("data:application/octet-stream;base64,{data}")}>
                            { "Download" }
                        </a>
                    </div>
                </div>
                { body }
            </>
        }
    }

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
            SubConn::Connecting => ("◌ connecting…".to_string(), "tag is-light"),
            SubConn::Live { max_int_secs } => (
                format!(
                    "● subscribed · reports at least every {max_int_secs} s · {} report(s)",
                    sub.updates
                ),
                "tag is-success is-light",
            ),
            SubConn::Offline => (
                format!("○ offline · retrying · {} report(s)", sub.updates),
                "tag is-danger is-light",
            ),
            SubConn::Reconnecting => (
                format!("◌ reconnecting… · {} report(s)", sub.updates),
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

/// The notice for a just-remembered device that may be a unit already listed.
fn rematch_notice(node_id: u64) -> String {
    format!(
        "Possible match: the device is the same model as device 0x{node_id:016x} — \
         check the Devices tab whether they are the same unit."
    )
}

/// A human label for a device: the user-set NodeLabel, else the device's own
/// ProductName, else the stable identifiers, else the raw node id.
fn device_title(d: &proto::DeviceInfo) -> String {
    if !d.node_label.is_empty() {
        d.node_label.clone()
    } else if !d.product_name.is_empty() {
        d.product_name.clone()
    } else if !d.unique_id.is_empty() {
        format!("uid {}", d.unique_id)
    } else if !d.serial_number.is_empty() {
        format!("sn {}", d.serial_number)
    } else {
        format!("node 0x{:016x}", d.handle.node_id)
    }
}

fn log_intent_name(intent: proto::LogIntent) -> &'static str {
    match intent {
        proto::LogIntent::EndUserSupport => "End-user support",
        proto::LogIntent::NetworkDiag => "Network diagnostics",
        proto::LogIntent::CrashLogs => "Crash",
    }
}

fn log_intent_file(intent: proto::LogIntent) -> &'static str {
    match intent {
        proto::LogIntent::EndUserSupport => "support",
        proto::LogIntent::NetworkDiag => "network",
        proto::LogIntent::CrashLogs => "crash",
    }
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
                    proto::Event::HealthReport { .. }
                    | proto::Event::SubscriptionState { .. }
                    | proto::Event::OtaProgress { .. } => {}
                },
                proto::ServerMsg::Response { response, .. } => {
                    link.send_message(Msg::Done(response));
                    return;
                }
            }
        }
    });
}

/// The size of an upload chunk, before base64.
const UPLOAD_CHUNK_LEN: u64 = 256 * 1024;

const UPLOAD_NOTICE: &str = "Uploading the OTA image";

fn upload_notice(percent: u8) -> String {
    format!("{UPLOAD_NOTICE}: {percent}%")
}

/// Upload an OTA image file in chunks, on its own connection.
/// The file picked in a file input.
fn picked_file(e: Event) -> Option<web_sys::File> {
    use wasm_bindgen::JsCast;

    e.target()
        .and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok())
        .and_then(|input| input.files())
        .and_then(|files| files.get(0))
}

/// Read a picture of a QR code and have the server read the setup code from it.
fn send_qr_image(link: html::Scope<App>, file: web_sys::File) {
    use base64::Engine as _;

    spawn_local(async move {
        match gloo_file::futures::read_as_bytes(&gloo_file::File::from(file)).await {
            Ok(image) => send_request(
                link,
                proto::Request::ReadQrImage {
                    image: base64::engine::general_purpose::STANDARD.encode(image),
                },
            ),
            Err(e) => link.send_message(Msg::Error(format!("cannot read the image: {e}"))),
        }
    });
}

fn send_ota_upload(link: html::Scope<App>, file: web_sys::File) {
    spawn_local(async move {
        if let Err(e) = ota_upload(&link, gloo_file::File::from(file)).await {
            link.send_message(Msg::Error(e));
        }
    });
}

async fn ota_upload(link: &html::Scope<App>, file: gloo_file::File) -> Result<(), String> {
    use base64::Engine as _;

    let ws = WebSocket::open(WS_URL).map_err(|e| format!("connect failed: {e}"))?;
    let (mut write, mut read) = ws.split();

    let size = file.size();
    let mut req_id = 0;

    let begin = proto::Request::OtaUploadBegin { size };
    let upload_id = match call(&mut write, &mut read, &mut req_id, begin).await? {
        proto::Response::OtaUploadStarted { upload_id } => upload_id,
        other => return Err(response_error(other)),
    };

    let mut offset = 0;
    while offset < size {
        let end = (offset + UPLOAD_CHUNK_LEN).min(size);
        let chunk = gloo_file::futures::read_as_bytes(&file.slice(offset, end))
            .await
            .map_err(|e| format!("cannot read the file: {e}"))?;

        let request = proto::Request::OtaUploadChunk {
            upload_id,
            offset,
            data: base64::engine::general_purpose::STANDARD.encode(chunk),
        };
        match call(&mut write, &mut read, &mut req_id, request).await? {
            proto::Response::Ok => (),
            other => return Err(response_error(other)),
        }

        offset = end;
        link.send_message(Msg::OtaUploading((offset * 100 / size.max(1)) as u8));
    }

    let end = proto::Request::OtaUploadEnd { upload_id };
    let images = call(&mut write, &mut read, &mut req_id, end).await?;
    link.send_message(Msg::Done(images));

    Ok(())
}

/// Send `request` with the next `req_id`, and wait for its response.
async fn call<W, R>(
    write: &mut W,
    read: &mut R,
    req_id: &mut proto::ReqId,
    request: proto::Request,
) -> Result<proto::Response, String>
where
    W: futures::Sink<Message, Error = gloo_net::websocket::WebSocketError> + Unpin,
    R: futures::Stream<Item = Result<Message, gloo_net::websocket::WebSocketError>> + Unpin,
{
    *req_id += 1;
    let json = serde_json::to_string(&proto::ClientMsg {
        req_id: *req_id,
        request,
    })
    .unwrap();
    write
        .send(Message::Text(json))
        .await
        .map_err(|e| format!("send failed: {e}"))?;

    while let Some(frame) = read.next().await {
        let Ok(Message::Text(text)) = frame else {
            continue;
        };

        if let Ok(proto::ServerMsg::Response {
            req_id: id,
            response,
        }) = serde_json::from_str(&text)
        {
            if id == *req_id {
                return Ok(response);
            }
        }
    }

    Err("the connection closed".into())
}

fn response_error(response: proto::Response) -> String {
    match response {
        proto::Response::Error { message } => message,
        other => format!("unexpected response: {other:?}"),
    }
}

/// Start the OTA update of a device on its own connection, and follow it there
/// until the device runs the new image.
fn send_ota_update(link: html::Scope<App>, node_id: u64, name: String) {
    spawn_local(async move {
        let ws = match WebSocket::open(WS_URL) {
            Ok(ws) => ws,
            Err(e) => {
                link.send_message(Msg::Error(format!("connect failed: {e}")));
                return;
            }
        };
        let (mut write, mut read) = ws.split();

        let client_msg = proto::ClientMsg {
            req_id: 1,
            request: proto::Request::OtaUpdate { node_id, name },
        };
        let json = serde_json::to_string(&client_msg).unwrap();
        if let Err(e) = write.send(Message::Text(json)).await {
            link.send_message(Msg::Error(format!("send failed: {e}")));
            return;
        }

        while let Some(Ok(Message::Text(text))) = read.next().await {
            match serde_json::from_str(&text) {
                Ok(proto::ServerMsg::Event {
                    event: proto::Event::OtaProgress { node_id, progress },
                }) => {
                    let applied = matches!(progress, proto::OtaProgress::Applied { .. });
                    link.send_message(Msg::OtaProgress(node_id, progress));
                    if applied {
                        return;
                    }
                }
                Ok(proto::ServerMsg::Response {
                    response: proto::Response::Error { message },
                    ..
                }) => {
                    link.send_message(Msg::Error(message));
                    return;
                }
                _ => {}
            }
        }
    });
}

/// A status line for a stage of an OTA update.
fn ota_progress_text(progress: &proto::OtaProgress) -> String {
    match progress {
        proto::OtaProgress::Offered { name } => format!("The device asked; offered {name}"),
        proto::OtaProgress::NotOffered => {
            "The device asked, but the image is not an update for it".into()
        }
        proto::OtaProgress::Downloading { percent } => format!("Downloading: {percent}%"),
        proto::OtaProgress::Applying { version } => format!("Applying version {version}…"),
        proto::OtaProgress::Applied { version } => format!("Updated: runs version {version}"),
    }
}

/// Fire-and-forget a `Cancel` on its own short-lived WS connection. The running
/// commission (on its original connection) replies `Cancelled` when it next polls
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

/// Whether `code` is a usable setup code: a passcode, a manual pairing code or
/// an `MT:` QR code payload. Checks the form only; the server parses it in full.
fn valid_setup_code(code: &str) -> bool {
    /// The fewest base38 characters a QR code payload (11 bytes) takes.
    const MIN_QR_PAYLOAD_LEN: usize = 19;
    /// Passcodes the spec rules out, besides 0 and those above 99999998.
    const INVALID_PASSCODES: [u32; 10] = [
        11111111, 22222222, 33333333, 44444444, 55555555, 66666666, 77777777, 88888888, 12345678,
        87654321,
    ];

    let code = code.trim();

    if let Some(payload) = code.strip_prefix("MT:") {
        return payload.len() >= MIN_QR_PAYLOAD_LEN
            && payload
                .bytes()
                .all(|b| b.is_ascii_digit() || b.is_ascii_uppercase() || b == b'-' || b == b'.');
    }

    let digits: String = code.chars().filter(|c| !matches!(c, '-' | ' ')).collect();
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }

    match digits.len() {
        1..=8 => digits
            .parse::<u32>()
            .is_ok_and(|p| (1..=99999998).contains(&p) && !INVALID_PASSCODES.contains(&p)),
        // The first digit's bit 2 flags the VID/PID the 21-digit form carries
        // (8 and 9 are reserved for future versions).
        11 | 21 => {
            verhoeff::validate(digits.as_str())
                && (digits.as_bytes()[0] - b'0') >> 2 == u8::from(digits.len() == 21)
        }
        _ => false,
    }
}

/// Human-readable name for a commission step.
fn phase_name(step: proto::CommissionStep) -> &'static str {
    match step {
        proto::CommissionStep::Commission => "Commissioning",
    }
}

/// Whether `addr` is a Bluetooth address (`AA:BB:CC:DD:EE:FF`).
fn valid_bt_addr(addr: &str) -> bool {
    let parts: Vec<&str> = addr.split(':').collect();

    parts.len() == 6
        && parts
            .iter()
            .all(|p| p.len() == 2 && p.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Whether `hex` is a non-empty hex byte string (whitespace allowed).
fn valid_hex(hex: &str) -> bool {
    let digits: Vec<u8> = hex.bytes().filter(|b| !b.is_ascii_whitespace()).collect();

    !digits.is_empty() && digits.len().is_multiple_of(2) && digits.iter().all(u8::is_ascii_hexdigit)
}

fn textarea_value(e: InputEvent) -> String {
    use wasm_bindgen::JsCast;

    e.target()
        .and_then(|t| t.dyn_into::<web_sys::HtmlTextAreaElement>().ok())
        .map(|t| t.value())
        .unwrap_or_default()
}

fn input_value(e: InputEvent) -> String {
    use wasm_bindgen::JsCast;
    use web_sys::HtmlInputElement;
    e.target()
        .and_then(|t| t.dyn_into::<HtmlInputElement>().ok())
        .map(|i| i.value())
        .unwrap_or_default()
}

fn select_value(e: Event) -> String {
    use wasm_bindgen::JsCast;
    use web_sys::HtmlSelectElement;
    e.target()
        .and_then(|t| t.dyn_into::<HtmlSelectElement>().ok())
        .map(|s| s.value())
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
