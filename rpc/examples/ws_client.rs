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
    //   ws_client [URL] subscribe <node_id> [secs]      (live health stream)
    //   ws_client [URL] settags <node_id> <comma,separated,tags>
    //   ws_client [URL] ota-images
    //   ws_client [URL] ota-upload <file.ota>
    //   ws_client [URL] ota-delete <name>
    //   ws_client [URL] ota-update <node_id> <name>   (follows the update)
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
        "subscribe" => proto::Request::Subscribe {
            node_id: parse_node(arg3.as_deref().unwrap_or("0")),
        },
        "unsubscribe" => proto::Request::Unsubscribe {
            node_id: parse_node(arg3.as_deref().unwrap_or("0")),
        },
        "decommission" => proto::Request::Decommission {
            node_id: parse_node(arg3.as_deref().unwrap_or("0")),
            force: args.next().as_deref() == Some("force"),
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
        "ota-images" => proto::Request::OtaImages,
        "ota-upload" => {
            let path = arg3.expect("an OTA image file");
            futures_lite::future::block_on(upload(&url, &path));
            return;
        }
        "ota-delete" => proto::Request::OtaDelete {
            name: arg3.clone().unwrap_or_default(),
        },
        "ota-update" => proto::Request::OtaUpdate {
            node_id: parse_node(arg3.as_deref().unwrap_or("0")),
            name: args.next().unwrap_or_default(),
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

    // For `subscribe`, keep the connection open after the `Subscribed` reply and
    // keep printing the streamed events (Ctrl-C to stop). Everything else is a
    // one-shot request → response.
    let is_subscribe = matches!(request, proto::Request::Subscribe { .. });
    // `ota-update` follows the update until the device runs the new version.
    let is_ota_update = matches!(request, proto::Request::OtaUpdate { .. });

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
                proto::ServerMsg::Event { event } => match event {
                    proto::Event::Step { step, status, .. } => {
                        println!("   event: {step:?} -> {status:?}");
                    }
                    proto::Event::HealthReport {
                        node_id,
                        seq,
                        health,
                    } => {
                        println!("   health 0x{node_id:016x} #{seq}: {health:?}");
                    }
                    proto::Event::SubscriptionState { node_id, state } => {
                        println!("   sub 0x{node_id:016x}: {state:?}");
                    }
                    proto::Event::OtaProgress { node_id, progress } => {
                        println!("   ota 0x{node_id:016x}: {progress:?}");
                        if matches!(progress, proto::OtaProgress::Applied { .. }) {
                            break;
                        }
                    }
                },
                proto::ServerMsg::Response { response, .. } => {
                    println!("<< response: {response:#?}");
                    // For a subscription, the data streams *after* this reply — keep
                    // reading events. For one-shot requests, we're done.
                    if is_subscribe {
                        println!("   (streaming live health — Ctrl-C to stop)");
                        continue;
                    }
                    if is_ota_update && matches!(response, proto::Response::Ok) {
                        println!("   (following the update — Ctrl-C to stop)");
                        continue;
                    }
                    break;
                }
            }
        }
    });
}

/// The size of an upload chunk, before base64.
const UPLOAD_CHUNK_LEN: usize = 256 * 1024;

/// Upload an OTA image file in chunks, on one connection.
async fn upload(url: &str, path: &str) {
    use base64::Engine as _;
    use std::io::Read as _;

    let mut file = std::fs::File::open(path).expect("open the OTA image file");
    let size = file.metadata().expect("the file's size").len();

    let stream = async_net::TcpStream::connect(url.trim_start_matches("ws://"))
        .await
        .expect("tcp connect");
    let (ws, _) = async_tungstenite::client_async(url, stream)
        .await
        .expect("ws handshake");
    let (mut write, mut read) = ws.split();

    // Each request gets the next `req_id`, to find its response by.
    let mut req_id = 0;
    let mut call = |request: proto::Request| {
        req_id += 1;
        let text = serde_json::to_string(&proto::ClientMsg { req_id, request }).unwrap();

        (req_id, text)
    };

    let (id, text) = call(proto::Request::OtaUploadBegin { size });
    write.send(Message::Text(text)).await.expect("send");
    let upload_id = match response(&mut read, id).await {
        proto::Response::OtaUploadStarted { upload_id } => upload_id,
        other => panic!("cannot start the upload: {other:?}"),
    };

    let mut chunk = vec![0; UPLOAD_CHUNK_LEN];
    let mut offset = 0u64;
    loop {
        let len = file.read(&mut chunk).expect("read the OTA image file");
        if len == 0 {
            break;
        }

        let (id, text) = call(proto::Request::OtaUploadChunk {
            upload_id,
            offset,
            data: base64::engine::general_purpose::STANDARD.encode(&chunk[..len]),
        });
        write.send(Message::Text(text)).await.expect("send");

        match response(&mut read, id).await {
            proto::Response::Ok => (),
            other => panic!("upload failed: {other:?}"),
        }

        offset += len as u64;
        println!("   uploaded {offset} of {size} bytes");
    }

    let (id, text) = call(proto::Request::OtaUploadEnd { upload_id });
    write.send(Message::Text(text)).await.expect("send");
    println!("<< response: {:#?}", response(&mut read, id).await);
}

/// The response to request `req_id`, skipping events.
async fn response<S>(read: &mut S, req_id: proto::ReqId) -> proto::Response
where
    S: futures_util::Stream<Item = Result<Message, async_tungstenite::tungstenite::Error>> + Unpin,
{
    while let Some(frame) = read.next().await {
        let Ok(Message::Text(text)) = frame else {
            continue;
        };

        if let Ok(proto::ServerMsg::Response {
            req_id: id,
            response,
        }) = serde_json::from_str(&text)
        {
            if id == req_id {
                return response;
            }
        }
    }

    panic!("the connection closed");
}
