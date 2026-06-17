//! Reading the standard Matter diagnostic clusters over a CASE session.
//!
//! Phase 1 target. Each reader uses the codegen per-cluster client trait
//! (`*Client` on an [`Exchange`](rs_matter::transport::exchange::Exchange)) to
//! read the relevant attributes, mapping them into [`crate::model`] DTOs:
//!
//! - General  → `rs_matter::dm::clusters::gen_diag`
//! - Software → `rs_matter::dm::clusters::sw_diag`  (Uptime, CurrentHeapFree, ...)
//! - Thread   → `rs_matter::dm::clusters::thread_diag`
//! - Wi-Fi    → `rs_matter::dm::clusters::wifi_diag`
//! - Ethernet → `rs_matter::dm::clusters::eth_diag`
//!
//! Not all clusters exist on every device; readers probe and skip gracefully so
//! the tool stays vendor-agnostic.

// Intentionally empty for Phase 0 — see module docs for the Phase 1 plan.
