//! The matter-peek API.
//!
//! [`MatterPeek`] is the controller: start one on this host with
//! [`MatterPeek::start`] (`std`), and call its methods from any thread. The same
//! methods are the native Rust API, the UniFFI one (`uniffi` feature, for the
//! mobile apps), and, as [`wire`] messages, the WebSocket one.
//!
//! The types are always available, also without `controller` (e.g. for the web
//! UI, which only speaks the [`wire`] messages).

pub use types::*;

#[cfg(feature = "controller")]
pub use peek::{
    CommissionListener, HealthListener, MatterPeek, OtaListener, OtaUpload, Subscription,
};

#[cfg(feature = "controller")]
pub(crate) use peek::{Op, Reply, Service};

pub mod wire;

#[cfg(feature = "controller")]
mod peek;
mod types;
