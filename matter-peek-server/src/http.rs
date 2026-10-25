//! The API over WebSocket/JSON, and the web UI over HTTP, on one port.
//!
//! A connection that asks to upgrade to WebSocket is the API: a [`Session`]
//! of the [`Hub`], whose [`ClientMsg`] frames run as requests, and whose
//! responses and events go back as [`ServerMsg`] frames. Any other request is
//! for a file of the web UI.

use std::rc::Rc;
use std::time::{Duration, Instant};

use async_tungstenite::tungstenite::Message;
use futures_lite::{AsyncRead, AsyncWrite};
use futures_util::{SinkExt, StreamExt};

use async_io::Timer;
use async_net::TcpStream;

use log::{debug, error, info, warn};

use matter_peek_core::api::wire::{ClientMsg, ServerMsg};

use crate::hub::Hub;

/// The most bytes a request's head may have.
const MAX_HEAD_LEN: usize = 8 * 1024;
/// How long a client may take to send it.
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);
/// How often to look whether more of it arrived.
const HEAD_POLL: Duration = Duration::from_millis(10);

/// Serve the API of `hub` and the web UI on `addr`, forever.
pub(crate) async fn serve(addr: &str, hub: Rc<Hub>) -> std::io::Result<()> {
    let listener = async_net::TcpListener::bind(addr).await?;
    info!("server: the web UI at http://{addr}/, the API at ws://{addr}/ws");

    loop {
        let (stream, peer) = listener.accept().await?;
        debug!("server: connection from {peer}");

        let hub = hub.clone();
        super::spawn(async move {
            if let Err(e) = handle_conn(stream, &hub).await {
                debug!("server: connection {peer} ended: {e}");
            }
        });
    }
}

/// The API, if the connection asks to upgrade to WebSocket; else a file of
/// the web UI.
async fn handle_conn(stream: TcpStream, hub: &Hub) -> Result<(), String> {
    let head = peek_head(&stream).await.map_err(|e| e.to_string())?;

    if is_upgrade(&head) {
        websocket(stream, hub).await
    } else {
        super::web::respond(stream, &head)
            .await
            .map_err(|e| e.to_string())
    }
}

/// The head of the request on `stream`, left in it: the WebSocket handshake
/// reads it again.
async fn peek_head(stream: &TcpStream) -> std::io::Result<String> {
    let mut buf = vec![0; MAX_HEAD_LEN];
    let started = Instant::now();

    loop {
        let len = stream.peek(&mut buf).await?;
        if len == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        if let Some(end) = buf[..len].windows(4).position(|w| w == b"\r\n\r\n") {
            return Ok(String::from_utf8_lossy(&buf[..end + 4]).into_owned());
        }
        if len == buf.len() || started.elapsed() > HEAD_TIMEOUT {
            return Err(std::io::ErrorKind::InvalidData.into());
        }

        Timer::after(HEAD_POLL).await;
    }
}

/// Whether the request asks to upgrade to WebSocket.
fn is_upgrade(head: &str) -> bool {
    head.lines().skip(1).any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.trim().eq_ignore_ascii_case("upgrade")
                && value.trim().eq_ignore_ascii_case("websocket")
        })
    })
}

async fn websocket<S>(stream: S, hub: &Hub) -> Result<(), String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let ws = async_tungstenite::accept_async(stream)
        .await
        .map_err(|e| e.to_string())?;
    let (mut write, mut read) = ws.split();

    let (out, out_rx) = async_channel::unbounded::<ServerMsg>();
    let session = hub.connect(out);

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

            session.handle(msg);
        }
    };

    futures_lite::future::or(reader, writer).await;

    Ok(())
}
