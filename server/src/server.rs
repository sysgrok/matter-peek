//! The WebSocket/JSON server.
//!
//! Each connection is a [`WireSession`]: its [`ClientMsg`] frames run as
//! requests (concurrently, so a `Cancel` reaches a running `Commission`), and
//! their responses and events go back as [`ServerMsg`] frames. The chat
//! requests go to the assistant's [`Chats`], which all connections share.

use std::rc::Rc;
use std::sync::Arc;

use async_tungstenite::tungstenite::Message;
use futures_lite::{AsyncRead, AsyncWrite};
use futures_util::{SinkExt, StreamExt};

use log::{debug, error, info, warn};

use matter_peek_core::api::wire::{ClientMsg, ServerMsg, WireSession};
use matter_peek_core::api::MatterPeek;

use crate::chat::Chats;

/// Serve WebSocket connections on `addr` with `peek` and `chats`, forever.
pub(crate) async fn serve(
    addr: &str,
    peek: Arc<MatterPeek>,
    chats: Rc<Chats>,
) -> std::io::Result<()> {
    let listener = async_net::TcpListener::bind(addr).await?;
    info!("server: listening on ws://{addr}");

    loop {
        let (stream, peer) = listener.accept().await?;
        debug!("server: connection from {peer}");

        let peek = peek.clone();
        let chats = chats.clone();
        crate::spawn(async move {
            if let Err(e) = handle_conn(stream, peek, chats).await {
                debug!("server: connection {peer} ended: {e}");
            }
        });
    }
}

async fn handle_conn<S>(stream: S, peek: Arc<MatterPeek>, chats: Rc<Chats>) -> Result<(), String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let ws = async_tungstenite::accept_async(stream)
        .await
        .map_err(|e| e.to_string())?;
    let (mut write, mut read) = ws.split();

    let (out, out_rx) = async_channel::unbounded::<ServerMsg>();
    let session = Rc::new(WireSession::new(peek, out.clone()));

    let writer = async {
        while let Ok(msg) = out_rx.recv().await {
            let json = match serde_json::to_string(&msg) {
                Ok(json) => json,
                Err(e) => {
                    error!("server: cannot serialize a message: {e}");
                    continue;
                }
            };

            if write.send(Message::Text(json)).await.is_err() {
                break;
            }
        }
    };

    let reader = async {
        while let Some(frame) = read.next().await {
            let text = match frame {
                Ok(Message::Text(text)) => text,
                Ok(Message::Close(_)) => break,
                Ok(_) => continue,
                Err(e) => {
                    debug!("server: read error: {e}");
                    break;
                }
            };

            let msg: ClientMsg = match serde_json::from_str(&text) {
                Ok(msg) => msg,
                Err(e) => {
                    warn!("server: bad message: {e}");
                    continue;
                }
            };

            if msg.request.is_chat() {
                let chats = chats.clone();
                let out = out.clone();
                crate::spawn(async move { chats.handle(msg, out).await });
            } else {
                let session = session.clone();
                crate::spawn(async move { session.handle(msg).await });
            }
        }
    };

    futures_lite::future::or(reader, writer).await;

    Ok(())
}
