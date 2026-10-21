//! matter-peek-core — a Matter diagnostic controller.
//!
//! **`no_std` + `alloc`.** The core carries no `std` dependency so it can run on
//! embedded targets (e.g. esp32 with PSRAM) as well as desktop/mobile; only the
//! optional `mdns-builtin` backend pulls in `std` (raw sockets) and is
//! feature-gated. Socket creation and the concrete KV backend are injected by
//! the binary, never created in `core`.
//!
//! This crate wraps [`rs_matter`]'s controller-side primitives
//! ([`Commissioner`](rs_matter::onboard::Commissioner), the IM client, and the
//! Diagnostic Logs / BDX receiver) behind a small, UI-facing API. The same API
//! is projected onto two transports:
//!
//! - a local WebSocket/JSON RPC server (desktop / web UI), and
//! - UniFFI bindings (mobile — later).
//!
//! See `DESIGN.md` at the workspace root for the full architecture.

#![no_std]

extern crate alloc;

// The `std` feature (which every hosted mDNS backend turns on) is what lets the
// backends that need real sockets — `mdns-builtin` — name `std::` paths. The
// crate body stays `no_std`; this only makes the facade reachable when a hosted
// build has already opted in.
#[cfg(feature = "std")]
extern crate std;

pub mod basic_info;
#[cfg(feature = "ble")]
pub mod ble;
pub mod ca;
pub mod controller;
pub mod diagnostics;
pub mod discovery;
pub mod logs;
pub mod model;
pub mod node;
pub mod ota;
pub mod persist;
pub mod setup_code;
pub mod subscribe;
pub mod vendors;

pub use controller::Controller;
pub use discovery::DiscoveredDevice;
pub use model::*;
pub use persist::ControllerStore;
