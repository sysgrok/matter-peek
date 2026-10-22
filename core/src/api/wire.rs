//! The API as messages, for the WebSocket server, the web UI and the clients
//! that speak JSON to it.
//!
//! The client sends [`ClientMsg`]s, each a [`Request`] tagged with a `req_id`;
//! the server answers each with one [`ServerMsg::Response`] carrying the same
//! `req_id`, and sends [`ServerMsg::Event`]s in between: a commission's steps,
//! the reports of the connection's subscriptions, OTA progress, what the
//! assistant does. [`WireSession`] runs them against a
//! [`MatterPeek`](super::MatterPeek); `matter-peek-server` also runs the
//! assistant's chats (the `Chat…` requests).

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use super::agent::{Message, ToolCall};
use super::types::*;

#[cfg(feature = "controller")]
pub use session::WireSession;

#[cfg(feature = "controller")]
mod session;

/// A correlation id chosen by the client and echoed on the matching response.
pub type ReqId = u64;

/// Anything the client may send.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientMsg {
    pub req_id: ReqId,
    pub request: Request,
}

/// Anything the server may send: either a reply to a request, or an event.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ServerMsg {
    /// Reply to the request with this `req_id`.
    Response { req_id: ReqId, response: Response },
    /// An event, of a request or of the connection's subscriptions.
    Event { event: Event },
}

/// A request; each mirrors a [`MatterPeek`](super::MatterPeek) method, but the
/// `Chat…` ones: the server's assistant (see [`super::agent`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// See `MatterPeek::commission`. Progress arrives as [`Event::Step`]s; the
    /// answer is [`Response::Commissioned`], or [`Response::Cancelled`].
    Commission {
        via: CommissionVia,
        /// The device's passcode, manual pairing code or QR code payload.
        setup_code: String,
        remember: bool,
    },
    /// See `MatterPeek::read_qr_image`; the picture is base64-encoded.
    /// Answered with [`Response::SetupCode`].
    ReadQrImage { image: String },
    /// See `MatterPeek::discover`. Answered with [`Response::Discovered`].
    Discover {
        discriminator: Option<u16>,
        #[serde(default)]
        setup_code: Option<String>,
        max: u8,
    },
    /// See `MatterPeek::list_devices`. Answered with [`Response::Devices`].
    ListDevices,
    /// See `MatterPeek::peek_device`. Answered with [`Response::Peek`].
    PeekDevice { node_id: u64 },
    /// See `MatterPeek::read_network`. Answered with [`Response::Network`].
    ReadNetwork { node_id: u64 },
    /// See `MatterPeek::change_network`. Answered with [`Response::Network`].
    ChangeNetwork {
        node_id: u64,
        endpoint: u16,
        change: NetworkChange,
    },
    /// See `MatterPeek::retrieve_logs`. Answered with [`Response::Logs`].
    RetrieveLogs { node_id: u64, intent: LogIntent },
    /// See `MatterPeek::identify`. Answered with [`Response::Ok`].
    Identify { node_id: u64, secs: u16 },
    /// Follow the health of a device on this connection (see
    /// `MatterPeek::subscribe`), as [`Event::HealthReport`]s and
    /// [`Event::SubscriptionState`]s, until [`Request::Unsubscribe`] or the
    /// connection closes. Answered at once with [`Response::Subscribed`].
    Subscribe { node_id: u64 },
    /// Stop following a device on this connection. Answered with
    /// [`Response::Ok`], also for a device not followed.
    Unsubscribe { node_id: u64 },
    /// See `MatterPeek::decommission`. Answered with [`Response::Devices`].
    Decommission { node_id: u64, force: bool },
    /// See `MatterPeek::sync`. Answered with [`Response::Devices`].
    Sync,
    /// See `MatterPeek::remember`. Answered with [`Response::Devices`].
    Remember { node_id: u64 },
    /// See `MatterPeek::set_tags`. Answered with [`Response::Devices`].
    SetTags { node_id: u64, tags: Vec<String> },
    /// See `MatterPeek::cancel_commission`. Answered at once with
    /// [`Response::Ok`]; the commission answers [`Response::Cancelled`].
    Cancel,
    /// See `MatterPeek::ota_images`. Answered with [`Response::OtaImages`].
    OtaImages,
    /// Start uploading an OTA image file of `size` bytes, in chunks on this
    /// connection (see `MatterPeek::ota_upload`). Answered with
    /// [`Response::OtaUploadStarted`].
    OtaUploadBegin { size: u64 },
    /// The next chunk of upload `upload_id`, at `offset`, base64-encoded; send
    /// the next once this one is answered with [`Response::Ok`].
    OtaUploadChunk {
        upload_id: u32,
        offset: u64,
        data: String,
    },
    /// Finish upload `upload_id`. Answered with [`Response::OtaImages`].
    OtaUploadEnd { upload_id: u32 },
    /// See `MatterPeek::ota_delete`. Answered with [`Response::OtaImages`].
    OtaDelete { name: String },
    /// See `MatterPeek::ota_update`. Answered with [`Response::Ok`] once
    /// announced; progress follows on this connection as [`Event::OtaProgress`].
    OtaUpdate { node_id: u64, name: String },
    /// The assistant's chats, the latest first. Answered with
    /// [`Response::Chats`].
    ChatList,
    /// A chat. Answered with [`Response::Chat`].
    ChatOpen { chat_id: String },
    /// Ask the assistant `text`, in chat `chat_id` or in a new one; with
    /// `allow_changes`, it is also offered the tools that change things. What
    /// it does arrives as [`Event::Chat`]s; the answer is [`Response::Chat`].
    ChatAsk {
        chat_id: Option<String>,
        text: String,
        allow_changes: bool,
    },
    /// Answer the assistant's request to run a tool ([`ChatEvent::Approve`]).
    /// Answered with [`Response::Ok`].
    ChatApprove {
        chat_id: String,
        call_id: String,
        answer: Approval,
    },
    /// Stop the assistant in chat `chat_id` before its next step. Answered
    /// with [`Response::Ok`].
    ChatStop { chat_id: String },
    /// Delete chat `chat_id`. Answered with [`Response::Chats`].
    ChatDelete { chat_id: String },
}

impl Request {
    /// Whether it is about the assistant's chats, which the server runs.
    pub fn is_chat(&self) -> bool {
        matches!(
            self,
            Self::ChatList
                | Self::ChatOpen { .. }
                | Self::ChatAsk { .. }
                | Self::ChatApprove { .. }
                | Self::ChatStop { .. }
                | Self::ChatDelete { .. }
        )
    }
}

/// A reply to a [`Request`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Response {
    Commissioned(CommissionResult),
    SetupCode {
        setup_code: String,
    },
    Discovered {
        devices: Vec<DiscoveredDevice>,
        #[serde(default)]
        ble_devices: Vec<BleDevice>,
    },
    Devices {
        devices: Vec<DeviceInfo>,
        #[serde(default)]
        notice: Option<String>,
    },
    Peek {
        node_id: u64,
        health: DeviceHealth,
    },
    Network {
        node_id: u64,
        network: Box<NetworkInfo>,
    },
    /// A log, base64-encoded in `data`; empty if the device has none of that
    /// kind.
    Logs {
        node_id: u64,
        intent: LogIntent,
        data: String,
    },
    Ok,
    /// See [`PeekError::Cancelled`].
    Cancelled {
        decommissioned: bool,
    },
    /// `already` is `true` if this connection followed the device already.
    Subscribed {
        node_id: u64,
        already: bool,
    },
    OtaImages {
        images: Vec<OtaImageInfo>,
    },
    OtaUploadStarted {
        upload_id: u32,
    },
    /// The assistant's chats; without a model (`None`), there is no
    /// assistant.
    Chats {
        model: Option<String>,
        chats: Vec<ChatSummary>,
    },
    Chat {
        chat: Box<ChatView>,
    },
    Error {
        message: String,
    },
}

/// An event pushed to the client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// A step of the commission requested by `req_id`.
    Step {
        req_id: ReqId,
        step: CommissionStep,
        status: StepStatus,
    },
    /// A health report of a followed device (see `HealthListener::on_report`).
    HealthReport {
        node_id: u64,
        seq: u64,
        health: DeviceHealth,
    },
    /// The subscription state of a followed device changed.
    SubscriptionState {
        node_id: u64,
        state: SubscriptionState,
    },
    /// How an update started on this connection goes.
    OtaProgress { node_id: u64, progress: OtaProgress },
    /// What the assistant does in chat `chat_id`, asked on this connection.
    Chat { chat_id: String, update: ChatEvent },
}

/// A chat with the assistant, as listed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatSummary {
    pub id: String,
    pub title: String,
    /// When it last changed, in seconds since the Unix epoch.
    pub updated: u64,
}

/// A chat with the assistant, as shown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatView {
    pub id: String,
    pub title: String,
    pub updated: u64,
    /// Whether the assistant is offered the tools that change things.
    pub allow_changes: bool,
    /// Whether the assistant is at work on a question.
    pub running: bool,
    /// The conversation, without the system prompt.
    pub messages: Vec<Message>,
    /// The tool the assistant asks to run, awaiting [`Request::ChatApprove`].
    pub pending: Option<ToolCall>,
}

/// What the assistant does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatEvent {
    /// The model is asked.
    Thinking,
    /// The model's text, Markdown.
    Text { text: String },
    /// A tool is to run (or be asked for).
    ToolCall { call: ToolCall },
    /// A tool ran: its result (the error, if it failed).
    ToolResult {
        call_id: String,
        result: String,
        is_error: bool,
    },
    /// The assistant asks to run `call`; answer with [`Request::ChatApprove`].
    Approve { call: ToolCall },
    /// The question ended without an answer (or was stopped).
    Error { message: String },
}

/// The answer to [`ChatEvent::Approve`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Approval {
    Yes,
    No,
    /// Yes, and to this tool for the rest of the chat.
    Always,
}
