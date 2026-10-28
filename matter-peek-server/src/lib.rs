//! The matter-peek server: a [`Hub`], a controller and its assistant for the
//! connections to them, and [`run`], which serves a hub's [`wire`] messages
//! to WebSocket clients (the web UI, the CLI and the MCP server with
//! `--server`) and the web UI's files ([`web`]), on one port. The executables
//! embed it: `matter-peek` (`serve`, and its own connections without a
//! server) and the app (its connections to the controller in it).
//!
//! The API asks for an access token, which the server makes at its first start
//! and keeps in its store's directory ([`access_token`]).
//!
//! The assistant's chats keep the model in the store or take the one the
//! environment sets ([`llm`]).
//!
//! It runs on a single-threaded [`LocalExecutor`] (smol/async-io family),
//! driven by [`block_on`]; the controller runs on its own thread.
//!
//! [`wire`]: matter_peek_core::api::wire

use std::future::Future;
use std::io;
use std::path::Path;
use std::rc::Rc;

use async_executor::LocalExecutor;

use matter_peek_core::api::agent::ModelConfig;
use matter_peek_core::api::platform::data_dir;
use matter_peek_core::api::PeekConfig;

pub use http::UNAUTHORIZED;
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

/// The file in the store's directory that holds the API's access token.
const ACCESS_TOKEN_FILE: &str = "access-token";

/// Run the server: start the controller, then serve the API and the web UI on
/// `addr`, with the assistant's model `env` sets, else the stored one. The
/// store, the chats and the access token are in `store_dir`, else in the
/// user's data directory.
pub fn run(addr: &str, env: Option<ModelConfig>, store_dir: Option<&Path>) -> io::Result<()> {
    let dir = store_dir.map_or_else(data_dir, Path::to_path_buf);
    let config = PeekConfig {
        store_dir: store_dir.map(|dir| dir.display().to_string()),
        periodic_sync: true,
    };
    // The store is locked first: one server per store, and per token.
    let hub = Hub::start(config, env, dir.join("chats"))?;
    let token = access_token(&dir, false)?;

    block_on(http::serve(addr, hub, token))
}

/// The API's access token of the server whose store is in `dir`: the one kept
/// there, else a new one, kept from now on. `renew` makes a new one anyway; a
/// running server takes it at its next start.
pub fn access_token(dir: &Path, renew: bool) -> io::Result<String> {
    if !renew {
        if let Some(token) = stored_access_token(dir)? {
            return Ok(token);
        }
    }

    let path = dir.join(ACCESS_TOKEN_FILE);
    let bytes: [u8; 16] = rand::random();
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();

    std::fs::create_dir_all(dir)?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    // For the server's user alone.
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    io::Write::write_all(&mut options.open(&path)?, token.as_bytes())?;

    Ok(token)
}

/// The API's access token kept in `dir`, if there is one.
pub fn stored_access_token(dir: &Path) -> io::Result<Option<String>> {
    match std::fs::read_to_string(dir.join(ACCESS_TOKEN_FILE)) {
        Ok(token) if !token.trim().is_empty() => Ok(Some(token.trim().to_string())),
        Ok(_) => Ok(None),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Handle to the thread-local executor.
fn executor() -> Rc<LocalExecutor<'static>> {
    EXECUTOR.with(Rc::clone)
}
