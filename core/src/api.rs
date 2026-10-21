//! The matter-peek API.
//!
//! [`MatterPeek`] is the controller: start one on this host with
//! [`MatterPeek::start`] (`std`), or run one on what a host provides with
//! [`MatterPeek::new`] (see [`platform`]); call its methods from any thread.
//! The same methods are the native Rust API, the UniFFI one (`uniffi` feature,
//! for the mobile apps), as [`wire`] messages the WebSocket one, and as [`mcp`]
//! tools the one for AI agents.
//!
//! The types, the messages and the tools are always available, also without
//! `controller` (e.g. for the web UI, which only speaks the [`wire`] messages).

pub use types::*;

#[cfg(feature = "controller")]
pub use peek::{
    CommissionListener, HealthListener, MatterPeek, OtaListener, OtaUpload, Subscription,
};

#[cfg(feature = "controller")]
pub(crate) use peek::{Op, Reply, Service};

pub mod mcp;
#[cfg(feature = "controller")]
pub mod platform;
pub mod wire;

#[cfg(feature = "controller")]
mod peek;
mod types;
