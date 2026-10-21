//! matter-peek-mcp binary — the MCP stdio server.
//!
//! ```text
//! matter-peek-mcp [--server ws://127.0.0.1:8087] [--allow-mutate]
//! ```
//!
//! Speaks newline-delimited JSON-RPC 2.0 (the MCP stdio transport) on
//! stdin/stdout. Each `tools/call` becomes a request to the matter-peek server
//! (`matter-peek-server`), the same API the web UI and the CLI use. Logs go to
//! **stderr** so they never corrupt the stdout JSON stream.
//!
//! Read-only tools are always available; the mutating ones require
//! `--allow-mutate`.

use core::cell::RefCell;

use std::process::ExitCode;
use std::rc::Rc;

use futures_lite::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use futures_lite::StreamExt;

use log::{debug, error, info, warn};

use matter_peek_core::api::wire;
use matter_peek_mcp::{handle, Caps, Dispatch};
use matter_peek_server::client::{self, Connection};

/// Where the matter-peek server listens by default.
const DEFAULT_SERVER: &str = "ws://127.0.0.1:8087";

/// A [`Dispatch`] over a connection to the matter-peek server, opened on the
/// first call and reopened once if it was closed (e.g. the server restarted).
/// One connection serves the whole session: an upload in progress belongs to it.
struct ServerDispatch {
    url: String,
    conn: RefCell<Option<Rc<Connection>>>,
}

impl ServerDispatch {
    async fn connection(&self) -> Result<Rc<Connection>, String> {
        if let Some(conn) = self.conn.borrow().clone() {
            return Ok(conn);
        }

        let conn = Rc::new(
            client::connect(&self.url)
                .await
                .map_err(|e| e.to_string())?,
        );
        *self.conn.borrow_mut() = Some(conn.clone());

        Ok(conn)
    }
}

impl Dispatch for ServerDispatch {
    async fn dispatch(&self, request: wire::Request) -> Result<wire::Response, String> {
        // Events are not surfaced: a tool call is request/response.
        match self.connection().await?.call(request.clone(), |_| ()).await {
            Ok(response) => Ok(response),
            Err(e) => {
                warn!("matter-peek-mcp: {e}; reconnecting");
                self.conn.borrow_mut().take();

                self.connection()
                    .await?
                    .call(request, |_| ())
                    .await
                    .map_err(|e| e.to_string())
            }
        }
    }

    async fn read_file(&self, path: &str) -> Result<Vec<u8>, String> {
        let path = path.to_string();

        blocking::unblock(move || {
            std::fs::read(&path).map_err(|e| format!("cannot read {path}: {e}"))
        })
        .await
    }
}

fn main() -> ExitCode {
    // Logs to STDERR — stdout is the JSON-RPC channel and must stay clean.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .target(env_logger::Target::Stderr)
        .init();

    let mut allow_mutate = false;
    let mut url = DEFAULT_SERVER.to_string();

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--allow-mutate" => allow_mutate = true,
            "--server" => match args.next() {
                Some(server) => url = server,
                None => {
                    error!("matter-peek-mcp: --server needs a URL");
                    return ExitCode::FAILURE;
                }
            },
            other => {
                error!("matter-peek-mcp: unknown argument {other}");
                return ExitCode::FAILURE;
            }
        }
    }

    info!(
        "matter-peek-mcp: starting, server {url} (mutating tools {})",
        if allow_mutate { "ENABLED" } else { "disabled" }
    );

    let caps = Caps { allow_mutate };
    let dispatch = ServerDispatch {
        url,
        conn: RefCell::new(None),
    };

    match matter_peek_server::block_on(stdio_loop(dispatch, caps)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("matter-peek-mcp: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The newline-delimited JSON-RPC loop: read a line from stdin, parse, handle,
/// write the response line to stdout. Ends cleanly at EOF (client closed stdin).
///
/// stdin/stdout are async-wrapped with `blocking::Unblock` (a thread pool) — the
/// smol-family way to do async stdio; a direct `async_io::Async` over a pipe/tty
/// fd can fail to register with epoll.
async fn stdio_loop<D: Dispatch>(dispatch: D, caps: Caps) -> std::io::Result<()> {
    let stdin = BufReader::new(blocking::Unblock::new(std::io::stdin()));
    let mut stdout = blocking::Unblock::new(std::io::stdout());
    let mut lines = stdin.lines();

    while let Some(line) = lines.next().await {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        let req: matter_peek_mcp::jsonrpc::Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                debug!("mcp: bad JSON-RPC line: {e}");
                continue; // malformed lines are ignored (can't reply without an id)
            }
        };

        if let Some(resp) = handle(req, &dispatch, caps).await {
            let mut json = serde_json::to_string(&resp).unwrap_or_default();
            json.push('\n');
            stdout.write_all(json.as_bytes()).await?;
            stdout.flush().await?;
        }
    }

    info!("matter-peek-mcp: stdin closed, exiting");
    Ok(())
}
