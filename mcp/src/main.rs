//! matter-peek-mcp binary — the MCP stdio server.
//!
//! ```text
//! matter-peek-mcp [--server ws://127.0.0.1:8087] [--allow-mutate]
//! ```
//!
//! Speaks newline-delimited JSON-RPC 2.0 (the MCP stdio transport) on
//! stdin/stdout. Each `tools/call` becomes a request of the matter-peek API: to a
//! running `matter-peek-server` with `--server`, else to a controller started in
//! this process (which needs the store to itself, so not while a server runs).
//! Logs go to **stderr** so they never corrupt the stdout JSON stream.
//!
//! Read-only tools are always available; the mutating ones require
//! `--allow-mutate`.

use core::cell::RefCell;

use std::process::ExitCode;
use std::rc::Rc;

use clap::Parser;
use futures_lite::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use futures_lite::StreamExt;

use log::{debug, error, info, warn};

use matter_peek_core::api::{wire, PeekConfig};
use matter_peek_mcp::{handle, Caps, Dispatch};
use matter_peek_server::client::{self, Connection};

#[derive(Parser)]
#[command(
    name = "matter-peek-mcp",
    version,
    about = "The matter-peek API as MCP tools, over stdio"
)]
struct Cli {
    /// Use the matter-peek server at this URL (e.g. ws://127.0.0.1:8087),
    /// instead of a controller in this process.
    #[arg(long, value_name = "URL")]
    server: Option<String>,
    /// Also offer the tools that change devices or the stored state.
    #[arg(long)]
    allow_mutate: bool,
}

/// A [`Dispatch`] over a connection to the API: to a controller in this
/// process, or to the server at `url`, connected on the first call and
/// reconnected once if it was closed (e.g. the server restarted). One connection
/// serves the whole session: an upload in progress belongs to it.
struct ApiDispatch {
    url: Option<String>,
    conn: RefCell<Option<Rc<Connection>>>,
}

impl ApiDispatch {
    async fn connection(&self) -> Result<Rc<Connection>, String> {
        if let Some(conn) = self.conn.borrow().clone() {
            return Ok(conn);
        }

        let Some(url) = &self.url else {
            return Err("the controller stopped".into());
        };

        let conn = Rc::new(client::connect(url).await.map_err(|e| e.to_string())?);
        *self.conn.borrow_mut() = Some(conn.clone());

        Ok(conn)
    }
}

impl Dispatch for ApiDispatch {
    async fn dispatch(&self, request: wire::Request) -> Result<wire::Response, String> {
        // Events are not surfaced: a tool call is request/response.
        match self.connection().await?.call(request.clone(), |_| ()).await {
            Ok(response) => Ok(response),
            Err(e) if self.url.is_some() => {
                warn!("matter-peek-mcp: {e}; reconnecting");
                self.conn.borrow_mut().take();

                self.connection()
                    .await?
                    .call(request, |_| ())
                    .await
                    .map_err(|e| e.to_string())
            }
            Err(e) => Err(e.to_string()),
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

    let cli = Cli::parse();

    info!(
        "matter-peek-mcp: starting, {} (mutating tools {})",
        match &cli.server {
            Some(url) => format!("server {url}"),
            None => "controller in this process".into(),
        },
        if cli.allow_mutate {
            "ENABLED"
        } else {
            "disabled"
        }
    );

    // Started at once, so that a store held by another process fails here, and
    // the device list is synced before the first tool call.
    let conn = match &cli.server {
        Some(_) => None,
        // Long-lived, like a server: keep the device list in sync.
        None => match client::connect_in_process(PeekConfig {
            store_dir: None,
            periodic_sync: true,
        }) {
            Ok(conn) => Some(Rc::new(conn)),
            Err(e) => {
                error!("matter-peek-mcp: {e}");
                return ExitCode::FAILURE;
            }
        },
    };

    let caps = Caps {
        allow_mutate: cli.allow_mutate,
    };
    let dispatch = ApiDispatch {
        url: cli.server,
        conn: RefCell::new(conn),
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
