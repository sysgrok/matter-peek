//! `matter-peek mcp`: the API as MCP tools, for AI agents, over stdio.
//!
//! Speaks newline-delimited JSON-RPC 2.0 (the MCP stdio transport) on
//! stdin/stdout; each `tools/call` becomes a request of the API
//! ([`matter_peek_core::api::mcp`]). Logs go to stderr, so they never corrupt the
//! stdout stream.

use core::cell::RefCell;

use std::io;
use std::rc::Rc;

use futures_lite::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use futures_lite::StreamExt;

use log::{debug, info, warn};

use matter_peek_core::api::mcp::{handle, jsonrpc, Caps, Dispatch};
use matter_peek_core::api::{wire, PeekConfig};
use matter_peek_server::client::{self, Connection};

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
                warn!("mcp: {e}; reconnecting");
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

/// Serve the MCP tools on stdin/stdout until stdin closes: through the server at
/// `server`, else through a controller started in this process.
pub async fn serve(server: Option<String>, allow_mutate: bool) -> io::Result<()> {
    info!(
        "mcp: starting, {} (mutating tools {})",
        match &server {
            Some(url) => format!("server {url}"),
            None => "controller in this process".into(),
        },
        if allow_mutate { "ENABLED" } else { "disabled" }
    );

    // Started at once, so that a store held by another process fails here, and
    // the device list is synced before the first tool call.
    let conn = match &server {
        Some(_) => None,
        // Long-lived, like a server: keep the device list in sync.
        None => Some(Rc::new(client::connect_in_process(PeekConfig {
            store_dir: None,
            periodic_sync: true,
        })?)),
    };

    let dispatch = ApiDispatch {
        url: server,
        conn: RefCell::new(conn),
    };

    stdio_loop(&dispatch, Caps { allow_mutate }).await
}

/// The newline-delimited JSON-RPC loop: read a line from stdin, handle it,
/// write the response line to stdout. Ends at EOF (the client closed stdin).
///
/// stdin/stdout are async-wrapped with `blocking::Unblock` (a thread pool), the
/// smol-family way to do async stdio: `async_io::Async` over a pipe/tty fd can
/// fail to register with epoll.
async fn stdio_loop(dispatch: &ApiDispatch, caps: Caps) -> io::Result<()> {
    let stdin = BufReader::new(blocking::Unblock::new(io::stdin()));
    let mut stdout = blocking::Unblock::new(io::stdout());
    let mut lines = stdin.lines();

    while let Some(line) = lines.next().await {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        let req: jsonrpc::Request = match serde_json::from_str(&line) {
            Ok(req) => req,
            Err(e) => {
                // Without an id, there is no one to answer.
                debug!("mcp: bad JSON-RPC line: {e}");
                continue;
            }
        };

        if let Some(resp) = handle(req, dispatch, caps).await {
            let mut json = serde_json::to_string(&resp).unwrap_or_default();
            json.push('\n');
            stdout.write_all(json.as_bytes()).await?;
            stdout.flush().await?;
        }
    }

    info!("mcp: stdin closed, exiting");
    Ok(())
}
