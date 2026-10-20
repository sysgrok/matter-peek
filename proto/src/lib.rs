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
        /// The device's passcode, manual pairing code or QR code payload.
        setup_code: String,
        policy: FabricPolicy,
    },
    /// Browse the local network for commissionable devices via mDNS, returning a
    /// pick-list. Optional `discriminator` narrows to a single device (useful
    /// when several commissionable devices are present).
    Discover {
        discriminator: Option<u16>,
        /// Narrows to the device of this setup code, unless `discriminator` is
        /// given. A bare passcode does not narrow.
        #[serde(default)]
        setup_code: Option<String>,
        /// Stop after this many matches (the UI caps the list).
        max: u8,
    },
    /// List the devices the controller has persistently commissioned (from the
    /// local store). The basis for the persistent-fabric device list UI.
    ListDevices,
    /// Re-address an already-commissioned (persisted) device by its NodeID over a
    /// fresh CASE session — no re-commissioning — and read its health (+ optional
    /// `CrashLogs`). Returns [`Response::Peek`].
    PeekDevice { node_id: u64, with_logs: bool },
    /// Ask a persisted device to physically identify itself (blink/beep) for
    /// `secs` seconds via the Identify cluster — the "which unit is this?" UX.
    Identify { node_id: u64, secs: u16 },
    /// Start a **live health subscription** to a persisted device. Answered
    /// **immediately** with [`Response::Subscribed`]; the health data then streams
    /// independently as [`Event::HealthReport`] frames (keyed by `node_id`), and
    /// connection health as [`Event::SubscriptionState`], until a matching
    /// [`Request::Unsubscribe`] (or the connection drops).
    ///
    /// The subscription is a detached background task on the server — it does
    /// **not** block other commands, so multiple devices can be watched at once.
    /// It self-heals: if the device goes silent past its reporting deadline the
    /// server re-establishes it (a fresh subscription), surfaced via
    /// [`SubscriptionState::Reconnecting`] → [`SubscriptionState::Live`].
    /// Subscribing a device that is already subscribed is a no-op success.
    Subscribe { node_id: u64 },
    /// Stop a live health subscription started by [`Request::Subscribe`].
    /// Idempotent — unsubscribing a device that isn't subscribed still succeeds.
    /// Answered with [`Response::Ok`].
    Unsubscribe { node_id: u64 },
    /// **Decommission** a persisted device: `RemoveFabric` our fabric on the
    /// device (over CASE) and drop it from the local store. Answered with the
    /// refreshed [`Response::Devices`] list.
    ///
    /// The device is contacted over CASE, retried a few times. If it stays
    /// unreachable, its record is kept as pending removal, which a
    /// [`Request::Sync`] completes once the device advertises again. `force`
    /// drops the record without contacting the device; a device that still holds
    /// our fabric then shows up again on the next sync.
    Decommission { node_id: u64, force: bool },
    /// Re-sync the device list with the devices on our fabric: adopt the ones we
    /// have no record of, refresh the others' identity, complete pending
    /// removals, and note which are online. Also runs periodically. Answered with
    /// the refreshed [`Response::Devices`] list.
    Sync,
    /// Replace the local tag set of a stored device (controller-local metadata
    /// for filtering/grouping; never touches the device). The server normalizes
    /// (trim / drop empties / de-dup) and answers with the refreshed
    /// [`Response::Devices`] list.
    SetTags { node_id: u64, tags: Vec<String> },
    /// Request cancellation of the in-flight vertical (commission flow). Handled
    /// out-of-band (it does not queue behind the running command): the server
    /// sets a cancel flag the vertical polls at phase boundaries. Cancellation
    /// only takes effect at a *safe* point — before the device fabric is
    /// installed it aborts cleanly; once installed, the device is decommissioned
    /// (RemoveFabric) so nothing is left orphaned. Answered immediately with
    /// [`Response::Ok`]; the cancelled vertical itself replies
    /// [`Response::Cancelled`].
    Cancel,
    /// List the stored OTA images. Answered with [`Response::OtaImages`].
    OtaImages,
    /// Start uploading an OTA image file (`.ota`) of `size` bytes, in chunks on
    /// this connection. Answered with [`Response::OtaUploadStarted`].
    OtaUploadBegin { size: u64 },
    /// The next chunk of upload `upload_id`, at `offset`, base64-encoded; send
    /// the next once this one is answered with [`Response::Ok`]. The file is
    /// checked as it arrives: a bad one fails the upload.
    OtaUploadChunk {
        upload_id: u32,
        offset: u64,
        data: String,
    },
    /// Finish upload `upload_id`: check the file's payload digest and store it.
    /// Answered with [`Response::OtaImages`].
    OtaUploadEnd { upload_id: u32 },
    /// Delete stored OTA image `name`. Answered with [`Response::OtaImages`].
    OtaDelete { name: String },
    /// Update a stored device to OTA image `name`: allow it that image, and
    /// announce us to it as its OTA Provider. Answered with [`Response::Ok`] once
    /// announced; progress follows as [`Event::OtaProgress`] frames.
    OtaUpdate { node_id: u64, name: String },
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
    /// The controller's persistently-commissioned devices (from the store).
    ///
    /// `notice` carries a non-fatal remark about the operation that produced the
    /// list — e.g. that a decommission dropped the record locally without being
    /// able to reach the device. `None` for a plain listing.
    Devices {
        devices: Vec<StoredDeviceInfo>,
        #[serde(default)]
        notice: Option<String>,
    },
    /// Result of a [`Request::PeekDevice`].
    Peek {
        handle: DeviceHandle,
        health: DeviceHealth,
        crash_log_bytes: Option<usize>,
    },
    /// A [`Request::Identify`] / other fire-and-forget op succeeded.
    Ok,
    /// The in-flight vertical was cancelled at a safepoint. `decommissioned` is
    /// `true` if the device had already been given our fabric and was therefore
    /// cleaned up (RemoveFabric) on the way out; `false` if cancellation landed
    /// before any device state was created.
    Cancelled { decommissioned: bool },
    /// A [`Request::Subscribe`] was accepted: the live subscription is now
    /// running as a background task and its data will arrive as
    /// [`Event::HealthReport`] / [`Event::SubscriptionState`] frames. `already`
    /// is `true` if the device was already subscribed (the request was a no-op).
    Subscribed { node_id: u64, already: bool },
    /// The stored OTA images.
    OtaImages { images: Vec<OtaImageInfo> },
    /// An upload started by [`Request::OtaUploadBegin`]; send its chunks.
    OtaUploadStarted { upload_id: u32 },
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
    /// A live health snapshot from an active subscription, keyed by `node_id`
    /// (subscriptions are detached from the request that started them, so events
    /// carry the device id, not a `req_id`). The first frame after each
    /// (re-)establishment is a priming snapshot; later frames are deltas (only the
    /// attributes that changed) — the UI merges them into a running per-device
    /// view.
    HealthReport {
        node_id: u64,
        /// Monotonic update counter within the subscription's lifetime.
        seq: u64,
        health: DeviceHealth,
    },
    /// A live subscription's connection state changed (established / lost /
    /// recovering). The UI shows this per device so a silent-but-dead
    /// subscription is distinguishable from a healthy-but-quiet one.
    SubscriptionState {
        node_id: u64,
        state: SubscriptionState,
    },
    /// How an update started by [`Request::OtaUpdate`] goes.
    OtaProgress { node_id: u64, progress: OtaProgress },
}

/// The stages of an OTA update, as the provider sees them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "stage", rename_all = "snake_case")]
pub enum OtaProgress {
    /// The device asked for an update and was offered `name`.
    Offered { name: String },
    /// The device asked for an update and was told there is none for it.
    NotOffered,
    /// The device downloaded `percent` of its image.
    Downloading { percent: u8 },
    /// The device was told to apply `version`.
    Applying { version: u32 },
    /// The device reports that it runs `version`.
    Applied { version: u32 },
}

/// A stored OTA image, as its header describes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OtaImageInfo {
    pub name: String,
    pub vendor_id: u16,
    pub product_id: u16,
    pub version: u32,
    pub version_str: String,
    pub min_applicable_version: Option<u32>,
    pub max_applicable_version: Option<u32>,
    pub release_notes_url: Option<String>,
    pub size: u64,
}

/// The connection state of a live health subscription (mirrors core's `SubState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SubscriptionState {
    /// Established and reporting; the device will report at least every
    /// `max_int_secs` seconds.
    Live { max_int_secs: u16 },
    /// The subscription went silent past its deadline (or failed to establish)
    /// and is being rebuilt; the next `Live` marks recovery.
    Reconnecting,
    /// The subscription was stopped (by [`Request::Unsubscribe`] or the
    /// connection dropping). No further reports will arrive.
    Stopped,
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
    /// Our fabric, in the controller's fabric table.
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

/// A device the controller has persistently commissioned (from the local store).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredDeviceInfo {
    pub handle: DeviceHandle,
    /// BasicInformation `UniqueID` (stable across resets).
    pub unique_id: String,
    pub serial_number: String,
    /// User-set name (from the primary ecosystem, if it wrote `NodeLabel`).
    pub node_label: String,
    /// BasicInformation `VendorName` — the vendor's own human-readable name
    /// (empty if the device didn't serve it).
    pub vendor_name: String,
    /// BasicInformation `ProductName` — the product's own human-readable name
    /// (empty if the device didn't serve it).
    pub product_name: String,
    /// A ready-to-show vendor label resolved server-side: the device's
    /// `VendorName` when present, else an offline VID→name lookup, else the raw
    /// hex `vid:0x…`. The UI shows this verbatim (it needs no vendor table).
    pub vendor_display: String,
    /// `true` when the vendor id is in the reserved **test** range
    /// (`0xFFF1`–`0xFFF4`) — a development / uncertified device, worth flagging.
    pub is_test_vendor: bool,
    /// Per-model numeric fingerprint (vendor/product/hw/sw), for display + the
    /// re-match link suggestion.
    pub fingerprint: Fingerprint,
    /// Local user tags for filtering/grouping.
    pub tags: Vec<String>,
    /// Whether the device advertised on our fabric at the last sync; `None`
    /// before the first one.
    #[serde(default)]
    pub online: Option<bool>,
    /// Our fabric is to be removed from it once it is reachable again.
    #[serde(default)]
    pub pending_removal: bool,
}

/// The numeric BasicInformation fingerprint, mirrored for the wire.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    pub vendor_id: Option<u16>,
    pub product_id: Option<u16>,
    pub hardware_version: Option<u16>,
    pub software_version: Option<u32>,
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
    /// Set when the just-commissioned device's *per-model fingerprint* matched an
    /// existing stored device (but no per-unit id did): the `node_id` of that
    /// existing device, offered to the UI as a "link these?" suggestion. `None`
    /// when the device was an exact re-match (auto-merged) or genuinely new.
    pub rematch_candidate: Option<u64>,
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
