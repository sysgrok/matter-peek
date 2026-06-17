//! matter-peek-core — a vendor-agnostic Matter diagnostic controller.
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

pub mod controller;
pub mod diagnostics;
pub mod logs;
pub mod model;

pub use controller::Controller;
pub use model::*;
