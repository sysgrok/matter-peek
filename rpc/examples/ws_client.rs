//! A minimal WS client that drives the RPC server through one `RunVertical`,
//! speaking the exact `matter-peek-proto` contract the Yew UI uses. Doubles as a
//! headless smoke test of the full server↔core↔device pipe.
//!
//! ```text
//! ws_client [WS_URL] [PEER_ADDR] [PASSCODE]
//! # defaults: ws://127.0.0.1:8087  [::1]:5540  20202021
//! ```

use async_tungstenite::tungstenite::Message;
use futures_util::{SinkExt, StreamExt};

use matter_peek_proto as proto;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let mut args = std::env::args().skip(1);
    let url = args.next().unwrap_or_else(|| "ws://127.0.0.1:8087".into());
    let peer_addr = args.next().unwrap_or_else(|| "[::1]:5540".into());
    let passcode: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(20202021);

    futures_lite::future::block_on(async move {
        let stream = async_net::TcpStream::connect(url.trim_start_matches("ws://"))
            .await
            .expect("tcp connect");
        let (ws, _) = async_tungstenite::client_async(&url, stream)
            .await
            .expect("ws handshake");
        let (mut write, mut read) = ws.split();

        let client_msg = proto::ClientMsg {
            req_id: 1,
            request: proto::Request::RunVertical {
                peer_addr,
                passcode,
                policy: proto::FabricPolicy::Ephemeral,
            },
        };
        write
            .send(Message::Text(serde_json::to_string(&client_msg).unwrap()))
            .await
            .expect("send");
        println!(">> sent RunVertical");

        while let Some(frame) = read.next().await {
            let text = match frame {
                Ok(Message::Text(t)) => t,
                Ok(Message::Close(_)) => break,
                Ok(_) => continue,
                Err(e) => {
                    eprintln!("ws error: {e}");
                    break;
                }
            };
            let msg: proto::ServerMsg = serde_json::from_str(&text).expect("parse ServerMsg");
            match msg {
                proto::ServerMsg::Event { event } => {
                    let proto::Event::Step { step, status, .. } = event;
                    println!("   event: {step:?} -> {status:?}");
                }
                proto::ServerMsg::Response { response, .. } => {
                    println!("<< response: {response:#?}");
                    break;
                }
            }
        }
    });
}
