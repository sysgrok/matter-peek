//! Connections to the matter-peek API, for its clients (the CLI, the MCP server).
//!
//! A [`Connection`] carries [`ClientMsg`]s in and [`ServerMsg`]s out, either over
//! a WebSocket to a running server (`matter-peek serve`) ([`connect`]) or to a
//! controller in this process ([`connect_in_process`]). Either way, the requests
//! are serviced the same way, the assistant's too.

use core::cell::Cell;

use std::io;
use std::rc::Rc;

use async_tungstenite::tungstenite::Message;
use futures_util::{SinkExt, StreamExt};

use log::{debug, warn};

use matter_peek_core::api::platform::data_dir;
use matter_peek_core::api::wire::{
    ClientMsg, Event, ReqId, Request, Response, ServerMsg, WireSession,
};
use matter_peek_core::api::{MatterPeek, PeekConfig};

use super::chats::Chats;
use super::llm::model_from_env;

/// A connection to the API. Runs on the thread-local executor, so it must be
/// driven by [`super::block_on`].
pub struct Connection {
    tx: async_channel::Sender<ClientMsg>,
    rx: async_channel::Receiver<ServerMsg>,
    next_req_id: Cell<ReqId>,
}

impl Connection {
    fn new(tx: async_channel::Sender<ClientMsg>, rx: async_channel::Receiver<ServerMsg>) -> Self {
        Self {
            tx,
            rx,
            next_req_id: Cell::new(1),
        }
    }

    /// Send `request`, returning its ID.
    pub async fn send(&self, request: Request) -> io::Result<ReqId> {
        let req_id = self.next_req_id.get();
        self.next_req_id.set(req_id + 1);

        self.tx
            .send(ClientMsg { req_id, request })
            .await
            .map_err(|_| closed())?;

        Ok(req_id)
    }

    /// The next response or event; `None` once the connection is closed.
    pub async fn recv(&self) -> Option<ServerMsg> {
        self.rx.recv().await.ok()
    }

    /// Send `request` and wait for its response, handing `on_event` the events
    /// that arrive in the meantime.
    pub async fn call(
        &self,
        request: Request,
        mut on_event: impl FnMut(Event),
    ) -> io::Result<Response> {
        let req_id = self.send(request).await?;

        loop {
            match self.recv().await.ok_or_else(closed)? {
                ServerMsg::Response {
                    req_id: id,
                    response,
                } if id == req_id => return Ok(response),
                ServerMsg::Response { .. } => {}
                ServerMsg::Event { event } => on_event(event),
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

    let (client_tx, client_rx) = async_channel::unbounded::<ClientMsg>();
    let (server_tx, server_rx) = async_channel::unbounded::<ServerMsg>();

    super::spawn(async move {
        while let Ok(msg) = client_rx.recv().await {
            let Ok(json) = serde_json::to_string(&msg) else {
                continue;
            };

            if write.send(Message::Text(json)).await.is_err() {
                break;
            }
        }
    });

    super::spawn(async move {
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
                Err(e) => warn!("client: bad message: {e}"),
            }
        }
    });

    Ok(Connection::new(client_tx, server_rx))
}

/// Start a controller in this process, as `config` says, and connect to it;
/// the assistant runs here too, as in the server. Fails if another matter-peek
/// process (e.g. a running server) holds the store.
pub fn connect_in_process(config: PeekConfig) -> io::Result<Connection> {
    let peek = MatterPeek::start(config).map_err(io::Error::other)?;

    let (client_tx, client_rx) = async_channel::unbounded::<ClientMsg>();
    let (server_tx, server_rx) = async_channel::unbounded::<ServerMsg>();

    let chats = Chats::new(peek.clone(), model_from_env(), data_dir().join("chats"));
    let session = Rc::new(WireSession::new(peek, server_tx.clone()));

    super::spawn(async move {
        while let Ok(msg) = client_rx.recv().await {
            if msg.request.is_assistant() {
                let chats = chats.clone();
                let out = server_tx.clone();
                super::spawn(async move { chats.handle(msg, out).await });
            } else {
                let session = session.clone();
                super::spawn(async move { session.handle(msg).await });
            }
        }
    });

    Ok(Connection::new(client_tx, server_rx))
}

fn closed() -> io::Error {
    io::Error::new(io::ErrorKind::ConnectionAborted, "the connection is closed")
}
