//! matter-peek as an app: the web UI in a window. Its API is either a
//! server's, which the page reaches over WebSocket itself, or the core in this
//! process, which the page reaches over IPC: `wire_open` / `wire_send` /
//! `wire_close` carry the [`wire`](matter_peek_core::api::wire) messages of
//! one connection each, its answers and events flowing back on a channel.
//!
//! The core, with its assistant, is a [`Hub`] of the server's: it starts with
//! the first connection to it, so an app on a server never starts one. The
//! page has it stop, to start again with a store imported (`restart_core`), or
//! for good once its store moved to a server (`retire_core`, which puts the
//! store aside). The page is the server's copy of the web UI, at
//! `matter-peek://localhost/`.
//!
//! On Android, the page also reaches Google Play services' Matter
//! commissioning: the `play` plugin's commands are answered by `PlayPlugin`
//! (Kotlin), in `gen/android`. The `theme` plugin gives the window the page's
//! colours, light or dark; the `files` plugin saves the files the page's
//! downloads would; Tauri's `opener` opens the project's page in the browser.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use log::{error, info};
use tauri::ipc::Channel;
use tauri::plugin::TauriPlugin;
use tauri::webview::{PermissionKind, PermissionResponse};
use tauri::{Manager, Runtime, State};

use matter_peek_core::api::platform::store_in_use;
use matter_peek_core::api::wire::{ClientMsg, Response, ServerMsg};
use matter_peek_core::api::PeekConfig;
use matter_peek_server::{Hub, Session};

/// How long the core may take to stop.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// The connections of the page to the core in this process.
struct Wire {
    cmds: async_channel::Sender<Cmd>,
    next_id: AtomicU32,
}

/// What the page asks of a connection.
enum Cmd {
    Open {
        id: u32,
        channel: Channel<String>,
    },
    Send {
        id: u32,
        msg: String,
    },
    Close {
        id: u32,
    },
    /// End every connection, and with them the core; `retire`: put its store
    /// aside.
    Stop {
        retire: bool,
        done: async_channel::Sender<Result<(), String>>,
    },
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

/// Stop the core; the next connection starts it again, with a store imported.
#[tauri::command]
async fn restart_core(wire: State<'_, Wire>) -> Result<(), String> {
    stop(&wire, false).await
}

/// Stop the core, and put its store aside: it moved to a server.
#[tauri::command]
async fn retire_core(wire: State<'_, Wire>) -> Result<(), String> {
    stop(&wire, true).await
}

async fn stop(wire: &Wire, retire: bool) -> Result<(), String> {
    let (done, done_rx) = async_channel::bounded(1);
    wire.cmds
        .send(Cmd::Stop { retire, done })
        .await
        .map_err(|_| "the app's connections ended".to_string())?;

    done_rx
        .recv()
        .await
        .map_err(|_| "the app's connections ended".to_string())?
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
        .plugin(play())
        .plugin(theme())
        .plugin(files())
        .plugin(tauri_plugin_opener::init())
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
        .invoke_handler(tauri::generate_handler![
            wire_open,
            wire_send,
            wire_close,
            restart_core,
            retire_core
        ])
        .run(tauri::generate_context!())
        .expect("the app failed to run");
}

/// Google Play services' Matter commissioning, for the page: `PlayPlugin` on
/// Android, nothing elsewhere (the page then finds no commands).
fn play<R: Runtime>() -> TauriPlugin<R> {
    tauri::plugin::Builder::new("play")
        .setup(|_app, _api| {
            #[cfg(target_os = "android")]
            _api.register_android_plugin("com.sysgrok.matterpeek", "PlayPlugin")?;

            Ok(())
        })
        .build()
}

/// The page's colours, light or dark, for the window around it: on the
/// desktop, its title bar (Tauri's window theme); on Android, `ThemePlugin`
/// colours the system bars.
fn theme<R: Runtime>() -> TauriPlugin<R> {
    let builder = tauri::plugin::Builder::new("theme").setup(|_app, _api| {
        #[cfg(target_os = "android")]
        _api.register_android_plugin("com.sysgrok.matterpeek", "ThemePlugin")?;

        Ok(())
    });

    #[cfg(desktop)]
    let builder = builder.invoke_handler(tauri::generate_handler![set_theme]);

    builder.build()
}

/// Give the window dark colours, or light ones. Async, so that it runs off the
/// main thread: on Windows, changing the theme from within WebView2's message
/// handler, where a sync command runs, hangs the window.
#[cfg(desktop)]
#[tauri::command]
async fn set_theme<R: Runtime>(window: tauri::Window<R>, dark: bool) -> Result<(), String> {
    let theme = if dark {
        tauri::Theme::Dark
    } else {
        tauri::Theme::Light
    };

    window.set_theme(Some(theme)).map_err(|e| e.to_string())
}

/// Files for the user to keep, which the page's downloads do not save in the
/// app: on the desktop, in the Downloads directory; on Android, `FilesPlugin`
/// asks where.
fn files<R: Runtime>() -> TauriPlugin<R> {
    let builder = tauri::plugin::Builder::new("files").setup(|_app, _api| {
        #[cfg(target_os = "android")]
        _api.register_android_plugin("com.sysgrok.matterpeek", "FilesPlugin")?;

        Ok(())
    });

    #[cfg(desktop)]
    let builder = builder.invoke_handler(tauri::generate_handler![save]);

    builder.build()
}

/// Save file `name`, its contents `data` (base64), in the Downloads
/// directory, under another name if one has it; where it went.
#[cfg(desktop)]
#[tauri::command]
fn save<R: Runtime>(
    app: tauri::AppHandle<R>,
    name: String,
    data: String,
) -> Result<serde_json::Value, String> {
    use base64::Engine as _;
    use std::io::Write as _;

    let data = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|e| e.to_string())?;
    let name = Path::new(&name)
        .file_name()
        .ok_or("not a file name")?
        .to_string_lossy()
        .into_owned();
    let dir = app.path().download_dir().map_err(|e| e.to_string())?;

    let (stem, extension) = name.rsplit_once('.').unwrap_or((&name, ""));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    // It may hold keys.
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    for n in 1..100 {
        let file = match n {
            1 => name.clone(),
            n if extension.is_empty() => format!("{stem} ({n})"),
            n => format!("{stem} ({n}).{extension}"),
        };
        let path = dir.join(file);
        match options.open(&path) {
            Ok(mut out) => {
                out.write_all(&data).map_err(|e| e.to_string())?;
                info!("app: saved {}", path.display());
                return Ok(serde_json::json!({ "saved": true, "path": path }));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("cannot save to {}: {e}", dir.display())),
        }
    }

    Err(format!("{} has too many files named {name}", dir.display()))
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
                Cmd::Stop { retire, done } => {
                    conns.clear();
                    hub = None;
                    let _ = done.send(stop_core(&store_dir, retire).await).await;
                }
            }
        }
    });
}

/// Wait for the core on the store in `store_dir` to stop, its connections
/// gone, and put the store aside if `retire`.
async fn stop_core(store_dir: &Path, retire: bool) -> Result<(), String> {
    let started = std::time::Instant::now();
    while store_in_use(store_dir) {
        if started.elapsed() > STOP_TIMEOUT {
            error!("app: the core does not stop");
            return Err("the controller does not stop: restart the app".into());
        }
        async_io::Timer::after(Duration::from_millis(200)).await;
    }
    info!("app: the core stopped");

    if retire {
        let retired = matter_peek_server::retire_store(store_dir).map_err(|e| {
            error!("app: cannot put the store aside: {e}");
            format!("cannot put the store aside: {e}")
        })?;
        info!(
            "app: the store moved to a server; put aside in {}",
            retired.display()
        );
    }

    Ok(())
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
    Hub::start(config, None, false).map_err(|e| {
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
