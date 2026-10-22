//! matter-peek web UI (Yew/Wasm).
//!
//! A WebSocket/JSON client of the matter-peek API ([`matter_peek_core::api::wire`]): the devices on our
//! fabric with their live health, logs and updates; commissioning (with mDNS
//! discovery); and the OTA images.

use std::collections::HashMap;
use std::net::SocketAddr;

use futures::{SinkExt, StreamExt};
use gloo_net::websocket::{futures::WebSocket, Message};
use wasm_bindgen_futures::spawn_local;
use yew::prelude::*;

use matter_peek_core::api::{self, display, wire};

use assistant::{Assistant, AssistantMsg};
use camera::Camera;
use icons::{icon, Icon};
use network::{Confirm, NetworkView};

mod assistant;
mod camera;
mod commission;
mod devices;
mod icons;
mod memory;
mod network;
mod ui;
mod updates;

/// Where the server listens (see `matter-peek-server`'s default).
const WS_URL: &str = "ws://127.0.0.1:8087";

#[derive(Clone, PartialEq)]
struct StepLine {
    step: api::CommissionStep,
    status: api::StepStatus,
}

/// UI state for one live health subscription (per device).
#[derive(Clone, PartialEq)]
struct LiveSub {
    /// The running, merged health view (successive report deltas merged in).
    health: api::DeviceHealth,
    /// Connection state of this subscription (live / offline / …).
    conn: SubConn,
    /// How many reports the device's subscription has delivered.
    updates: u64,
}

impl Default for LiveSub {
    fn default() -> Self {
        Self {
            health: api::DeviceHealth::default(),
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

/// A Refresh request in flight.
#[derive(Clone, Copy, PartialEq)]
enum Loading {
    /// Of the device list.
    List,
    /// Of one device.
    Device(u64),
}

/// Which top-level tab is showing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Devices,
    Assistant,
    Commission,
    Updates,
}

/// How often the camera popup sends a frame to decode.
const CAMERA_FRAME_MS: u32 = 500;
/// How long to wait before reopening the subscription WS.
const SUB_WS_RETRY_MS: u32 = 2_000;
/// How long to wait before subscribing again to a device the server refused.
const SUBSCRIBE_RETRY_MS: u32 = 5_000;

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

/// A destructive action awaiting confirmation.
#[derive(Clone, PartialEq)]
enum Ask {
    Decommission(u64),
    DeleteImage(String),
    DeleteChat(String),
    /// The assistant's model, and its key.
    ForgetModel,
}

enum Msg {
    SelectTab(Tab),
    Assistant(AssistantMsg),
    /// In a device's memory popup.
    Memory(memory::MemoryMsg),
    /// Open or close the navigation drawer (on phones).
    ToggleDrawer,
    Ask(Ask),
    AskConfirmed,
    AskCancelled,
    DismissNotice,
    /// Clear the status line, if it is still the one numbered so.
    ClearStatus(u32),
    /// Edit the tags of a device, or stop editing.
    EditTags(Option<u64>),
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
    ToggleWifiPassword,
    /// Open the camera popup, to read the setup code from a QR code.
    OpenCamera,
    CloseCamera,
    CameraStream(web_sys::MediaStream),
    CameraFailed(String),
    /// Time to send a frame to decode.
    CameraTick,
    /// A frame was decoded: the setup code in it, if any.
    CameraDecoded(Option<String>),
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
    FetchLogs(api::LogIntent),
    /// Open the network popup of a device, reading its networking.
    OpenNetwork(u64),
    /// Read the networking of the device whose network popup is open again.
    RefreshNetwork,
    CloseNetwork,
    NetworkSsid(String),
    NetworkPassword(String),
    NetworkDataset(String),
    /// Add the network in the form to the networks on an endpoint, or update it.
    AddNetwork(u16, api::NetworkKind),
    /// Change the networks on an endpoint, once confirmed where it may cost the
    /// device its network.
    ChangeNetwork(u16, api::NetworkChange),
    ConfirmNetworkChange,
    CancelNetworkChange,
    NetworkTab(network::Tab),
    /// Open or close the form adding a network.
    ToggleAddNetwork,
    ToggleShowPassword,
    DismissError,
    /// A live health update arrived for a subscribed device (node_id, the
    /// subscription's report count, health). Deltas are merged into that
    /// device's running view.
    HealthReport(u64, u64, api::DeviceHealth),
    /// A subscription's connection state changed (node_id, state).
    SubState(u64, api::SubscriptionState),
    /// Subscription WS number `.0` is open and ready; its writer sink is
    /// attached. Any subscriptions queued before it opened are (re)sent.
    SubWsReady(u32, futures::channel::mpsc::UnboundedSender<String>),
    /// Subscription WS number `.0` closed (server gone / network): its
    /// subscriptions are gone, and a new WS takes them up again.
    SubWsClosed(u32),
    /// The server refused to subscribe to a device (node_id), e.g. one it does
    /// not list yet; retried after a while.
    SubscribeFailed(u64),
    /// Subscribe to a device (node_id) again, unless it is live meanwhile.
    Resubscribe(u64),
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
    OtaProgress(u64, api::OtaProgress),
    Step(StepLine),
    Done(wire::Response),
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
    show_wifi_password: bool,
    /// The camera popup, while open.
    camera: Option<Camera>,
    /// Its video.
    video_ref: NodeRef,
    /// The Thread operational dataset, hex.
    thread_dataset: String,
    running: bool,
    /// The discovery popup is showing.
    discover_open: bool,
    discovering: bool,
    /// A device op (list / peek / identify / …) is in flight.
    busy_stored: bool,
    /// Which Refresh button's request that is, for its spinner.
    loading: Option<Loading>,
    discovered: Vec<api::DiscoveredDevice>,
    discovered_ble: Vec<api::BleDevice>,
    /// The devices on our fabric.
    stored: Vec<api::DeviceInfo>,
    /// Per-device tag-input text (node_id → comma-separated edit buffer). Seeded
    /// from the device's current tags when the list arrives.
    tag_inputs: HashMap<u64, String>,
    /// Filter the device list to devices whose tags contain this substring.
    tag_filter: String,
    steps: Vec<StepLine>,
    result: Option<wire::Response>,
    /// Transient status line for device ops (identify ack, …).
    stored_status: Option<String>,
    /// Numbers the status line, so a stale timer does not clear a newer one.
    status_seq: u32,
    /// The navigation drawer is open (on phones).
    drawer_open: bool,
    /// The destructive action awaiting confirmation.
    ask: Option<Ask>,
    /// The device whose tags are being edited.
    editing_tags: Option<u64>,
    /// How an OTA image upload goes (percent), while it does.
    uploading: Option<u8>,
    /// The last progress of each device's update (node_id → progress).
    ota_progress: HashMap<u64, api::OtaProgress>,
    /// Live health subscriptions, keyed by device `node_id`. Multiple devices can
    /// be watched at once, each with its own running panel. An entry exists for as
    /// long as the device is being watched (removed on stop).
    subs: HashMap<u64, LiveSub>,
    /// The persistent WS to the server over which all subscriptions are driven
    /// (Subscribe/Unsubscribe out, HealthReport/SubscriptionState in). Opened
    /// lazily on the first subscribe; a `None` here means no live WS yet. The
    /// value is the sink to its writer task.
    sub_ws: Option<futures::channel::mpsc::UnboundedSender<String>>,
    /// Numbers the subscription WSs, so a stale one's close is told apart.
    sub_ws_gen: u32,
    /// A subscription WS is being opened.
    sub_ws_opening: bool,
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
    ota_images: Vec<api::OtaImageInfo>,
    /// The image chosen to update each device to (node_id → image name).
    ota_choice: HashMap<u64, String>,
    /// How each device's update goes (node_id → status line).
    ota_status: HashMap<u64, String>,
    /// The logs popup, when open.
    logs: Option<LogsView>,
    /// The memory popup, while open.
    memory: Option<memory::MemoryView>,
    /// The network popup, when open.
    network_view: Option<NetworkView>,
    /// The Assistant page.
    assistant: Assistant,
    /// Its conversation, which scrolls.
    chat_scroll: NodeRef,
    /// Scroll the conversation to its end once shown.
    scroll_chat: bool,
    error: Option<String>,
}

/// The logs popup of one device.
struct LogsView {
    node_id: u64,
    /// The log being retrieved or shown.
    intent: Option<api::LogIntent>,
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
            show_wifi_password: false,
            camera: None,
            video_ref: NodeRef::default(),
            thread_dataset: String::new(),
            running: false,
            discover_open: false,
            discovering: false,
            busy_stored: false,
            loading: None,
            discovered: Vec::new(),
            discovered_ble: Vec::new(),
            stored: Vec::new(),
            tag_inputs: HashMap::new(),
            tag_filter: String::new(),
            steps: Vec::new(),
            result: None,
            stored_status: None,
            status_seq: 0,
            drawer_open: false,
            ask: None,
            editing_tags: None,
            uploading: None,
            ota_progress: HashMap::new(),
            subs: HashMap::new(),
            sub_ws: None,
            sub_ws_gen: 0,
            sub_ws_opening: false,
            notice: None,
            last_commissioned_addr: None,
            confirm_recommission: false,
            ota_images: Vec::new(),
            ota_choice: HashMap::new(),
            ota_status: HashMap::new(),
            logs: None,
            memory: None,
            network_view: None,
            assistant: Assistant::default(),
            chat_scroll: NodeRef::default(),
            scroll_chat: false,
            error: None,
        }
    }

    fn update(&mut self, ctx: &Context<Self>, msg: Msg) -> bool {
        match msg {
            Msg::SelectTab(tab) => {
                self.tab = tab;
                self.drawer_open = false;
                if tab == Tab::Assistant {
                    ctx.link().send_message(Msg::Assistant(AssistantMsg::Load));
                    self.scroll_chat = true;
                }
                // Lazily refresh the device list the first time the user opens
                // the Devices tab with an empty list.
                if tab == Tab::Devices && self.stored.is_empty() && !self.busy_stored {
                    self.busy_stored = true;
                    self.loading = Some(Loading::List);
                    self.set_status(None);
                    send_request(ctx.link().clone(), wire::Request::ListDevices);
                }
                // Devices offer updates to the images.
                if matches!(tab, Tab::Devices | Tab::Updates) {
                    send_request(ctx.link().clone(), wire::Request::OtaImages);
                }
                true
            }
            Msg::Assistant(msg) => self.update_assistant(ctx, msg),
            Msg::Memory(msg) => self.update_memory(ctx, msg),
            Msg::ToggleDrawer => {
                self.drawer_open = !self.drawer_open;
                true
            }
            Msg::Ask(ask) => {
                self.ask = Some(ask);
                true
            }
            Msg::AskConfirmed => {
                match self.ask.take() {
                    Some(Ask::Decommission(node_id)) => {
                        ctx.link().send_message(Msg::Decommission(node_id))
                    }
                    Some(Ask::DeleteImage(name)) => ctx.link().send_message(Msg::OtaDelete(name)),
                    Some(Ask::DeleteChat(chat_id)) => {
                        if self
                            .assistant
                            .chat
                            .as_ref()
                            .is_some_and(|chat| chat.id == chat_id)
                        {
                            self.assistant.chat = None;
                        }
                        send_chat_request(
                            ctx.link().clone(),
                            wire::Request::ChatDelete { chat_id },
                        );
                    }
                    Some(Ask::ForgetModel) => {
                        ctx.link()
                            .send_message(Msg::Assistant(AssistantMsg::Settings(
                                assistant::SettingsMsg::Forget,
                            )))
                    }
                    None => {}
                }
                true
            }
            Msg::AskCancelled => {
                self.ask = None;
                true
            }
            Msg::DismissNotice => {
                self.notice = None;
                true
            }
            Msg::ClearStatus(seq) => {
                if seq != self.status_seq || self.busy_stored {
                    return false;
                }
                self.stored_status = None;
                true
            }
            Msg::EditTags(node_id) => {
                self.editing_tags = node_id;
                true
            }
            Msg::OtaImages => {
                send_request(ctx.link().clone(), wire::Request::OtaImages);
                false
            }
            Msg::QrImage(file) => {
                self.error = None;
                send_qr_image(ctx.link().clone(), file);
                false
            }
            Msg::OtaFile(file) => {
                self.uploading = Some(0);
                self.error = None;
                send_ota_upload(ctx.link().clone(), file);
                true
            }
            Msg::OtaUploading(percent) => {
                self.uploading = Some(percent);
                true
            }
            Msg::OtaDelete(name) => {
                send_request(ctx.link().clone(), wire::Request::OtaDelete { name });
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

                self.ota_progress.remove(&node_id);
                self.ota_status
                    .insert(node_id, format!("Announcing {name} to the device…"));
                send_ota_update(ctx.link().clone(), node_id, name);
                true
            }
            Msg::OtaProgress(node_id, progress) => {
                self.ota_status
                    .insert(node_id, ota_progress_text(&progress));
                self.ota_progress.insert(node_id, progress);
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
                self.error = None;
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
            Msg::ToggleWifiPassword => {
                self.show_wifi_password = !self.show_wifi_password;
                true
            }
            Msg::OpenCamera => {
                let camera = Camera::new();
                let running = camera.running.clone();
                self.camera = Some(camera);
                self.error = None;

                let link = ctx.link().clone();
                spawn_local(async move {
                    match camera::open().await {
                        Ok(stream) => link.send_message(Msg::CameraStream(stream)),
                        Err(e) => link.send_message(Msg::CameraFailed(e)),
                    }
                });
                let link = ctx.link().clone();
                spawn_local(async move {
                    while running.get() {
                        gloo_timers::future::TimeoutFuture::new(CAMERA_FRAME_MS).await;
                        if running.get() {
                            link.send_message(Msg::CameraTick);
                        }
                    }
                });
                true
            }
            Msg::CloseCamera => {
                if let Some(camera) = self.camera.take() {
                    camera.stop();
                }
                true
            }
            Msg::CameraStream(stream) => {
                // Closed meanwhile.
                let Some(camera) = self.camera.as_mut() else {
                    camera::stop(&stream);
                    return false;
                };
                if let Some(video) = self.video_ref.cast::<web_sys::HtmlVideoElement>() {
                    video.set_src_object(Some(&stream));
                    let _ = video.play();
                }
                camera.stream = Some(stream);
                true
            }
            Msg::CameraFailed(e) => {
                if let Some(camera) = self.camera.as_mut() {
                    camera.running.set(false);
                    camera.error = Some(e);
                }
                true
            }
            Msg::CameraTick => {
                let Some(camera) = self.camera.as_mut() else {
                    return false;
                };
                if camera.decoding || camera.stream.is_none() {
                    return false;
                }
                let Some(image) = self
                    .video_ref
                    .cast::<web_sys::HtmlVideoElement>()
                    .and_then(|video| camera::frame(&video))
                else {
                    return false;
                };
                camera.decoding = true;
                send_camera_frame(ctx.link().clone(), image);
                false
            }
            Msg::CameraDecoded(code) => {
                let Some(camera) = self.camera.as_mut() else {
                    return false;
                };
                camera.decoding = false;
                let Some(code) = code else {
                    return false;
                };
                camera.stop();
                self.camera = None;
                self.setup_code = code;
                self.set_status(Some("Read the setup code from the QR code.".into()));
                self.fade_status(ctx);
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
                self.loading = Some(Loading::List);
                self.set_status(None);
                self.error = None;
                send_request(ctx.link().clone(), wire::Request::ListDevices);
                true
            }
            Msg::Sync => {
                self.busy_stored = true;
                self.set_status(Some("Syncing with the devices on our fabric…".into()));
                self.error = None;
                self.notice = None;
                send_request(ctx.link().clone(), wire::Request::Sync);
                true
            }
            Msg::Refresh(node_id) => {
                // One-shot re-read — the result updates this device's card panel
                // (not a status line). Useful for the values the subscription
                // doesn't push on every change (e.g. the live-computed uptime).
                self.busy_stored = true;
                self.loading = Some(Loading::Device(node_id));
                self.error = None;
                send_request(ctx.link().clone(), wire::Request::PeekDevice { node_id });
                true
            }
            Msg::Remember(node_id) => {
                self.busy_stored = true;
                self.error = None;
                send_request(ctx.link().clone(), wire::Request::Remember { node_id });
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
                self.error = None;
                true
            }
            Msg::OpenNetwork(node_id) => {
                self.error = None;
                self.network_view = Some(NetworkView::new(node_id));
                send_request(ctx.link().clone(), wire::Request::ReadNetwork { node_id });
                true
            }
            Msg::RefreshNetwork => {
                let Some(network) = self.network_view.as_mut() else {
                    return false;
                };
                network.loading = true;
                self.error = None;
                send_request(
                    ctx.link().clone(),
                    wire::Request::ReadNetwork {
                        node_id: network.node_id,
                    },
                );
                true
            }
            Msg::CloseNetwork => {
                self.network_view = None;
                self.error = None;
                true
            }
            Msg::NetworkSsid(ssid) => {
                if let Some(view) = self.network_view.as_mut() {
                    view.ssid = ssid;
                }
                true
            }
            Msg::NetworkPassword(password) => {
                if let Some(view) = self.network_view.as_mut() {
                    view.password = password;
                }
                true
            }
            Msg::NetworkDataset(dataset) => {
                if let Some(view) = self.network_view.as_mut() {
                    view.dataset = dataset;
                }
                true
            }
            Msg::AddNetwork(endpoint, kind) => {
                let Some(view) = self.network_view.as_ref() else {
                    return false;
                };
                let credentials = match kind {
                    api::NetworkKind::Thread => api::NetworkCredentials::Thread {
                        dataset: view.dataset.trim().to_string(),
                    },
                    _ => api::NetworkCredentials::Wifi {
                        ssid: view.ssid.trim().to_string(),
                        password: view.password.clone(),
                    },
                };
                ctx.link().send_message(Msg::ChangeNetwork(
                    endpoint,
                    api::NetworkChange::AddOrUpdate { credentials },
                ));
                false
            }
            Msg::ChangeNetwork(endpoint, change) => {
                let Some(view) = self.network_view.as_mut() else {
                    return false;
                };
                self.error = None;
                match Confirm::of(view.info.as_ref(), endpoint, change) {
                    Ok(confirm) => view.confirm = Some(confirm),
                    Err(change) => view.change(ctx.link(), endpoint, change),
                }
                true
            }
            Msg::ConfirmNetworkChange => {
                let Some(view) = self.network_view.as_mut() else {
                    return false;
                };
                if let Some(confirm) = view.confirm.take() {
                    view.change(ctx.link(), confirm.endpoint, confirm.change);
                }
                true
            }
            Msg::CancelNetworkChange => {
                if let Some(view) = self.network_view.as_mut() {
                    view.confirm = None;
                }
                true
            }
            Msg::NetworkTab(tab) => {
                if let Some(view) = self.network_view.as_mut() {
                    view.tab = Some(tab);
                }
                true
            }
            Msg::ToggleAddNetwork => {
                if let Some(view) = self.network_view.as_mut() {
                    view.adding = !view.adding;
                }
                true
            }
            Msg::ToggleShowPassword => {
                if let Some(view) = self.network_view.as_mut() {
                    view.show_password = !view.show_password;
                }
                true
            }
            Msg::DismissError => {
                self.error = None;
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
                    wire::Request::RetrieveLogs {
                        node_id: logs.node_id,
                        intent,
                    },
                );
                true
            }
            Msg::Identify(node_id) => {
                self.busy_stored = true;
                self.set_status(Some(format!(
                    "Asking node {} to identify itself…",
                    display::hex_u64(node_id)
                )));
                self.error = None;
                send_request(
                    ctx.link().clone(),
                    wire::Request::Identify { node_id, secs: 3 },
                );
                true
            }
            Msg::Decommission(node_id) => {
                self.busy_stored = true;
                self.set_status(Some(format!(
                    "Decommissioning node {}…",
                    display::hex_u64(node_id)
                )));
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
                    wire::Request::Decommission {
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
                    api::SubscriptionState::Live { max_int_secs } => {
                        if let Some(sub) = self.subs.get_mut(&node_id) {
                            sub.conn = SubConn::Live { max_int_secs };
                        }
                    }
                    api::SubscriptionState::Offline => {
                        if let Some(sub) = self.subs.get_mut(&node_id) {
                            sub.conn = SubConn::Offline;
                        }
                    }
                    // Terminal. A device we still watch and still store gets
                    // re-subscribed; one we stopped watching (e.g. being
                    // decommissioned) or that is gone from the store is dropped.
                    api::SubscriptionState::Stopped => {
                        let still_stored = self.stored.iter().any(|d| d.handle.node_id == node_id);
                        match self.subs.get_mut(&node_id) {
                            Some(sub) if still_stored => {
                                sub.conn = SubConn::Reconnecting;
                                self.sub_send(&wire::Request::Subscribe { node_id });
                            }
                            _ => {
                                self.subs.remove(&node_id);
                            }
                        }
                    }
                }
                true
            }
            Msg::SubWsReady(gen, tx) => {
                if gen != self.sub_ws_gen {
                    return false;
                }
                self.sub_ws = Some(tx);
                self.sub_ws_opening = false;
                // (Re)send Subscribe for every device we intend to watch — covers
                // the initial open and any reconnect.
                let nodes: Vec<u64> = self.subs.keys().copied().collect();
                for node_id in nodes {
                    self.sub_send(&wire::Request::Subscribe { node_id });
                }
                true
            }
            Msg::SubWsClosed(gen) => {
                if gen != self.sub_ws_gen {
                    return false;
                }
                self.sub_ws = None;
                self.sub_ws_opening = false;
                // The connection dropped → every subscription is dead. Mark them
                // reconnecting and reopen the WS (which re-subscribes on ready), so
                // the dashboard self-heals without user action.
                for sub in self.subs.values_mut() {
                    sub.conn = SubConn::Reconnecting;
                }
                if !self.subs.is_empty() {
                    self.open_sub_ws(ctx, SUB_WS_RETRY_MS);
                }
                true
            }
            Msg::SubscribeFailed(node_id) => {
                let Some(sub) = self.subs.get_mut(&node_id) else {
                    return false;
                };
                sub.conn = SubConn::Connecting;
                let link = ctx.link().clone();
                spawn_local(async move {
                    gloo_timers::future::TimeoutFuture::new(SUBSCRIBE_RETRY_MS).await;
                    link.send_message(Msg::Resubscribe(node_id));
                });
                true
            }
            Msg::Resubscribe(node_id) => {
                if self
                    .subs
                    .get(&node_id)
                    .is_some_and(|sub| !matches!(sub.conn, SubConn::Live { .. }))
                {
                    self.sub_send(&wire::Request::Subscribe { node_id });
                }
                false
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
                self.editing_tags = None;
                self.set_status(Some(format!(
                    "Saving the tags of node {}…",
                    display::hex_u64(node_id)
                )));
                self.error = None;
                send_request(ctx.link().clone(), wire::Request::SetTags { node_id, tags });
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
                self.loading = None;
                if let Some(logs) = self.logs.as_mut() {
                    logs.loading = false;
                }
                if let Some(network) = self.network_view.as_mut() {
                    network.loading = false;
                }
                match resp {
                    wire::Response::Discovered {
                        devices,
                        ble_devices,
                    } => {
                        self.discovered = devices;
                        self.discovered_ble = ble_devices;
                    }
                    wire::Response::SetupCode { setup_code } => self.setup_code = setup_code,
                    wire::Response::Devices { devices, notice } => {
                        self.set_status(None);
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
                    wire::Response::Peek { node_id, health } => {
                        // A Refresh result: merge the fresh read into the device's
                        // card panel so its values (uptime, heap, …) update in place.
                        self.subs.entry(node_id).or_default().health.merge(health);
                    }
                    wire::Response::Ok => {
                        self.set_status(Some(
                            "Identify acknowledged: the device should blink.".into(),
                        ));
                    }
                    // `Subscribed` / streamed subscription frames arrive on the
                    // dedicated persistent subscription WS, not this one-shot path.
                    wire::Response::Subscribed { .. } => {}
                    wire::Response::OtaImages { images } => {
                        self.uploading = None;
                        self.ota_images = images;
                    }
                    // Uploads run on their own connection (`send_ota_upload`).
                    wire::Response::OtaUploadStarted { .. } => {}
                    // The Assistant page's requests answer to it.
                    // The Assistant page's own.
                    wire::Response::Chats { .. }
                    | wire::Response::Chat { .. }
                    | wire::Response::Assistant { .. }
                    | wire::Response::Models { .. }
                    | wire::Response::Tested { .. } => {}
                    wire::Response::Cancelled { decommissioned } => {
                        self.notice = Some(if decommissioned {
                            "Commission cancelled — the device had been added to our fabric and was cleaned up (decommissioned).".into()
                        } else {
                            "Commission cancelled before the device was modified.".into()
                        });
                    }
                    wire::Response::Network { node_id, network } => {
                        // The card's icon, before the next listing has it.
                        let kind = network
                            .configs
                            .iter()
                            .find(|config| config.endpoint == 0)
                            .and_then(|config| config.kind);
                        if let Some(d) =
                            self.stored.iter_mut().find(|d| d.handle.node_id == node_id)
                        {
                            d.network = kind.or(d.network);
                        }
                        if let Some(view) =
                            self.network_view.as_mut().filter(|n| n.node_id == node_id)
                        {
                            view.info = Some(*network);
                            if view.changing {
                                view.changed();
                            }
                        }
                    }
                    wire::Response::Logs { node_id, data, .. } => {
                        if let Some(logs) = self.logs.as_mut().filter(|l| l.node_id == node_id) {
                            logs.data = Some(data);
                        }
                    }
                    // The device is listed now: refresh the list and surface it on
                    // the Devices tab.
                    other @ wire::Response::Commissioned(_) => {
                        // Its commissioning window is closed now — remember its
                        // address to guard a re-commission.
                        if !self.via_ble {
                            self.last_commissioned_addr = Some(self.peer_addr.trim().to_string());
                        }
                        if let wire::Response::Commissioned(result) = &other {
                            if let Some(node_id) = result.rematch_candidate {
                                self.notice = Some(rematch_notice(node_id));
                            }
                        }
                        self.result = Some(other);
                        self.busy_stored = true;
                        self.loading = Some(Loading::List);
                        send_request(ctx.link().clone(), wire::Request::ListDevices);
                        self.tab = Tab::Devices;
                    }
                    wire::Response::Error { message } => self.error = Some(message),
                }
                if let Some(network) = self.network_view.as_mut() {
                    network.changing = false;
                }
                self.fade_status(ctx);
                true
            }
            Msg::Error(e) => {
                self.uploading = None;
                self.running = false;
                self.discovering = false;
                self.busy_stored = false;
                self.loading = None;
                if let Some(network) = self.network_view.as_mut() {
                    network.loading = false;
                    network.changing = false;
                }
                self.error = Some(e);
                self.set_status(None);
                true
            }
        }
    }

    fn view(&self, ctx: &Context<Self>) -> Html {
        let link = ctx.link();
        // A popup shows the error itself.
        let popup = self.discover_open
            || self.logs.is_some()
            || self.network_view.is_some()
            || self.camera.is_some();

        html! {
            <>
                { self.view_app_bar(link) }
                <main class={classes!("page", match self.tab {
                    Tab::Devices => None,
                    Tab::Assistant => Some("is-chat"),
                    Tab::Commission | Tab::Updates => Some("is-narrow"),
                })}>
                    if let Some(notice) = &self.notice {
                        { ui::notification(Icon::Info, classes!("is-warning", "is-light"), html! { notice.clone() },
                            link.callback(|_| Msg::DismissNotice)) }
                    }
                    if let Some(e) = self.error.as_ref().filter(|_| !popup) {
                        { ui::notification(Icon::Alert, classes!("is-danger", "is-light"), html! { e.clone() },
                            link.callback(|_| Msg::DismissError)) }
                    }
                    {
                        match self.tab {
                            Tab::Devices => self.view_devices_tab(link),
                            Tab::Assistant => self.view_assistant_tab(link),
                            Tab::Commission => self.view_commission_tab(link),
                            Tab::Updates => self.view_updates_tab(link),
                        }
                    }
                </main>
                { self.view_status(link) }
                { self.view_discover_modal(link) }
                { self.view_camera_modal(link) }
                { self.view_logs_modal(link) }
                { self.view_memory_modal(link) }
                { self.view_settings_modal(link) }
                { self.view_network_modal(link) }
                { self.view_ask(link) }
            </>
        }
    }

    fn rendered(&mut self, ctx: &Context<Self>, first_render: bool) {
        // The app opens on the Devices tab; load it as if the user picked it.
        if first_render {
            ctx.link().send_message(Msg::SelectTab(Tab::Devices));
        }

        if std::mem::take(&mut self.scroll_chat) {
            if let Some(scroll) = self.chat_scroll.cast::<web_sys::Element>() {
                scroll.set_scroll_top(scroll.scroll_height());
            }
        }
    }
}

impl App {
    /// Show `status` in the status line, or clear it.
    fn set_status(&mut self, status: Option<String>) {
        self.stored_status = status;
        self.status_seq = self.status_seq.wrapping_add(1);
    }

    /// Clear the status line after a while, unless it changes meanwhile.
    fn fade_status(&mut self, ctx: &Context<Self>) {
        if self.stored_status.is_none() {
            return;
        }
        let seq = self.status_seq;
        let link = ctx.link().clone();
        spawn_local(async move {
            gloo_timers::future::TimeoutFuture::new(4_000).await;
            link.send_message(Msg::ClearStatus(seq));
        });
    }

    /// Send a request over the **persistent subscription WS** (Subscribe /
    /// Unsubscribe). No-op if the WS isn't open yet — `SubWsReady` re-sends the
    /// active subscriptions once it opens.
    fn sub_send(&self, request: &wire::Request) {
        if let Some(tx) = &self.sub_ws {
            // A Subscribe's answer names its device by its req_id.
            let req_id = match request {
                wire::Request::Subscribe { node_id } => *node_id,
                _ => 0,
            };
            let msg = wire::ClientMsg {
                req_id,
                request: request.clone(),
            };
            if let Ok(json) = serde_json::to_string(&msg) {
                let _ = tx.unbounded_send(json);
            }
        }
    }

    /// Open the persistent subscription WS after `delay_ms`, unless it is open
    /// or opening, spawning its reader (server frames → `Msg`) and writer (outbound requests) tasks. All
    /// live subscriptions share this one connection; it stays open while any
    /// device is being watched. The server ties each subscription to this
    /// connection, so closing it stops them all server-side too.
    fn open_sub_ws(&mut self, ctx: &Context<Self>, delay_ms: u32) {
        if self.sub_ws.is_some() || self.sub_ws_opening {
            return;
        }
        self.sub_ws_opening = true;
        self.sub_ws_gen = self.sub_ws_gen.wrapping_add(1);
        let gen = self.sub_ws_gen;
        let link = ctx.link().clone();
        spawn_local(async move {
            if delay_ms > 0 {
                gloo_timers::future::TimeoutFuture::new(delay_ms).await;
            }
            let ws = match WebSocket::open(WS_URL) {
                Ok(ws) => ws,
                Err(_) => {
                    link.send_message(Msg::SubWsClosed(gen));
                    return;
                }
            };
            let (mut write, mut read) = ws.split();

            // Outbound: a channel the UI writes request-JSON into; drain to the WS.
            let (out_tx, mut out_rx) = futures::channel::mpsc::unbounded::<String>();
            link.send_message(Msg::SubWsReady(gen, out_tx));

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
                    let Ok(msg) = serde_json::from_str::<wire::ServerMsg>(&text) else {
                        continue;
                    };
                    match msg {
                        wire::ServerMsg::Event { event } => match event {
                            wire::Event::HealthReport {
                                node_id,
                                seq,
                                health,
                            } => link.send_message(Msg::HealthReport(node_id, seq, health)),
                            wire::Event::SubscriptionState { node_id, state } => {
                                link.send_message(Msg::SubState(node_id, state))
                            }
                            wire::Event::OtaProgress { node_id, progress } => {
                                link.send_message(Msg::OtaProgress(node_id, progress))
                            }
                            wire::Event::Step { .. } | wire::Event::Chat { .. } => {}
                        },
                        // A Subscribe refused (its req_id is its device).
                        wire::ServerMsg::Response {
                            req_id,
                            response: wire::Response::Error { .. },
                        } if req_id != 0 => link.send_message(Msg::SubscribeFailed(req_id)),
                        // `Subscribed` acks: the streamed events drive the UI.
                        wire::ServerMsg::Response { .. } => {}
                    }
                }
            };

            // Run until either half ends (connection closed), then tell the UI.
            futures::pin_mut!(reader, writer);
            futures::future::select(reader, writer).await;
            link.send_message(Msg::SubWsClosed(gen));
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

        self.open_sub_ws(ctx, 0);

        // Subscribe to any stored device we're not already watching, and again
        // to any still waiting (the server ignores a repeat).
        for node_id in &stored_ids {
            let waiting = self
                .subs
                .get(node_id)
                .map(|sub| matches!(sub.conn, SubConn::Connecting | SubConn::Reconnecting));
            if waiting.is_none() {
                self.subs.insert(*node_id, LiveSub::default());
            }
            if waiting != Some(false) {
                self.sub_send(&wire::Request::Subscribe { node_id: *node_id });
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
            self.sub_send(&wire::Request::Unsubscribe { node_id });
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
            wire::Request::Discover {
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
    fn commission_via(&self) -> api::CommissionVia {
        if !self.via_ble {
            return api::CommissionVia::Ip {
                addr: self.peer_addr.trim().to_string(),
            };
        }

        let addr = self.ble_addr.trim();
        let network = match self.network {
            Network::Wifi => Some(api::NetworkCredentials::Wifi {
                ssid: self.wifi_ssid.clone(),
                password: self.wifi_password.clone(),
            }),
            Network::Thread => Some(api::NetworkCredentials::Thread {
                dataset: self.thread_dataset.split_whitespace().collect(),
            }),
            Network::None => None,
        };

        api::CommissionVia::Ble {
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

        let req = wire::Request::Commission {
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

    /// The top bar: the brand, the pages and, apart from them, the assistant;
    /// all fold into a drawer on phones.
    fn view_app_bar(&self, link: &html::Scope<Self>) -> Html {
        let pages = [
            (
                Tab::Devices,
                "Devices",
                Icon::Devices,
                Some(self.stored.len()),
            ),
            (Tab::Commission, "Commission", Icon::Commission, None),
            (
                Tab::Updates,
                "Updates",
                Icon::Updates,
                Some(self.ota_images.len()),
            ),
        ];
        let link_to =
            |class: &'static str, (tab, label, glyph, count): (Tab, &str, Icon, Option<usize>)| {
                html! {
                    <a class={classes!(class, (self.tab == tab).then_some("is-active"))}
                        onclick={link.callback(move |_| Msg::SelectTab(tab))}>
                        <span class="nav-icon">{ icon(glyph, 18) }</span>
                        <span>{ label.to_string() }</span>
                        if let Some(count) = count.filter(|count| *count > 0) {
                            <span class="nav-count">{ count }</span>
                        }
                    </a>
                }
            };
        let assistant = (self.tab == Tab::Assistant).then_some("is-active");
        let open_assistant = link.callback(|_| Msg::SelectTab(Tab::Assistant));

        html! {
            <>
                <header class="appbar">
                    <div class="appbar-inner">
                        <button class="appbar-burger" aria-label="Menu"
                            onclick={link.callback(|_| Msg::ToggleDrawer)}>
                            { icon(Icon::Menu, 22) }
                        </button>
                        <a class="brand" onclick={link.callback(|_| Msg::SelectTab(Tab::Devices))}>
                            <span class="brand-mark">{ icon(Icon::Logo, 20) }</span>
                            <span class="brand-name">{ "matter-peek" }</span>
                        </a>
                        <nav class="appbar-nav">
                            { for pages.into_iter().map(|page| link_to("appbar-link", page)) }
                            <a class={classes!("appbar-link", "is-assistant", assistant)}
                                onclick={open_assistant.clone()}>
                                <span class="assistant-mark">{ icon(Icon::Sparkles, 13) }</span>
                                <span>{ "Assistant" }</span>
                            </a>
                        </nav>
                    </div>
                </header>

                <div class={classes!("drawer-backdrop", self.drawer_open.then_some("is-open"))}
                    onclick={link.callback(|_| Msg::ToggleDrawer)}></div>
                <aside class={classes!("drawer", self.drawer_open.then_some("is-open"))}>
                    <div class="drawer-head">
                        <span class="brand-mark">{ icon(Icon::Logo, 22) }</span>
                        <div>
                            <p class="brand-name">{ "matter-peek" }</p>
                            <p class="drawer-tagline">{ "The Matter diagnostics tool" }</p>
                        </div>
                    </div>
                    <nav class="drawer-nav">
                        { for pages.into_iter().map(|page| link_to("drawer-link", page)) }
                    </nav>
                    <div class="drawer-foot">
                        <a class={classes!("drawer-assistant", assistant)} onclick={open_assistant}>
                            <span class="assistant-mark is-large">{ icon(Icon::Sparkles, 18) }</span>
                            <span class="drawer-assistant-text">
                                <span class="drawer-assistant-title">{ "Assistant" }</span>
                                <span class="drawer-assistant-sub">{ "Ask about your devices" }</span>
                            </span>
                            <span class="drawer-assistant-go">{ icon(Icon::ChevronRight, 18) }</span>
                        </a>
                    </div>
                </aside>
            </>
        }
    }

    /// The status line of device operations, at the bottom.
    fn view_status(&self, link: &html::Scope<Self>) -> Html {
        let Some(status) = &self.stored_status else {
            return html! {};
        };
        let seq = self.status_seq;

        html! {
            <div class="snackbar">
                if self.busy_stored {
                    <span class="loader"></span>
                } else {
                    <span class="snackbar-icon">{ icon(Icon::CheckCircle, 18) }</span>
                }
                <span class="snackbar-text">{ status.clone() }</span>
                <button class="snackbar-close" aria-label="Dismiss"
                    onclick={link.callback(move |_| Msg::ClearStatus(seq))}>
                    { icon(Icon::Close, 16) }
                </button>
            </div>
        }
    }

    /// The confirmation of a destructive action.
    fn view_ask(&self, link: &html::Scope<Self>) -> Html {
        let Some(ask) = &self.ask else {
            return html! {};
        };

        let (title, text, action) = match ask {
            Ask::Decommission(node_id) => {
                let title = self
                    .stored
                    .iter()
                    .find(|d| d.handle.node_id == *node_id)
                    .map(api::DeviceInfo::title)
                    .unwrap_or_else(|| format!("node {}", display::hex_u64(*node_id)));
                (
                    format!("Decommission {title}?"),
                    "The device leaves our fabric and this list; the other ecosystems it is in \
                     keep it. To get it back, commission it again."
                        .to_string(),
                    "Decommission",
                )
            }
            Ask::DeleteChat(_) => (
                "Delete this chat?".to_string(),
                "Its conversation is gone for good.".to_string(),
                "Delete",
            ),
            Ask::ForgetModel => (
                "Forget the assistant's model?".to_string(),
                "Its API key goes too. The chats stay; the assistant answers again once it has a model."
                    .to_string(),
                "Forget",
            ),
            Ask::DeleteImage(name) => (
                format!("Delete {name}?"),
                "Devices can no longer update to this image.".to_string(),
                "Delete",
            ),
        };

        ui::confirm_dialog(
            &title,
            &text,
            action,
            true,
            link.callback(|_| Msg::AskConfirmed),
            link.callback(|_| Msg::AskCancelled),
        )
    }
}

/// The notice for a just-remembered device that may be a unit already listed.
fn rematch_notice(node_id: u64) -> String {
    format!(
        "Possible match: the device is the same model as device {} — \
         check the Devices tab whether they are the same unit.",
        display::hex_u64(node_id)
    )
}

fn log_intent_name(intent: api::LogIntent) -> &'static str {
    match intent {
        api::LogIntent::EndUserSupport => "End-user support",
        api::LogIntent::NetworkDiag => "Network diagnostics",
        api::LogIntent::CrashLogs => "Crash",
    }
}

fn log_intent_file(intent: api::LogIntent) -> &'static str {
    match intent {
        api::LogIntent::EndUserSupport => "support",
        api::LogIntent::NetworkDiag => "network",
        api::LogIntent::CrashLogs => "crash",
    }
}

/// Open a WS, send the request, and pump server frames back as component
/// messages (step events, then the final response) until it completes.
fn send_request(link: html::Scope<App>, request: wire::Request) {
    request_loop(link, request, Msg::Done, Msg::Error);
}

/// Send a request of the Assistant page: its answer goes to the page.
fn send_chat_request(link: html::Scope<App>, request: wire::Request) {
    request_loop(
        link,
        request,
        |response| Msg::Assistant(AssistantMsg::Response(response)),
        |e| Msg::Assistant(AssistantMsg::Failed(e)),
    );
}

/// Open a WS, send `request`, and pump the server's frames back as messages:
/// the events, then the response as `done`, or `failed` on the way.
fn request_loop(
    link: html::Scope<App>,
    request: wire::Request,
    done: fn(wire::Response) -> Msg,
    failed: fn(String) -> Msg,
) {
    spawn_local(async move {
        let ws = match WebSocket::open(WS_URL) {
            Ok(ws) => ws,
            Err(e) => {
                link.send_message(failed(format!("connect failed: {e}")));
                return;
            }
        };
        let (mut write, mut read) = ws.split();

        let req_id: wire::ReqId = 1;
        let client_msg = wire::ClientMsg { req_id, request };
        let json = serde_json::to_string(&client_msg).unwrap();
        if let Err(e) = write.send(Message::Text(json)).await {
            link.send_message(failed(format!("send failed: {e}")));
            return;
        }

        // `req_id`-keyed routing is trivial here (one in-flight request) but kept
        // explicit to mirror the multi-request server contract.
        let mut _pending: HashMap<wire::ReqId, ()> = HashMap::new();
        _pending.insert(req_id, ());

        while let Some(frame) = read.next().await {
            let text = match frame {
                Ok(Message::Text(t)) => t,
                Ok(Message::Bytes(_)) => continue,
                Err(e) => {
                    link.send_message(failed(format!("ws error: {e}")));
                    return;
                }
            };
            let server_msg: wire::ServerMsg = match serde_json::from_str(&text) {
                Ok(m) => m,
                Err(e) => {
                    link.send_message(failed(format!("bad server msg: {e}")));
                    return;
                }
            };
            match server_msg {
                wire::ServerMsg::Event { event } => match event {
                    wire::Event::Step { step, status, .. } => {
                        link.send_message(Msg::Step(StepLine { step, status }));
                    }
                    // Subscription events flow on the dedicated persistent WS, not
                    // this one-shot connection — ignore any that land here.
                    wire::Event::HealthReport { .. }
                    | wire::Event::SubscriptionState { .. }
                    | wire::Event::OtaProgress { .. } => {}
                    wire::Event::Chat { chat_id, update } => {
                        link.send_message(Msg::Assistant(AssistantMsg::Update(chat_id, update)));
                    }
                },
                wire::ServerMsg::Response { response, .. } => {
                    link.send_message(done(response));
                    return;
                }
            }
        }
    });
}

/// Send a camera frame to decode, answering with the setup code in it, if any.
fn send_camera_frame(link: html::Scope<App>, image: String) {
    spawn_local(async move {
        let code = match exchange(wire::Request::ReadQrImage { image }).await {
            Ok(wire::Response::SetupCode { setup_code }) => Some(setup_code),
            _ => None,
        };
        link.send_message(Msg::CameraDecoded(code));
    });
}

/// Send `request` on its own WS, answering with its response (events left
/// out).
async fn exchange(request: wire::Request) -> Result<wire::Response, String> {
    let ws = WebSocket::open(WS_URL).map_err(|e| e.to_string())?;
    let (mut write, mut read) = ws.split();

    let json = serde_json::to_string(&wire::ClientMsg { req_id: 1, request })
        .map_err(|e| e.to_string())?;
    write
        .send(Message::Text(json))
        .await
        .map_err(|e| e.to_string())?;

    while let Some(frame) = read.next().await {
        let Message::Text(text) = frame.map_err(|e| e.to_string())? else {
            continue;
        };
        if let Ok(wire::ServerMsg::Response { response, .. }) = serde_json::from_str(&text) {
            return Ok(response);
        }
    }

    Err("the connection closed".into())
}

/// The size of an upload chunk, before base64.
const UPLOAD_CHUNK_LEN: u64 = 256 * 1024;

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
                wire::Request::ReadQrImage {
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

    let begin = wire::Request::OtaUploadBegin { size };
    let upload_id = match call(&mut write, &mut read, &mut req_id, begin).await? {
        wire::Response::OtaUploadStarted { upload_id } => upload_id,
        other => return Err(response_error(other)),
    };

    let mut offset = 0;
    while offset < size {
        let end = (offset + UPLOAD_CHUNK_LEN).min(size);
        let chunk = gloo_file::futures::read_as_bytes(&file.slice(offset, end))
            .await
            .map_err(|e| format!("cannot read the file: {e}"))?;

        let request = wire::Request::OtaUploadChunk {
            upload_id,
            offset,
            data: base64::engine::general_purpose::STANDARD.encode(chunk),
        };
        match call(&mut write, &mut read, &mut req_id, request).await? {
            wire::Response::Ok => (),
            other => return Err(response_error(other)),
        }

        offset = end;
        link.send_message(Msg::OtaUploading((offset * 100 / size.max(1)) as u8));
    }

    let end = wire::Request::OtaUploadEnd { upload_id };
    let images = call(&mut write, &mut read, &mut req_id, end).await?;
    link.send_message(Msg::Done(images));

    Ok(())
}

/// Send `request` with the next `req_id`, and wait for its response.
async fn call<W, R>(
    write: &mut W,
    read: &mut R,
    req_id: &mut wire::ReqId,
    request: wire::Request,
) -> Result<wire::Response, String>
where
    W: futures::Sink<Message, Error = gloo_net::websocket::WebSocketError> + Unpin,
    R: futures::Stream<Item = Result<Message, gloo_net::websocket::WebSocketError>> + Unpin,
{
    *req_id += 1;
    let json = serde_json::to_string(&wire::ClientMsg {
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

        if let Ok(wire::ServerMsg::Response {
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

fn response_error(response: wire::Response) -> String {
    match response {
        wire::Response::Error { message } => message,
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

        let client_msg = wire::ClientMsg {
            req_id: 1,
            request: wire::Request::OtaUpdate { node_id, name },
        };
        let json = serde_json::to_string(&client_msg).unwrap();
        if let Err(e) = write.send(Message::Text(json)).await {
            link.send_message(Msg::Error(format!("send failed: {e}")));
            return;
        }

        while let Some(Ok(Message::Text(text))) = read.next().await {
            match serde_json::from_str(&text) {
                Ok(wire::ServerMsg::Event {
                    event: wire::Event::OtaProgress { node_id, progress },
                }) => {
                    let applied = matches!(progress, api::OtaProgress::Applied { .. });
                    link.send_message(Msg::OtaProgress(node_id, progress));
                    if applied {
                        return;
                    }
                }
                Ok(wire::ServerMsg::Response {
                    response: wire::Response::Error { message },
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
fn ota_progress_text(progress: &api::OtaProgress) -> String {
    match progress {
        api::OtaProgress::Offered { name } => format!("The device asked; offered {name}"),
        api::OtaProgress::NotOffered => {
            "The device asked, but the image is not an update for it".into()
        }
        api::OtaProgress::Downloading { percent } => format!("Downloading: {percent}%"),
        api::OtaProgress::Applying { version } => format!("Applying version {version}…"),
        api::OtaProgress::Applied { version } => format!("Updated: runs version {version}"),
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
        let client_msg = wire::ClientMsg {
            req_id: 1,
            request: wire::Request::Cancel,
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
fn phase_name(step: api::CommissionStep) -> &'static str {
    match step {
        api::CommissionStep::Commission => "Commissioning",
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
