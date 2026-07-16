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
//!
//! ## Status: Phase 0 (vertical spike)
//!
//! Only the `controller::commission` primitive is wired end-to-end so far,
//! against an externally-running Matter accessory. `diagnostics` and `logs`
//! carry the intended signatures over the real `rs-matter` APIs and are filled
//! in next.

#![no_std]

extern crate alloc;

pub mod basic_info;
#[cfg(feature = "ble")]
pub mod ble;
pub mod ca;
pub mod controller;
pub mod diagnostics;
pub mod discovery;
pub mod logs;
pub mod model;
pub mod persist;
pub mod subscribe;
pub mod vendors;

pub use controller::Controller;
pub use discovery::DiscoveredDevice;
pub use model::*;
pub use persist::ControllerStore;
