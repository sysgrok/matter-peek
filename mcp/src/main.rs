//! matter-peek-mcp binary — the MCP stdio server.
//!
//! ```text
//! matter-peek-mcp [--allow-mutate]
//! ```
//!
//! Speaks newline-delimited JSON-RPC 2.0 (the MCP stdio transport) on
//! stdin/stdout. Each `tools/call` is run by the **same actor** the WS server
//! (`matter-peek-rpc`) uses — one `Command` per call. Logs go to **stderr** so
//! they never corrupt the stdout JSON stream.
//!
//! Read-only tools are always available; mutating tools (commission / set_tags)
//! require `--allow-mutate`.

use std::process::ExitCode;

use futures_lite::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use futures_lite::StreamExt;

use log::{debug, error, info};

use matter_peek_mcp::{handle, Caps, Dispatch};
use matter_peek_proto as proto;
use matter_peek_rpc::actor::{self, Command};

/// A [`Dispatch`] over the `matter-peek-rpc` actor channel: build one `Command`,
/// send it, await the single reply (events are drained + ignored — a tool call
/// is request/response).
struct ActorDispatch {
    tx: async_channel::Sender<Command>,
}

impl Dispatch for ActorDispatch {
    async fn dispatch(&self, request: proto::Request) -> Result<proto::Response, String> {
        let (ev_tx, ev_rx) = async_channel::unbounded::<proto::Event>();
        let (rep_tx, rep_rx) = async_channel::bounded::<proto::Response>(1);

        let cmd = Command {
            req_id: 1,
            request,
            events: ev_tx,
            reply: rep_tx,
        };
        self.tx
            .send(cmd)
            .await
            .map_err(|_| "controller actor unavailable".to_string())?;

        // Drain events so the actor never blocks on a full event channel, while
        // waiting for the single reply.
        loop {
            match futures_lite::future::or(async { Sel::Event(ev_rx.recv().await.ok()) }, async {
                Sel::Reply(rep_rx.recv().await.ok())
            })
            .await
            {
                Sel::Event(Some(_)) => continue,
                Sel::Event(None) => {
                    return rep_rx
                        .recv()
                        .await
                        .map_err(|_| "actor dropped without reply".to_string());
                }
                Sel::Reply(Some(resp)) => return Ok(resp),
                Sel::Reply(None) => return Err("actor dropped without reply".to_string()),
            }
        }
    }
}

enum Sel {
    Event(Option<proto::Event>),
    Reply(Option<proto::Response>),
}

fn main() -> ExitCode {
    // Logs to STDERR — stdout is the JSON-RPC channel and must stay clean.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .target(env_logger::Target::Stderr)
        .init();

    let allow_mutate = std::env::args().any(|a| a == "--allow-mutate");
    info!(
        "matter-peek-mcp: starting (mutating tools {})",
        if allow_mutate { "ENABLED" } else { "disabled" }
    );

    let caps = Caps { allow_mutate };

    // One actor + the stdio loop, both on the single-threaded executor (the
    // Matter stack is !Send). `matter_peek_rpc::spawn` puts the actor on the same
    // thread-local executor the loop is driven by.
    let (cmd_tx, cmd_rx) = async_channel::unbounded::<Command>();
    // MCP tool calls are request/response (no streaming, no cancel), so the
    // actor's cancel flag is supplied but never set here.
    let cancel: actor::CancelFlag = std::rc::Rc::new(std::cell::Cell::new(false));
    matter_peek_rpc::spawn(actor::run(cmd_rx, cancel));

    let dispatch = ActorDispatch { tx: cmd_tx };

    match matter_peek_rpc::block_on(stdio_loop(dispatch, caps)) {
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
