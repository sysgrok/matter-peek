//! matter-peek as an app: the web UI in a window. Its API is either a
//! server's, which the page reaches over WebSocket itself, or the core in this
//! process, which the page reaches over IPC: `wire_open` / `wire_send` /
//! `wire_close` carry the [`wire`](matter_peek_core::api::wire) messages of
//! one connection each, its answers and events flowing back on a channel.
//!
//! The core starts with the first connection to it, so an app on a server
//! never starts one.

use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use async_executor::LocalExecutor;
use log::{error, info};
use tauri::ipc::Channel;
use tauri::{Manager, State};

use matter_peek_core::api::wire::{ClientMsg, Response, ServerMsg, WireSession};
use matter_peek_core::api::{MatterPeek, PeekConfig};

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

/// A connection, on the core or, when it could not start, on why not.
enum Conn {
    Session(Rc<WireSession>),
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

/// Run the connections to the core, on this thread, until the app ends.
fn serve(store_dir: PathBuf, cmds: async_channel::Receiver<Cmd>) {
    let ex = LocalExecutor::new();

    futures_lite::future::block_on(ex.run(async {
        let mut peek: Option<Arc<MatterPeek>> = None;
        let mut conns: HashMap<u32, Conn> = HashMap::new();

        while let Ok(cmd) = cmds.recv().await {
            match cmd {
                Cmd::Open { id, channel } => {
                    let started = match &peek {
                        Some(peek) => Ok(peek.clone()),
                        None => start(&store_dir),
                    };
                    let conn = match started {
                        Ok(started) => {
                            peek = Some(started.clone());
                            Conn::Session(Rc::new(session(&ex, started, channel)))
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
                        Some(Conn::Session(session)) => {
                            let session = session.clone();
                            ex.spawn(async move { session.handle(msg).await }).detach();
                        }
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
    }));
}

/// Start the core, keeping its store in `store_dir`.
fn start(store_dir: &std::path::Path) -> Result<Arc<MatterPeek>, String> {
    info!(
        "app: starting the core, its store in {}",
        store_dir.display()
    );

    MatterPeek::start(PeekConfig {
        store_dir: Some(store_dir.display().to_string()),
        periodic_sync: true,
    })
    .map_err(|e| {
        error!("app: the core does not start: {e:?}");
        format!("the controller does not start: {e:?}")
    })
}

/// A connection to `peek`, whose answers and events go to `channel`.
fn session(ex: &LocalExecutor<'_>, peek: Arc<MatterPeek>, channel: Channel<String>) -> WireSession {
    let (out, out_rx) = async_channel::unbounded::<ServerMsg>();

    ex.spawn(async move {
        while let Ok(msg) = out_rx.recv().await {
            if !send(&channel, &msg) {
                break;
            }
        }
    })
    .detach();

    WireSession::new(peek, out)
}

/// Send `msg` to the page; `false` once it is gone.
fn send(channel: &Channel<String>, msg: &ServerMsg) -> bool {
    match serde_json::to_string(msg) {
        Ok(json) => channel.send(json).is_ok(),
        Err(_) => true,
    }
}
