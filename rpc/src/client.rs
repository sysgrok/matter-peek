//! Connections to the matter-peek API, for its clients (the CLI, the MCP server).
//!
//! A [`Connection`] carries [`proto::ClientMsg`]s in and [`proto::ServerMsg`]s out,
//! either over a WebSocket to a running `matter-peek-server` ([`connect`]) or to
//! an actor in this process ([`connect_in_process`]). Either way, the requests
//! are serviced the same way.

use core::cell::Cell;

use std::io;

use async_tungstenite::tungstenite::Message;
use futures_util::{SinkExt, StreamExt};

use log::{debug, warn};

use matter_peek_proto as proto;

use crate::actor::{self, CancelFlag, Command, Store};
use crate::server;

/// A connection to the API. Runs on the thread-local executor, so it must be
/// driven by [`crate::block_on`].
pub struct Connection {
    tx: async_channel::Sender<proto::ClientMsg>,
    rx: async_channel::Receiver<proto::ServerMsg>,
    next_req_id: Cell<proto::ReqId>,
}

impl Connection {
    fn new(
        tx: async_channel::Sender<proto::ClientMsg>,
        rx: async_channel::Receiver<proto::ServerMsg>,
    ) -> Self {
        Self {
            tx,
            rx,
            next_req_id: Cell::new(1),
        }
    }

    /// Send `request`, returning its ID.
    pub async fn send(&self, request: proto::Request) -> io::Result<proto::ReqId> {
        let req_id = self.next_req_id.get();
        self.next_req_id.set(req_id + 1);

        self.tx
            .send(proto::ClientMsg { req_id, request })
            .await
            .map_err(|_| closed())?;

        Ok(req_id)
    }

    /// The next response or event; `None` once the connection is closed.
    pub async fn recv(&self) -> Option<proto::ServerMsg> {
        self.rx.recv().await.ok()
    }

    /// Send `request` and wait for its response, handing `on_event` the events
    /// that arrive in the meantime.
    pub async fn call(
        &self,
        request: proto::Request,
        mut on_event: impl FnMut(proto::Event),
    ) -> io::Result<proto::Response> {
        let req_id = self.send(request).await?;

        loop {
            match self.recv().await.ok_or_else(closed)? {
                proto::ServerMsg::Response {
                    req_id: id,
                    response,
                } if id == req_id => return Ok(response),
                proto::ServerMsg::Response { .. } => {}
                proto::ServerMsg::Event { event } => on_event(event),
            }
        }
    }
}

/// Connect to the matter-peek server at `url` (`ws://host:port`).
pub async fn connect(url: &str) -> io::Result<Connection> {
    let addr = url
        .strip_prefix("ws://")
        .and_then(|rest| rest.split('/').next())
        .ok_or_else(|| io::Error::other(format!("not a ws:// URL: {url}")))?;

    let stream = async_net::TcpStream::connect(addr)
        .await
        .map_err(|e| io::Error::new(e.kind(), format!("cannot reach the server at {url}: {e}")))?;
    let (ws, _) = async_tungstenite::client_async(url, stream)
        .await
        .map_err(io::Error::other)?;
    let (mut write, mut read) = ws.split();

    let (client_tx, client_rx) = async_channel::unbounded::<proto::ClientMsg>();
    let (server_tx, server_rx) = async_channel::unbounded::<proto::ServerMsg>();

    crate::spawn(async move {
        while let Ok(msg) = client_rx.recv().await {
            let Ok(json) = serde_json::to_string(&msg) else {
                continue;
            };

            if write.send(Message::Text(json)).await.is_err() {
                break;
            }
        }
    });

    crate::spawn(async move {
        while let Some(frame) = read.next().await {
            let text = match frame {
                Ok(Message::Text(text)) => text,
                Ok(Message::Close(_)) => break,
                Ok(_) => continue,
                Err(e) => {
                    debug!("client: read error: {e}");
                    break;
                }
            };

            match serde_json::from_str(&text) {
                Ok(msg) => {
                    if server_tx.send(msg).await.is_err() {
                        break;
                    }
                }
                Err(e) => warn!("client: bad ServerMsg: {e}"),
            }
        }
    });

    Ok(Connection::new(client_tx, server_rx))
}

/// Start an actor in this process and connect to it. Fails if another
/// matter-peek process (e.g. a running server) holds the store.
pub fn connect_in_process() -> io::Result<Connection> {
    let store = Store::lock()?;

    let (cmd_tx, cmd_rx) = async_channel::unbounded::<Command>();
    let cancel = CancelFlag::default();

    // Short-lived: no periodic sync, which the process could exit in the middle of.
    crate::spawn(actor::run(cmd_rx, cancel.clone(), store, false));

    let (client_tx, client_rx) = async_channel::unbounded::<proto::ClientMsg>();
    let (server_tx, server_rx) = async_channel::unbounded::<proto::ServerMsg>();

    crate::spawn(async move {
        while let Ok(msg) = client_rx.recv().await {
            server::dispatch(msg, &cmd_tx, &server_tx, &cancel).await;
        }
    });

    Ok(Connection::new(client_tx, server_rx))
}

fn closed() -> io::Error {
    io::Error::new(io::ErrorKind::ConnectionAborted, "the connection is closed")
}
