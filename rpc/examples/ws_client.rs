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

    // Usage:
    //   ws_client [URL] discover
    //   ws_client [URL] list
    //   ws_client [URL] peek <node_id_hex_or_dec>
    //   ws_client [URL] identify <node_id> [secs]
    //   ws_client [URL] settags <node_id> <comma,separated,tags>
    //   ws_client [URL] persistent <addr> [passcode]   (persistent commission)
    //   ws_client [URL] <addr> [passcode]               (ephemeral vertical)
    let mut args = std::env::args().skip(1);
    let url = args.next().unwrap_or_else(|| "ws://127.0.0.1:8087".into());
    let arg2 = args.next().unwrap_or_else(|| "[::1]:5540".into());
    let arg3 = args.next();

    let parse_node = |s: &str| -> u64 {
        s.strip_prefix("0x")
            .and_then(|h| u64::from_str_radix(h, 16).ok())
            .or_else(|| s.parse().ok())
            .unwrap_or(0)
    };

    let request = match arg2.as_str() {
        "discover" => proto::Request::Discover {
            discriminator: None,
            max: 16,
        },
        "list" => proto::Request::ListDevices,
        "cancel" => proto::Request::Cancel,
        "peek" => proto::Request::PeekDevice {
            node_id: parse_node(arg3.as_deref().unwrap_or("0")),
            with_logs: false,
        },
        "identify" => proto::Request::Identify {
            node_id: parse_node(arg3.as_deref().unwrap_or("0")),
            secs: 3,
        },
        "settags" => proto::Request::SetTags {
            node_id: parse_node(arg3.as_deref().unwrap_or("0")),
            tags: args
                .next()
                .unwrap_or_default()
                .split(',')
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect(),
        },
        "persistent" => proto::Request::RunVertical {
            peer_addr: arg3.clone().unwrap_or_else(|| "[::1]:5540".into()),
            passcode: args.next().and_then(|s| s.parse().ok()).unwrap_or(20202021),
            policy: proto::FabricPolicy::Persistent,
        },
        addr => proto::Request::RunVertical {
            peer_addr: addr.to_string(),
            passcode: arg3.and_then(|s| s.parse().ok()).unwrap_or(20202021),
            policy: proto::FabricPolicy::Ephemeral,
        },
    };

    futures_lite::future::block_on(async move {
        let stream = async_net::TcpStream::connect(url.trim_start_matches("ws://"))
            .await
            .expect("tcp connect");
        let (ws, _) = async_tungstenite::client_async(&url, stream)
            .await
            .expect("ws handshake");
        let (mut write, mut read) = ws.split();

        let client_msg = proto::ClientMsg { req_id: 1, request };
        write
            .send(Message::Text(serde_json::to_string(&client_msg).unwrap()))
            .await
            .expect("send");
        println!(">> sent {:?}", client_msg.request);

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
