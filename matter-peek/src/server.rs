//! The server: the matter-peek API over WebSocket/JSON, and the web UI.
//!
//! [`run`] starts a [`MatterPeek`] and serves, on one port, its [`wire`]
//! messages to WebSocket clients (the web UI, the CLI and the MCP server with
//! `--server`) and the web UI's files ([`web`]). Those clients reach it, or a
//! controller in their own process, through a [`client::Connection`]. It also
//! runs the assistant: its chats, with the model kept in the store or set by
//! the environment ([`llm`]).
//!
//! The shells run on a single-threaded [`LocalExecutor`] (smol/async-io
//! family); the controller runs on its own thread.
//!
//! [`wire`]: matter_peek_core::api::wire

use std::future::Future;
use std::rc::Rc;

use async_executor::LocalExecutor;

use matter_peek_core::api::agent::ModelConfig;
use matter_peek_core::api::platform::data_dir;
use matter_peek_core::api::{MatterPeek, PeekConfig};

mod chats;
pub mod client;
mod http;
pub mod llm;
mod web;

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

/// Run the server: start the controller, then serve the API and the web UI on
/// `addr`, with the assistant's model `env` sets, else the stored one.
pub fn run(addr: &str, env: Option<ModelConfig>) -> std::io::Result<()> {
    let peek = MatterPeek::start(PeekConfig {
        store_dir: None,
        periodic_sync: true,
    })
    .map_err(std::io::Error::other)?;

    let chats = chats::Chats::new(peek.clone(), env, data_dir().join("chats"));

    block_on(http::serve(addr, peek, chats))
}

/// Handle to the thread-local executor.
fn executor() -> Rc<LocalExecutor<'static>> {
    EXECUTOR.with(Rc::clone)
}
