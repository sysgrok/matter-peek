//! matter-peek-core — a Matter controller, and its API.
//!
//! [`api::MatterPeek`] is the controller: it commissions devices onto its
//! fabric, lists them, reads and follows their health, retrieves their logs,
//! updates them and decommissions them. Its methods are the API for Rust, for
//! the mobile apps (UniFFI, `uniffi` feature) and, as messages, for the
//! WebSocket server (see [`api::wire`]). Everything else is private.
//!
//! **`no_std` + `alloc`.** Without `std`, the crate carries the API types, and
//! with `controller` the controller too. `std` runs the controller on this host:
//! its thread, UDP socket, store directory and OTA image files.
//!
//! The controller wraps [`rs_matter`]'s controller side: its `Commissioner`,
//! the IM client, the Diagnostic Logs / BDX receiver, the OTA Provider. See
//! `DESIGN.md` at the workspace root for the architecture.

#![no_std]
// The TLV derives of the stored device record nest deeper than the default.
#![recursion_limit = "256"]

extern crate alloc;

#[cfg(feature = "std")]
extern crate std;

#[cfg(feature = "controller")]
use controller::*;

pub mod api;

/// The controller behind the API; private.
#[cfg(feature = "controller")]
mod controller {
    pub mod basic_info;
    #[cfg(feature = "ble")]
    pub mod ble;
    pub mod ca;
    pub mod diagnostics;
    pub mod discovery;
    pub mod explain;
    pub mod fabrics;
    #[cfg(feature = "std")]
    pub mod host;
    pub mod logs;
    pub mod matter;
    pub mod mdns;
    pub mod mesh;
    pub mod model;
    pub mod netconfig;
    pub mod network;
    pub mod node;
    pub mod ota;
    pub mod persist;
    pub mod service;
    pub mod setup_code;
    pub mod subscribe;
    pub mod time;
    pub mod window;
}

#[cfg(feature = "uniffi")]
uniffi::setup_scaffolding!();
