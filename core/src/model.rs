//! UI-facing data types — the contract shared by every transport.
//!
//! These are deliberately decoupled from `rs-matter`'s internal types: the RPC
//! server and the (future) UniFFI layer expose *these*, never `rs-matter`
//! types directly. (serde derives are added once the `rpc` crate lands.)

use core::num::NonZeroU8;

/// How long the controller's fabric (NOC) lives on the device.
///
/// Both models share the same commission / read / fetch primitives; only the
/// before/after lifecycle differs. See `DESIGN.md` §5.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FabricPolicy {
    /// Commission once and keep the NOC on the device. Repeat access is a cheap
    /// CASE session. Requires local identity persistence + re-sync handling.
    Persistent,
    /// Commission on a throwaway fabric, do the work, then `RemoveFabric`
    /// (self-destruct). Nothing persisted locally.
    Ephemeral,
}

/// Which diagnostic clusters to read in a health snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiagSet {
    pub general: bool,
    pub software: bool,
    pub thread: bool,
    pub wifi: bool,
    pub ethernet: bool,
}

impl DiagSet {
    /// Read everything the device happens to support.
    pub const ALL: Self = Self {
        general: true,
        software: true,
        thread: true,
        wifi: true,
        ethernet: true,
    };
}

/// The `RetrieveLogsRequest` intent (maps to `rs_matter` `IntentEnum`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogIntent {
    EndUserSupport,
    NetworkDiag,
    CrashLogs,
}

/// A device the controller has commissioned and can address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceHandle {
    /// Our fabric slot on the device.
    pub fabric_index: NonZeroU8,
    /// NodeID we assigned to the device on our fabric.
    pub node_id: u64,
}

/// Result of a successful commissioning, returned to the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommissionOutcome {
    pub handle: DeviceHandle,
    pub policy: FabricPolicy,
}
