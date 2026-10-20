//! matter-peek-rpc — a local WebSocket/JSON RPC server over `matter-peek-core`.
//!
//! Architecture (see `DESIGN.md` §3):
//! - an **actor** task owns the Matter stack and services one command at a time
//!   ([`actor`]);
//! - a **WS server** accepts browser/UI connections and relays
//!   [`matter_peek_proto`] frames to/from the actor ([`server`]).
//!
//! Everything runs on a single-threaded [`async_executor::LocalExecutor`]
//! (smol/async-io family). Single-threaded is deliberate: the Matter stack
//! (`rs_matter::Matter`, with its interior `RefCell`/`Mutex`) is not `Send`, and
//! a diagnostic controller drives one radio at a time anyway.

use std::future::Future;
use std::rc::Rc;

use async_executor::LocalExecutor;

pub mod actor;
pub mod convert;
pub mod ota;
pub mod server;

thread_local! {
    /// The thread-local executor, shared by clone. Tasks spawned via [`spawn`]
    /// run here; `run` drives it. An `Rc` (not a held `RefCell` borrow) avoids
    /// aliasing the executor across await points.
    static EXECUTOR: Rc<LocalExecutor<'static>> = Rc::new(LocalExecutor::new());
}

/// Handle to the thread-local executor.
fn executor() -> Rc<LocalExecutor<'static>> {
    EXECUTOR.with(Rc::clone)
}

/// Spawn a (not-necessarily-`Send`) task onto the thread-local executor and
/// detach it.
pub fn spawn<F>(fut: F)
where
    F: Future<Output = ()> + 'static,
{
    executor().spawn(fut).detach();
}

/// Drive the thread-local executor until `fut` completes, returning its output.
///
/// The companion to [`spawn`]: spawn the actor (and any background tasks), then
/// `block_on` the main loop. Used by alternative shells (e.g. the MCP stdio
/// server) that reuse the actor but supply their own top-level future instead of
/// the WS server.
pub fn block_on<F, T>(fut: F) -> T
where
    F: Future<Output = T>,
{
    let ex = executor();
    futures_lite::future::block_on(ex.run(fut))
}

/// Run the RPC server: start the actor, then serve WS connections on `addr`,
/// driving the thread-local executor to completion.
pub fn run(addr: &str) -> std::io::Result<()> {
    let (cmd_tx, cmd_rx) = async_channel::unbounded::<actor::Command>();

    // The vertical cancel flag, shared (single-threaded `Rc<Cell>`) between the
    // actor (polls it) and the server (sets it on a `Cancel` request, bypassing
    // the serial command queue).
    let cancel: actor::CancelFlag = std::rc::Rc::new(std::cell::Cell::new(false));

    spawn(actor::run(cmd_rx, cancel.clone()));

    let ex = executor();
    futures_lite::future::block_on(ex.run(server::serve(addr, cmd_tx, cancel)))
}
