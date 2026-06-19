//! matter-peek-mcp — an MCP (Model Context Protocol) server over `matter-peek`.
//!
//! **`no_std` + `alloc`.** The protocol layer carries no `std` and no controller
//! dependency: it is the MCP envelope (JSON-RPC 2.0), the tool catalog, and the
//! mapping tool ⇄ [`matter_peek_proto`] request/response. It runs a tool call by
//! delegating to an injected [`Dispatch`] — so the same protocol layer serves a
//! desktop stdio shell now and a future esp on-device shell later, neither baked
//! in here.
//!
//! The desktop **stdio binary** (behind the `std` feature, in `main.rs`) supplies
//! a `Dispatch` over the existing `matter-peek-rpc` actor and drives the
//! newline-delimited JSON-RPC loop on stdin/stdout.

#![no_std]

extern crate alloc;

pub mod jsonrpc;
pub mod mcp;

pub use mcp::{handle, Caps, Dispatch};
