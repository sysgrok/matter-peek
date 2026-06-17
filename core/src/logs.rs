//! Fetching the Diagnostic Logs cluster (crash dumps / logs) over BDX.
//!
//! Phase 1 target. The flow mirrors rs-matter's `tests/diag_logs.rs`:
//!
//! 1. Stand up a [`Bdx`](rs_matter::bdx::Bdx) responder wrapping a
//!    [`DiagLogsBdxHandler`](rs_matter::dm::clusters::diag_logs::client::DiagLogsBdxHandler)
//!    over our [`DiagLogsReceiver`](rs_matter::dm::clusters::diag_logs::client::DiagLogsReceiver).
//! 2. Send a `RetrieveLogsRequest` with `RequestedProtocol = BDX` and the chosen
//!    [`crate::model::LogIntent`] (→ `rs_matter` `IntentEnum`) via the generated
//!    `DiagnosticLogsClient` proxy on the CASE exchange.
//! 3. The device opens a BDX *upload* back to us; the responder routes it to our
//!    receiver, which drains the [`BdxReader`](rs_matter::bdx::BdxReader) and
//!    emits progress events to the UI.
//!
//! The receiver is the seam for the (future) consent-gated cloud upload: instead
//! of buffering to memory it can stream straight to an HTTPS sink.

// Intentionally empty for Phase 0 — see module docs for the Phase 1 plan.
