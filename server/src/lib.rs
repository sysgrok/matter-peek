//! matter-peek-server — the matter-peek API over WebSocket/JSON.
//!
//! The server ([`run`]) starts a [`MatterPeek`] and serves its [`wire`]
//! messages to WebSocket clients: the web UI, the CLI and the MCP server with
//! `--server`. Those clients reach it, or a controller in their own process,
//! through a [`client::Connection`].
//!
//! The shells run on a single-threaded [`LocalExecutor`] (smol/async-io
//! family); the controller runs on its own thread.
//!
//! [`wire`]: matter_peek_core::api::wire

use std::future::Future;
use std::rc::Rc;

use async_executor::LocalExecutor;

use matter_peek_core::api::{MatterPeek, PeekConfig};

pub mod client;
mod server;

thread_local! {
    /// The thread-local executor, shared by clone. Tasks spawned via [`spawn`]
    /// run here; [`block_on`] drives it.
    static EXECUTOR: Rc<LocalExecutor<'static>> = Rc::new(LocalExecutor::new());
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
pub fn block_on<F, T>(fut: F) -> T
where
    F: Future<Output = T>,
{
    let ex = executor();
    futures_lite::future::block_on(ex.run(fut))
}

/// Run the server: start the controller, then serve WebSocket connections on
/// `addr`.
pub fn run(addr: &str) -> std::io::Result<()> {
    let peek = MatterPeek::start(PeekConfig {
        store_dir: None,
        periodic_sync: true,
    })
    .map_err(std::io::Error::other)?;

    block_on(server::serve(addr, peek))
}

/// Handle to the thread-local executor.
fn executor() -> Rc<LocalExecutor<'static>> {
    EXECUTOR.with(Rc::clone)
}
