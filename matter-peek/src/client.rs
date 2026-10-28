//! Connections to the matter-peek API, for its clients (the CLI, the MCP server).
//!
//! A [`Connection`] carries [`ClientMsg`]s in and [`ServerMsg`]s out, either over
//! a WebSocket to a running server (`matter-peek serve`) ([`connect`]) or to a
//! [`Hub`] in this process ([`connect_in_process`]). Either way, the requests
//! are serviced the same way, the assistant's too. An [`Endpoint`] says which.

use core::cell::Cell;

use std::io;
use std::path::PathBuf;
use std::rc::Rc;

use async_tungstenite::tungstenite::client::IntoClientRequest as _;
use async_tungstenite::tungstenite::http::HeaderValue;
use async_tungstenite::tungstenite::Message;
use futures_util::{SinkExt, StreamExt};

use log::{debug, warn};

use matter_peek_core::api::wire::{ClientMsg, Event, ReqId, Request, Response, ServerMsg};
use matter_peek_core::api::PeekConfig;
use matter_peek_server::llm::model_from_env;
use matter_peek_server::{spawn, Hub, UNAUTHORIZED};

/// Where the API is: the server at `server`, given its access token `token`;
/// else a controller started in this process, with its store in `store_dir`
/// (else in the user's data directory).
#[derive(Clone, Default)]
pub struct Endpoint {
    pub server: Option<String>,
    pub token: Option<String>,
    pub store_dir: Option<PathBuf>,
}

impl Endpoint {
    /// The configuration of a controller started in this process.
    pub fn in_process(&self, periodic_sync: bool) -> PeekConfig {
        PeekConfig {
            store_dir: self.store_dir.as_ref().map(|dir| dir.display().to_string()),
            periodic_sync,
        }
    }
}

/// A connection to the API. Runs on the thread-local executor, so it must be
/// driven by [`matter_peek_server::block_on`].
pub struct Connection {
    tx: async_channel::Sender<ClientMsg>,
    rx: async_channel::Receiver<ServerMsg>,
    next_req_id: Cell<ReqId>,
    /// The server refused the connection: no access token, or a wrong one.
    refused: Rc<Cell<bool>>,
}

impl Connection {
    fn new(
        tx: async_channel::Sender<ClientMsg>,
        rx: async_channel::Receiver<ServerMsg>,
        refused: Rc<Cell<bool>>,
    ) -> Self {
        Self {
            tx,
            rx,
            next_req_id: Cell::new(1),
            refused,
        }
    }

    /// Send `request`, returning its ID.
    pub async fn send(&self, request: Request) -> io::Result<ReqId> {
        let req_id = self.next_req_id.get();
        self.next_req_id.set(req_id + 1);

        self.tx
            .send(ClientMsg { req_id, request })
            .await
            .map_err(|_| self.closed())?;

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
            match self.recv().await.ok_or_else(|| self.closed())? {
                ServerMsg::Response {
                    req_id: id,
                    response,
                } if id == req_id => return Ok(response),
                ServerMsg::Response { .. } => {}
                ServerMsg::Event { event } => on_event(event),
            }
        }
    }

    /// Why the connection is closed.
    fn closed(&self) -> io::Error {
        if self.refused.get() {
            return io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the server wants its access token: give it with --token or MATTER_PEEK_TOKEN \
                 (`matter-peek token` prints it on the server's machine)",
            );
        }

        io::Error::new(io::ErrorKind::ConnectionAborted, "the connection is closed")
    }
}

/// Connect to the matter-peek server at `url` (`ws://host:port/ws`), giving it
/// its access token `token`.
pub async fn connect(url: &str, token: Option<&str>) -> io::Result<Connection> {
    let addr = url
        .strip_prefix("ws://")
        .and_then(|rest| rest.split('/').next())
        .ok_or_else(|| io::Error::other(format!("not a ws:// URL: {url}")))?;

    let mut request = url.into_client_request().map_err(io::Error::other)?;
    if let Some(token) = token {
        let bearer = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| io::Error::other("the access token is not valid"))?;
        request.headers_mut().insert("authorization", bearer);
    }

    let stream = async_net::TcpStream::connect(addr)
        .await
        .map_err(|e| io::Error::new(e.kind(), format!("cannot reach the server at {url}: {e}")))?;
    let (ws, _) = async_tungstenite::client_async(request, stream)
        .await
        .map_err(io::Error::other)?;
    let (mut write, mut read) = ws.split();

    let (client_tx, client_rx) = async_channel::unbounded::<ClientMsg>();
    let (server_tx, server_rx) = async_channel::unbounded::<ServerMsg>();

    spawn(async move {
        while let Ok(msg) = client_rx.recv().await {
            let Ok(json) = serde_json::to_string(&msg) else {
                continue;
            };

            if write.send(Message::Text(json)).await.is_err() {
                break;
            }
        }
    });

    let refused = Rc::new(Cell::new(false));
    let refusal = refused.clone();
    spawn(async move {
        while let Some(frame) = read.next().await {
            let text = match frame {
                Ok(Message::Text(text)) => text,
                Ok(Message::Close(frame)) => {
                    let code = frame.map(|frame| u16::from(frame.code));
                    refusal.set(code == Some(UNAUTHORIZED));
                    break;
                }
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

    Ok(Connection::new(client_tx, server_rx, refused))
}

/// Start a controller in this process, as `config` says, and connect to it;
/// the assistant runs here too, as in the server, with its chats next to the
/// store. Fails if another matter-peek process (e.g. a running server) holds
/// the store.
pub fn connect_in_process(config: PeekConfig) -> io::Result<Connection> {
    let hub = Hub::start(config, model_from_env(), false)?;

    let (client_tx, client_rx) = async_channel::unbounded::<ClientMsg>();
    let (server_tx, server_rx) = async_channel::unbounded::<ServerMsg>();

    let session = hub.connect(server_tx);
    spawn(async move {
        while let Ok(msg) = client_rx.recv().await {
            session.handle(msg);
        }
    });

    Ok(Connection::new(client_tx, server_rx, Rc::default()))
}
