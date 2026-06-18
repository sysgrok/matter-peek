//! matter-peek wire contract.
//!
//! The JSON request/response/event vocabulary spoken between the native RPC
//! server (`matter-peek-rpc`) and the UIs (the Yew/Wasm web client now; the
//! mobile UniFFI surface will mirror these shapes later). This crate is the
//! single source of truth for the boundary — it deliberately depends on nothing
//! but `serde`, so it compiles to native, `wasm32`, **and embedded** (it is
//! `no_std` + `alloc`, so a future esp32 on-device adapter shares it).
//!
//! Envelope: the client sends [`Request`] frames tagged with a `req_id`; the
//! server answers with [`Response`] frames carrying the same `req_id`, and emits
//! unsolicited [`Event`] frames (progress, async results) in between. Every frame
//! on the socket is one [`ServerMsg`] or [`ClientMsg`].

#![no_std]

extern crate alloc;
use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

/// A correlation id chosen by the client and echoed on the matching response.
pub type ReqId = u64;

// ---------------------------------------------------------------------------
// Envelopes
// ---------------------------------------------------------------------------

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
    /// Unsolicited event (progress / async notification), not tied to a request.
    Event { event: Event },
}

// ---------------------------------------------------------------------------
// Requests / responses
// ---------------------------------------------------------------------------

/// A command from the UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Run the full Phase 0 vertical against a manually-addressed device:
    /// commission → read health → retrieve `CrashLogs` → decommission.
    /// Progress arrives as [`Event`]s; the final [`Response::Vertical`] carries
    /// the assembled result.
    RunVertical {
        /// e.g. `"[::1]:5540"`.
        peer_addr: String,
        passcode: u32,
        policy: FabricPolicy,
    },
    /// Browse the local network for commissionable devices via mDNS, returning a
    /// pick-list. Optional `discriminator` narrows to a single device (useful
    /// when several commissionable devices are present).
    Discover {
        discriminator: Option<u16>,
        /// Stop after this many matches (the UI caps the list).
        max: u8,
    },
}

/// A reply to a [`Request`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Response {
    /// The vertical completed (possibly with per-step soft failures recorded in
    /// the payload — e.g. logs unavailable on a device that has none).
    Vertical(VerticalResult),
    /// Devices found by a [`Request::Discover`] (possibly empty).
    Discovered { devices: Vec<DiscoveredDevice> },
    /// The request failed before producing a result.
    Error { message: String },
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// Server-pushed progress / async notifications.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// A step of the vertical started / finished.
    Step {
        req_id: ReqId,
        step: VerticalStep,
        status: StepStatus,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerticalStep {
    Commission,
    ReadHealth,
    RetrieveLogs,
    Decommission,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum StepStatus {
    Started,
    Done,
    Failed { message: String },
}

// ---------------------------------------------------------------------------
// Domain payloads (mirror matter-peek-core's model, with serde)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FabricPolicy {
    Persistent,
    Ephemeral,
}

/// Identifies a commissioned device on our fabric.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceHandle {
    pub fabric_index: u8,
    pub node_id: u64,
}

/// A commissionable device found on the network (pre-commissioning).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredDevice {
    /// Operational address as a string (e.g. `"[fdb5:…]:5540"`), ready to feed
    /// back into [`Request::RunVertical`].
    pub addr: String,
    /// The commissionable instance id from the advertisement (stable per device
    /// while its commissioning window is open).
    pub commissionable_id: u64,
}

/// The full result of a vertical run, as shown in the UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerticalResult {
    pub handle: DeviceHandle,
    pub health: DeviceHealth,
    /// Size in bytes of the retrieved `CrashLogs`, if retrieval succeeded.
    pub crash_log_bytes: Option<usize>,
    /// Whether the fabric was removed afterward (ephemeral policy).
    pub decommissioned: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceHealth {
    pub general: Option<GeneralDiag>,
    pub software: Option<SoftwareDiag>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GeneralDiag {
    pub up_time_secs: Option<u64>,
    pub reboot_count: Option<u16>,
    pub total_operational_hours: Option<u32>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SoftwareDiag {
    pub current_heap_free: Option<u64>,
    pub current_heap_used: Option<u64>,
    pub current_heap_high_watermark: Option<u64>,
}
