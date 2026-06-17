//! The WebSocket/JSON server.
//!
//! Accepts WS connections on a local TCP port. For each connection: parse
//! incoming [`proto::ClientMsg`] frames, hand each off to the actor as a
//! [`Command`] (with fresh per-command event + reply channels), and serialize
//! the resulting [`proto::ServerMsg`] frames back to the client.
//!
//! Runtime: `async-net` (smol/async-io family) to match the core's runtime, so
//! the whole process runs on one executor without mixing tokio in.

use async_tungstenite::tungstenite::Message;
use futures_lite::{AsyncRead, AsyncWrite};
use futures_util::{SinkExt, StreamExt};

use log::{debug, error, info, warn};

use matter_peek_proto as proto;

use crate::actor::Command;

/// Serve WS connections forever on `addr`, dispatching to the actor via `tx`.
pub async fn serve(addr: &str, tx: async_channel::Sender<Command>) -> std::io::Result<()> {
    let listener = async_net::TcpListener::bind(addr).await?;
    info!("rpc: listening on ws://{addr}");

    loop {
        let (stream, peer) = listener.accept().await?;
        debug!("rpc: connection from {peer}");
        let tx = tx.clone();
        // One task per connection.
        crate::spawn(async move {
            if let Err(e) = handle_conn(stream, tx).await {
                debug!("rpc: connection {peer} ended: {e}");
            }
        });
    }
}

async fn handle_conn<S>(stream: S, tx: async_channel::Sender<Command>) -> anyhow_lite::Result
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let ws = async_tungstenite::accept_async(stream).await.map_err(de)?;
    let (mut write, mut read) = ws.split();

    // Fan-in: both per-command replies and events are serialized out on `write`.
    // A single outbound channel keeps `write` owned by one place.
    let (out_tx, out_rx) = async_channel::unbounded::<proto::ServerMsg>();

    // Writer task: drain outbound ServerMsgs to the socket.
    let writer = async {
        while let Ok(msg) = out_rx.recv().await {
            let json = serde_json::to_string(&msg).unwrap_or_else(|e| {
                error!("rpc: serialize ServerMsg failed: {e}");
                String::new()
            });
            if json.is_empty() {
                continue;
            }
            if write.send(Message::Text(json)).await.is_err() {
                break;
            }
        }
    };

    // Reader task: parse ClientMsgs and dispatch commands.
    let reader = async {
        while let Some(frame) = read.next().await {
            let text = match frame {
                Ok(Message::Text(t)) => t,
                Ok(Message::Close(_)) => break,
                Ok(_) => continue, // ignore binary/ping/pong
                Err(e) => {
                    debug!("rpc: read error: {e}");
                    break;
                }
            };

            let client_msg: proto::ClientMsg = match serde_json::from_str(&text) {
                Ok(m) => m,
                Err(e) => {
                    warn!("rpc: bad ClientMsg: {e}");
                    continue;
                }
            };

            dispatch(client_msg, &tx, &out_tx).await;
        }
    };

    // Run reader and writer until either ends.
    futures_lite::future::or(reader, writer).await;
    Ok(())
}

/// Turn one client request into an actor [`Command`] and pump its events +
/// final reply out to the connection's outbound channel.
async fn dispatch(
    msg: proto::ClientMsg,
    tx: &async_channel::Sender<Command>,
    out_tx: &async_channel::Sender<proto::ServerMsg>,
) {
    let req_id = msg.req_id;
    let (ev_tx, ev_rx) = async_channel::unbounded::<proto::Event>();
    let (rep_tx, rep_rx) = async_channel::bounded::<proto::Response>(1);

    let cmd = Command {
        req_id,
        request: msg.request,
        events: ev_tx,
        reply: rep_tx,
    };

    if tx.send(cmd).await.is_err() {
        let _ = out_tx
            .send(proto::ServerMsg::Response {
                req_id,
                response: proto::Response::Error {
                    message: "server shutting down".into(),
                },
            })
            .await;
        return;
    }

    // Relay events as they arrive; once the single reply lands, forward it and
    // drain any trailing events, then stop. Events and reply both flow out on
    // the connection's `out_tx`.
    let out_tx = out_tx.clone();
    crate::spawn(async move {
        // Pump events until the reply channel produces the final response.
        let response = loop {
            match futures_lite::future::or(async { Sel::Event(ev_rx.recv().await.ok()) }, async {
                Sel::Reply(rep_rx.recv().await.ok())
            })
            .await
            {
                Sel::Event(Some(event)) => {
                    let _ = out_tx.send(proto::ServerMsg::Event { event }).await;
                }
                Sel::Event(None) => {
                    // Event channel closed before a reply — wait for the reply.
                    break rep_rx.recv().await.ok();
                }
                Sel::Reply(resp) => break resp,
            }
        };

        // Drain any events the actor emitted just before/with the reply.
        while let Ok(event) = ev_rx.try_recv() {
            let _ = out_tx.send(proto::ServerMsg::Event { event }).await;
        }

        let response = response.unwrap_or(proto::Response::Error {
            message: "actor dropped without reply".into(),
        });
        let _ = out_tx
            .send(proto::ServerMsg::Response { req_id, response })
            .await;
    });
}

enum Sel {
    Event(Option<proto::Event>),
    Reply(Option<proto::Response>),
}

/// Map a tungstenite error to our lite error type.
fn de<E: std::fmt::Display>(e: E) -> anyhow_lite::Error {
    anyhow_lite::Error(e.to_string())
}

/// A tiny no-dependency error wrapper (avoids pulling `anyhow` just for the
/// server's connection result).
mod anyhow_lite {
    pub struct Error(pub String);
    impl std::fmt::Display for Error {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.0)
        }
    }
    pub type Result = core::result::Result<(), Error>;
}
