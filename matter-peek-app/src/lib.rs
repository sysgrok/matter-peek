//! matter-peek as an app: the web UI in a window. Its API is either a
//! server's, which the page reaches over WebSocket itself, or the core in this
//! process, which the page reaches over IPC: `wire_open` / `wire_send` /
//! `wire_close` carry the [`wire`](matter_peek_core::api::wire) messages of
//! one connection each, its answers and events flowing back on a channel.
//!
//! The core, with its assistant, is a [`Hub`] of the server's: it starts with
//! the first connection to it, so an app on a server never starts one. The
//! page is the server's copy of the web UI, at `matter-peek://localhost/`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};

use log::{error, info};
use tauri::ipc::Channel;
use tauri::webview::{PermissionKind, PermissionResponse};
use tauri::{Manager, State};

use matter_peek_core::api::wire::{ClientMsg, Response, ServerMsg};
use matter_peek_core::api::PeekConfig;
use matter_peek_server::{Hub, Session};

/// The connections of the page to the core in this process.
struct Wire {
    cmds: async_channel::Sender<Cmd>,
    next_id: AtomicU32,
}

/// What the page asks of a connection.
enum Cmd {
    Open { id: u32, channel: Channel<String> },
    Send { id: u32, msg: String },
    Close { id: u32 },
}

/// A connection, to the core or, when it could not start, to why not.
enum Conn {
    Session(Session),
    Failed(Channel<String>, String),
}

/// Open a connection; its answers and events go to `channel`, as JSON.
#[tauri::command]
fn wire_open(wire: State<'_, Wire>, channel: Channel<String>) -> u32 {
    let id = wire.next_id.fetch_add(1, Ordering::Relaxed);
    let _ = wire.cmds.try_send(Cmd::Open { id, channel });

    id
}

/// Send a client message (JSON) on connection `id`.
#[tauri::command]
fn wire_send(wire: State<'_, Wire>, id: u32, msg: String) {
    let _ = wire.cmds.try_send(Cmd::Send { id, msg });
}

/// Close connection `id`: its subscriptions and uploads end.
#[tauri::command]
fn wire_close(wire: State<'_, Wire>, id: u32) {
    let _ = wire.cmds.try_send(Cmd::Close { id });
}

/// Run the app.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(
            tauri_plugin_log::Builder::default()
                .level(log::LevelFilter::Info)
                .build(),
        )
        // The page reads QR codes with the camera, when asked to; it wants
        // nothing else.
        .on_permission_request(|_, kind| match kind {
            PermissionKind::Camera => PermissionResponse::Allow,
            _ => PermissionResponse::Default,
        })
        .register_uri_scheme_protocol("matter-peek", |_, request| web(request.uri().path()))
        .setup(|app| {
            let store_dir = app.path().app_data_dir()?;
            let (cmds, cmds_rx) = async_channel::unbounded();

            std::thread::Builder::new()
                .name("wire".into())
                .spawn(move || serve(store_dir, cmds_rx))?;
            app.manage(Wire {
                cmds,
                next_id: AtomicU32::new(1),
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![wire_open, wire_send, wire_close])
        .run(tauri::generate_context!())
        .expect("the app failed to run");
}

/// The web UI's file at `path`, from the server's copy: the window's page is
/// at `matter-peek://localhost/`.
fn web(path: &str) -> tauri::http::Response<Vec<u8>> {
    let response = tauri::http::Response::builder();

    // The page names its files by their contents' hash; it itself must not be
    // kept, or an updated app shows the old one.
    let page = !path.trim_start_matches('/').contains('.') || path.ends_with("index.html");
    let cache = if page {
        "no-cache"
    } else {
        "public, max-age=86400"
    };

    match matter_peek_server::web_file(path) {
        Some((content_type, body)) => response
            .header(tauri::http::header::CONTENT_TYPE, content_type)
            .header(tauri::http::header::CACHE_CONTROL, cache)
            .body(body),
        None => response.status(404).body(b"Not found".to_vec()),
    }
    .expect("a response of known parts")
}

/// Run the connections to the core, on this thread, until the app ends.
fn serve(store_dir: PathBuf, cmds: async_channel::Receiver<Cmd>) {
    matter_peek_server::block_on(async {
        let mut hub: Option<Rc<Hub>> = None;
        let mut conns: HashMap<u32, Conn> = HashMap::new();

        while let Ok(cmd) = cmds.recv().await {
            match cmd {
                Cmd::Open { id, channel } => {
                    let started = match &hub {
                        Some(hub) => Ok(hub.clone()),
                        None => start(&store_dir),
                    };
                    let conn = match started {
                        Ok(started) => {
                            hub = Some(started.clone());
                            Conn::Session(connect(&started, channel))
                        }
                        Err(e) => Conn::Failed(channel, e),
                    };
                    conns.insert(id, conn);
                }
                Cmd::Send { id, msg } => {
                    let Ok(msg) = serde_json::from_str::<ClientMsg>(&msg) else {
                        continue;
                    };
                    match conns.get(&id) {
                        Some(Conn::Session(session)) => session.handle(msg),
                        Some(Conn::Failed(channel, message)) => {
                            let response = ServerMsg::Response {
                                req_id: msg.req_id,
                                response: Response::Error {
                                    message: message.clone(),
                                },
                            };
                            send(channel, &response);
                        }
                        None => {}
                    }
                }
                Cmd::Close { id } => {
                    conns.remove(&id);
                }
            }
        }
    });
}

/// Start the core and its assistant, keeping the store and the chats in
/// `store_dir`.
fn start(store_dir: &Path) -> Result<Rc<Hub>, String> {
    info!(
        "app: starting the core, its store in {}",
        store_dir.display()
    );

    let config = PeekConfig {
        store_dir: Some(store_dir.display().to_string()),
        periodic_sync: true,
    };
    Hub::start(config, None, store_dir.join("chats")).map_err(|e| {
        error!("app: the core does not start: {e}");
        format!("the controller does not start: {e}")
    })
}

/// A connection to `hub`, whose answers and events go to `channel`.
fn connect(hub: &Hub, channel: Channel<String>) -> Session {
    let (out, out_rx) = async_channel::unbounded::<ServerMsg>();

    matter_peek_server::spawn(async move {
        while let Ok(msg) = out_rx.recv().await {
            if !send(&channel, &msg) {
                break;
            }
        }
    });

    hub.connect(out)
}

/// Send `msg` to the page; `false` once it is gone.
fn send(channel: &Channel<String>, msg: &ServerMsg) -> bool {
    match serde_json::to_string(msg) {
        Ok(json) => channel.send(json).is_ok(),
        Err(_) => true,
    }
}
