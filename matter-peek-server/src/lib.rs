//! The matter-peek server: a [`Hub`], a controller and its assistant for the
//! connections to them, and [`run`], which serves a hub's [`wire`] messages
//! to WebSocket clients (the web UI, the CLI and the MCP server with
//! `--server`) and the web UI's files ([`web`]), on one port. The executables
//! embed it: `matter-peek` (`serve`, and its own connections without a
//! server) and the app (its connections to the controller in it).
//!
//! The assistant's chats keep the model in the store or take the one the
//! environment sets ([`llm`]).
//!
//! It runs on a single-threaded [`LocalExecutor`] (smol/async-io family),
//! driven by [`block_on`]; the controller runs on its own thread.
//!
//! [`wire`]: matter_peek_core::api::wire

use std::future::Future;
use std::rc::Rc;

use async_executor::LocalExecutor;

use matter_peek_core::api::agent::ModelConfig;
use matter_peek_core::api::platform::data_dir;
use matter_peek_core::api::PeekConfig;

pub use hub::{Hub, Session};
pub use web::web_file;

mod chats;
mod http;
mod hub;
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
    let config = PeekConfig {
        store_dir: None,
        periodic_sync: true,
    };
    let hub = Hub::start(config, env, data_dir().join("chats"))?;

    block_on(http::serve(addr, hub))
}

/// Handle to the thread-local executor.
fn executor() -> Rc<LocalExecutor<'static>> {
    EXECUTOR.with(Rc::clone)
}
