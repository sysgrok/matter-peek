//! The store as a whole: our fabrics and their keys, the devices, the
//! assistant's model and its chats. Backed up to a file, encrypted with a
//! passphrase, and restored from one into a controller without devices: a
//! server restarts with it, the app restarts its own controller. In the app
//! running the core itself, also moved to a server without devices: the app
//! hands its store over, puts its own aside, and shows the server from then
//! on.

use futures::{SinkExt, StreamExt};
use gloo_net::websocket::Message;
use gloo_timers::future::TimeoutFuture;
use wasm_bindgen_futures::spawn_local;
use yew::html::Scope;
use yew::prelude::*;

use matter_peek_core::api::wire;

use crate::icons::{icon, Icon};
use crate::transport::{self, Endpoint, Reader, Writer};
use crate::{input_value, picked_file, ui, App, Msg};

/// What a backup starts with, its version, and then 1 if it is encrypted (the
/// server's format).
const MAGIC: &[u8] = b"mpeek";

/// How long a server takes to restart with a store imported, at most.
const RESTART_TRIES: u32 = 20;

/// The popup: backing up and restoring, or moving to a server.
#[derive(Default)]
pub struct BackupView {
    moving: bool,
    passphrase: String,
    again: String,
    exported: Option<Exported>,
    /// The file picked to import.
    picked: Option<Picked>,
    import_passphrase: String,
    server: String,
    token: String,
    busy: bool,
    /// How the move or the import goes.
    status: Option<String>,
    error: Option<String>,
}

/// The store file exported.
struct Exported {
    name: String,
    /// Its contents, base64.
    data: String,
    /// In a browser, the URL to download it from; the app saves it itself.
    url: Option<String>,
}

/// A store file picked to import.
pub struct Picked {
    name: String,
    /// Its contents, base64.
    data: String,
    sealed: bool,
}

/// What the popup does.
pub enum BackupMsg {
    /// `moving`: to move the store to a server, else to back it up or restore
    /// it.
    Open {
        moving: bool,
    },
    Close,
    Passphrase(String),
    Again(String),
    Export,
    /// The file exported, base64.
    Exported(String),
    /// Save the file exported, in the app.
    Save,
    Pick(web_sys::File),
    Picked(Picked),
    ImportPassphrase(String),
    Import,
    Server(String),
    Token(String),
    Move,
    Status(String),
    Failed(String),
}

impl App {
    pub(crate) fn update_backup(&mut self, ctx: &Context<Self>, msg: BackupMsg) -> bool {
        let link = ctx.link().clone();
        if let BackupMsg::Open { moving } = msg {
            self.fabric_menu = false;
            self.drawer_open = false;
            self.backup = Some(BackupView {
                moving,
                ..Default::default()
            });
            return true;
        }
        let Some(view) = self.backup.as_mut() else {
            return false;
        };

        match msg {
            BackupMsg::Open { .. } => {}
            BackupMsg::Close if view.busy => return false,
            BackupMsg::Close => {
                if let Some(url) = view.exported.take().and_then(|exported| exported.url) {
                    let _ = web_sys::Url::revoke_object_url(&url);
                }
                self.backup = None;
            }
            BackupMsg::Passphrase(text) => view.passphrase = text,
            BackupMsg::Again(text) => view.again = text,
            BackupMsg::Export => {
                view.busy = true;
                view.error = None;
                let request = wire::Request::ExportStore {
                    passphrase: Some(view.passphrase.clone()),
                };
                spawn_local(async move {
                    let msg = match call(transport::open(), request).await {
                        Ok(wire::Response::StoreFile { data }) => BackupMsg::Exported(data),
                        Ok(_) => BackupMsg::Failed("an unexpected answer".into()),
                        Err(e) => BackupMsg::Failed(e),
                    };
                    link.send_message(backup(msg));
                });
            }
            BackupMsg::Exported(data) => {
                view.busy = false;
                let url = if transport::in_app() {
                    Ok(None)
                } else {
                    decode(&data).and_then(|file| object_url(&file)).map(Some)
                };
                match url {
                    Ok(url) => {
                        view.exported = Some(Exported {
                            name: file_name(),
                            data,
                            url,
                        })
                    }
                    Err(e) => view.error = Some(e),
                }
            }
            BackupMsg::Save => {
                let Some(exported) = &view.exported else {
                    return false;
                };
                view.error = None;
                let args = transport::args(&[
                    ("name", exported.name.clone().into()),
                    ("data", exported.data.clone().into()),
                ]);
                spawn_local(async move {
                    let msg = match transport::invoke("plugin:files|save", args).await {
                        Ok(saved) => saved_status(&saved).map(BackupMsg::Status),
                        Err(e) => Some(BackupMsg::Failed(format!(
                            "cannot save the file: {}",
                            transport::js_error(e)
                        ))),
                    };
                    if let Some(msg) = msg {
                        link.send_message(backup(msg));
                    }
                });
            }
            BackupMsg::Pick(file) => {
                view.error = None;
                spawn_local(async move {
                    let name = file.name();
                    let msg = match gloo_file::futures::read_as_bytes(&file.into()).await {
                        Ok(bytes) => BackupMsg::Picked(Picked {
                            name,
                            sealed: sealed(&bytes),
                            data: base64(&bytes),
                        }),
                        Err(e) => BackupMsg::Failed(format!("cannot read the file: {e}")),
                    };
                    link.send_message(backup(msg));
                });
            }
            BackupMsg::Picked(picked) => view.picked = Some(picked),
            BackupMsg::ImportPassphrase(text) => view.import_passphrase = text,
            BackupMsg::Import => {
                let Some(picked) = &view.picked else {
                    return false;
                };
                view.busy = true;
                view.error = None;
                view.status = Some("Importing…".into());
                let request = wire::Request::ImportStore {
                    data: picked.data.clone(),
                    passphrase: picked.sealed.then(|| view.import_passphrase.clone()),
                };
                spawn_local(async move {
                    let msg =
                        match call(transport::open(), request).await {
                            Ok(wire::Response::Imported { restarts: true }) => {
                                link.send_message(backup(BackupMsg::Status(
                                    "Imported. The server restarts with it…".into(),
                                )));
                                if restarted(transport::open).await {
                                    reload();
                                    return;
                                }
                                BackupMsg::Failed(
                                "Imported, but the server does not answer since: start it again"
                                    .into(),
                            )
                            }
                            // The app's own controller: restarted with it.
                            Ok(wire::Response::Imported { restarts: false })
                                if transport::in_app() =>
                            {
                                match transport::invoke("restart_core", transport::args(&[])).await {
                                Ok(_) => {
                                    reload();
                                    return;
                                }
                                Err(e) => BackupMsg::Failed(format!(
                                    "Imported, but the controller did not restart: restart the app ({})",
                                    transport::js_error(e)
                                )),
                            }
                            }
                            Ok(wire::Response::Imported { restarts: false }) => BackupMsg::Status(
                                "Imported: matter-peek uses it from its next start.".into(),
                            ),
                            Ok(_) => BackupMsg::Failed("an unexpected answer".into()),
                            Err(e) => BackupMsg::Failed(e),
                        };
                    link.send_message(backup(msg));
                });
            }
            BackupMsg::Server(text) => view.server = text,
            BackupMsg::Token(text) => view.token = text,
            BackupMsg::Move => {
                view.busy = true;
                view.error = None;
                let (server, token) = (view.server.clone(), view.token.clone());
                spawn_local(async move {
                    match move_to(&link, &server, &token).await {
                        Ok(()) => transport::use_server(Some(&server), &token),
                        Err(e) => link.send_message(backup(BackupMsg::Failed(e))),
                    }
                });
            }
            BackupMsg::Status(status) => {
                view.busy = status.ends_with('…');
                view.status = Some(status);
            }
            BackupMsg::Failed(e) => {
                view.busy = false;
                view.status = None;
                view.error = Some(e);
            }
        }

        true
    }

    /// The fabric menu's ways to the popup: backing up and restoring, and in
    /// the app running the core itself, moving to a server.
    pub(crate) fn view_backup_choice(&self, link: &Scope<Self>) -> Html {
        let on_app = transport::endpoint() == Endpoint::App;

        html! {
            <>
                <a class="fabric-choice is-manage"
                    onclick={link.callback(|_| backup(BackupMsg::Open { moving: false }))}>
                    { icon(Icon::Download, 15) }
                    <span class="fabric-choice-name">{ "Back up or restore…" }</span>
                </a>
                if on_app {
                    <a class="fabric-choice is-manage"
                        onclick={link.callback(|_| backup(BackupMsg::Open { moving: true }))}>
                        { icon(Icon::Server, 15) }
                        <span class="fabric-choice-name">{ "Move to a server…" }</span>
                    </a>
                }
            </>
        }
    }

    pub(crate) fn view_backup_popup(&self, link: &Scope<Self>) -> Html {
        let Some(view) = &self.backup else {
            return html! {};
        };
        let close = link.callback(|_| backup(BackupMsg::Close));

        html! {
            <div class="modal is-active">
                <div class="modal-background" onclick={close.clone()}></div>
                <div class="modal-card backup-card">
                    { ui::modal_head("Store",
                        if view.moving { "Move to a server" } else { "Back up or restore" },
                        html! {}, close) }
                    <section class="modal-card-body">
                        if let Some(error) = &view.error {
                            <p class="share-error">{ icon(Icon::Alert, 16) }<span>{ error.clone() }</span></p>
                        }
                        if let Some(status) = &view.status {
                            <p class="share-warning">{ icon(Icon::Info, 16) }<span>{ status.clone() }</span></p>
                        }
                        if view.moving {
                            { view_move(view, link) }
                        } else {
                            { view_export(view, link) }
                            { view_import(view, link) }
                        }
                    </section>
                </div>
            </div>
        }
    }
}

fn view_export(view: &BackupView, link: &Scope<App>) -> Html {
    let ready = !view.passphrase.is_empty() && view.passphrase == view.again;

    html! {
        <div class="backup-section">
            <p class="backup-title">{ "Back up" }</p>
            <p class="backup-text">
                { "A file of our fabrics and their keys, the devices, the assistant's model with its \
                   API key, and its chats, encrypted with a passphrase. Whoever has the file and the \
                   passphrase can control the devices." }
            </p>
            <div class="field">
                <input class="input" type="password" placeholder="A passphrase" autocomplete="new-password"
                    value={view.passphrase.clone()}
                    oninput={link.callback(|e: InputEvent| backup(BackupMsg::Passphrase(input_value(e))))} />
            </div>
            <div class="field">
                <input class="input" type="password" placeholder="The same again" autocomplete="new-password"
                    value={view.again.clone()}
                    oninput={link.callback(|e: InputEvent| backup(BackupMsg::Again(input_value(e))))} />
            </div>
            if !view.again.is_empty() && view.passphrase != view.again {
                <p class="help is-danger">{ "The two passphrases differ." }</p>
            }
            <div class="buttons">
                <button class={classes!("button", "is-link", (view.busy && view.status.is_none()).then_some("is-loading"))}
                    disabled={!ready || view.busy}
                    onclick={link.callback(|_| backup(BackupMsg::Export))}>
                    <span class="icon">{ icon(Icon::Lock, 16) }</span>
                    <span>{ "Export" }</span>
                </button>
                if let Some(exported) = &view.exported {
                    if let Some(url) = &exported.url {
                        <a class="button is-link is-light" download={exported.name.clone()} href={url.clone()}>
                            <span class="icon">{ icon(Icon::Download, 16) }</span>
                            <span>{ format!("Save {}", exported.name) }</span>
                        </a>
                    } else {
                        <button class="button is-link is-light" onclick={link.callback(|_| backup(BackupMsg::Save))}>
                            <span class="icon">{ icon(Icon::Download, 16) }</span>
                            <span>{ format!("Save {}", exported.name) }</span>
                        </button>
                    }
                }
            </div>
        </div>
    }
}

fn view_import(view: &BackupView, link: &Scope<App>) -> Html {
    let ready = view
        .picked
        .as_ref()
        .is_some_and(|picked| !picked.sealed || !view.import_passphrase.is_empty());

    html! {
        <div class="backup-section">
            <p class="backup-title">{ "Restore" }</p>
            <p class="backup-text">
                { "Replaces the store with the one in a file, while there are no devices on our \
                   fabrics; the controller restarts with it. The store the file came from stays as it \
                   is: use one of the two." }
            </p>
            <div class="field">
                <input class="input" type="file"
                    onchange={link.batch_callback(|e: Event| picked_file(e).map(|file| backup(BackupMsg::Pick(file))))} />
            </div>
            if view.picked.as_ref().is_some_and(|picked| picked.sealed) {
                <div class="field">
                    <input class="input" type="password" placeholder="Its passphrase" autocomplete="off"
                        value={view.import_passphrase.clone()}
                        oninput={link.callback(|e: InputEvent| backup(BackupMsg::ImportPassphrase(input_value(e))))} />
                </div>
            }
            <button class={classes!("button", "is-link", (view.busy && view.status.is_some()).then_some("is-loading"))}
                disabled={!ready || view.busy}
                onclick={link.callback(|_| backup(BackupMsg::Import))}>
                <span class="icon">{ icon(Icon::Upload, 16) }</span>
                <span>{ match &view.picked {
                    Some(picked) => format!("Import {}", picked.name),
                    None => "Import".into(),
                } }</span>
            </button>
        </div>
    }
}

fn view_move(view: &BackupView, link: &Scope<App>) -> Html {
    html! {
        <div class="backup-section">
            <p class="backup-text">
                { "Hands our fabrics, the devices, the assistant's model with its API key, and its \
                   chats over to a matter-peek server that has no devices of its own. This app then \
                   puts its own store aside, and shows the server's. On the way they are not \
                   encrypted, as nothing between the app and a server is yet." }
            </p>
            <div class="field">
                <input class="input" type="text" placeholder="192.168.0.10:17335"
                    value={view.server.clone()} spellcheck="false" autocomplete="off"
                    oninput={link.callback(|e: InputEvent| backup(BackupMsg::Server(input_value(e))))} />
            </div>
            <div class="field">
                <input class="input mono" type="password" placeholder="Its access token"
                    value={view.token.clone()} spellcheck="false" autocomplete="off"
                    oninput={link.callback(|e: InputEvent| backup(BackupMsg::Token(input_value(e))))} />
            </div>
            <button class={classes!("button", "is-link", view.busy.then_some("is-loading"))}
                disabled={view.server.trim().is_empty() || view.busy}
                onclick={link.callback(|_| backup(BackupMsg::Move))}>
                <span class="icon">{ icon(Icon::Server, 16) }</span>
                <span>{ "Move" }</span>
            </button>
        </div>
    }
}

/// Move the store of the core in the app to the server at `server`.
async fn move_to(link: &Scope<App>, server: &str, token: &str) -> Result<(), String> {
    let status = |text: &str| link.send_message(backup(BackupMsg::Status(text.into())));

    status("Reading this app's store…");
    let request = wire::Request::ExportStore { passphrase: None };
    let data = match call(transport::open(), request).await? {
        wire::Response::StoreFile { data } => data,
        _ => return Err("an unexpected answer".into()),
    };

    status("Handing it over to the server…");
    let request = wire::Request::ImportStore {
        data,
        passphrase: None,
    };
    let open = || transport::open_server(server, token);
    match call(open(), request).await? {
        wire::Response::Imported { .. } => {}
        _ => return Err("an unexpected answer".into()),
    }

    status("Putting this app's store aside…");
    transport::invoke("retire_core", transport::args(&[]))
        .await
        .map_err(|e| {
            format!(
                "The server has the store, but this app could not put its own aside: {}",
                transport::js_error(e)
            )
        })?;

    status("Waiting for the server to restart…");
    if !restarted(open).await {
        return Err(
            "The server has the store, but does not answer since: start it again, \
                    then choose it with the connection button"
                .into(),
        );
    }

    Ok(())
}

/// Whether the server `open` reaches answers again, once it restarted.
async fn restarted(open: impl Fn() -> Result<(Writer, Reader), String>) -> bool {
    // It restarts a second after its answer.
    TimeoutFuture::new(2_500).await;
    for _ in 0..RESTART_TRIES {
        if call(open(), wire::Request::Fabrics).await.is_ok() {
            return true;
        }
        TimeoutFuture::new(1_000).await;
    }

    false
}

/// Send `request` on connection `ws`, and take its answer.
async fn call(
    ws: Result<(Writer, Reader), String>,
    request: wire::Request,
) -> Result<wire::Response, String> {
    let (mut write, mut read) = ws?;
    let msg = wire::ClientMsg { req_id: 1, request };
    let json = serde_json::to_string(&msg).map_err(|e| e.to_string())?;
    write.send(Message::Text(json)).await?;

    while let Some(frame) = read.next().await {
        let Message::Text(text) = frame? else {
            continue;
        };
        if let Ok(wire::ServerMsg::Response { response, .. }) = serde_json::from_str(&text) {
            return match response {
                wire::Response::Error { message } => Err(message),
                response => Ok(response),
            };
        }
    }

    Err("the connection closed".into())
}

/// Whether `file`, a backup, is encrypted.
fn sealed(file: &[u8]) -> bool {
    file.starts_with(MAGIC) && file.get(MAGIC.len() + 1) == Some(&1)
}

/// A name for the file exported today.
fn file_name() -> String {
    let today = String::from(js_sys::Date::new_0().to_iso_string());

    format!(
        "matter-peek-backup-{}.mpeek",
        today.get(..10).unwrap_or("export")
    )
}

/// What the app's `save` says: where the file went; `None` if the user did not
/// save it.
fn saved_status(saved: &wasm_bindgen::JsValue) -> Option<String> {
    let field = |name: &str| js_sys::Reflect::get(saved, &name.into()).ok();
    if field("saved").and_then(|saved| saved.as_bool()) != Some(true) {
        return None;
    }

    Some(match field("path").and_then(|path| path.as_string()) {
        Some(path) => format!("Saved to {path}."),
        None => "Saved.".into(),
    })
}

/// A URL to save `file` from.
fn object_url(file: &[u8]) -> Result<String, String> {
    let parts = js_sys::Array::of1(&js_sys::Uint8Array::from(file));
    let options = web_sys::BlobPropertyBag::new();
    options.set_type("application/octet-stream");
    let blob = web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &options)
        .map_err(transport::js_error)?;

    web_sys::Url::create_object_url_with_blob(&blob).map_err(transport::js_error)
}

fn decode(data: &str) -> Result<Vec<u8>, String> {
    use base64::Engine as _;

    base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|e| e.to_string())
}

fn base64(data: &[u8]) -> String {
    use base64::Engine as _;

    base64::engine::general_purpose::STANDARD.encode(data)
}

fn reload() {
    if let Some(window) = web_sys::window() {
        let _ = window.location().reload();
    }
}

fn backup(msg: BackupMsg) -> Msg {
    Msg::Backup(msg)
}
