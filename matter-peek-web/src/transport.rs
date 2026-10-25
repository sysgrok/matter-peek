//! The page's connections to the API: a WebSocket to a server or, in the app
//! on the core in-process, Tauri's IPC (`wire_open` / `wire_send` /
//! `wire_close`, answers on a channel). Each carries the wire messages of one
//! client, as a server's WebSocket does.

use std::pin::Pin;

use futures::channel::mpsc;
use futures::{Sink, SinkExt, Stream, StreamExt};
use gloo_net::websocket::{futures::WebSocket, Message};
use js_sys::{Object, Reflect};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;

/// Where the app keeps where its API is: a server's URL, or [`IN_APP`].
const ENDPOINT_KEY: &str = "matter-peek.endpoint";

/// [`ENDPOINT_KEY`]'s value for the core in the app.
const IN_APP: &str = "app";

/// What the page sends on a connection.
pub type Writer = Pin<Box<dyn Sink<Message, Error = String>>>;
/// What the page gets on a connection.
pub type Reader = Pin<Box<dyn Stream<Item = Result<Message, String>>>>;

/// Where the API is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// A server, at its WebSocket URL.
    Server(String),
    /// The core in the app.
    App,
    /// In the app, not chosen yet.
    Unset,
}

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"], catch)]
    async fn invoke(cmd: &str, args: JsValue) -> Result<JsValue, JsValue>;
}

#[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"])]
extern "C" {
    /// Tauri's channel: what the app sends the page on a connection.
    #[wasm_bindgen(js_name = Channel)]
    type IpcChannel;

    #[wasm_bindgen(constructor, js_class = "Channel")]
    fn new() -> IpcChannel;

    #[wasm_bindgen(method, setter = onmessage, js_class = "Channel")]
    fn set_onmessage(this: &IpcChannel, handler: &js_sys::Function);
}

/// Whether the page runs in the app.
pub fn in_app() -> bool {
    web_sys::window()
        .is_some_and(|window| Reflect::has(&window, &JsValue::from_str("__TAURI__")).unwrap_or(false))
}

/// Where the API is: in a browser, the server that served the page; in the
/// app, the one chosen.
pub fn endpoint() -> Endpoint {
    if !in_app() {
        return Endpoint::Server(page_server());
    }

    match storage().and_then(|storage| storage.get_item(ENDPOINT_KEY).ok().flatten()) {
        Some(value) if value == IN_APP => Endpoint::App,
        Some(url) => Endpoint::Server(url),
        None => Endpoint::Unset,
    }
}

/// Have the app use the server at `address` (`host:port`, or a `ws://` URL),
/// or its own core if `None`, and reload the page on it.
pub fn use_server(address: Option<&str>) {
    let value = match address {
        Some(address) => server_url(address),
        None => Some(IN_APP.to_string()),
    };
    if let (Some(storage), Some(value)) = (storage(), value) {
        let _ = storage.set_item(ENDPOINT_KEY, &value);
    }
    if let Some(window) = web_sys::window() {
        let _ = window.location().reload();
    }
}

/// Open a connection to the API.
pub fn open() -> Result<(Writer, Reader), String> {
    match endpoint() {
        Endpoint::Server(url) => {
            let (write, read) = WebSocket::open(&url).map_err(|e| e.to_string())?.split();
            let write = write.sink_map_err(|e| e.to_string());
            let read = read.map(|frame| frame.map_err(|e| e.to_string()));

            Ok((Box::pin(write), Box::pin(read)))
        }
        Endpoint::App => Ok(open_in_app()),
        Endpoint::Unset => Err("choose where matter-peek runs first".into()),
    }
}

/// A connection to the core in the app: open until its writer is dropped.
fn open_in_app() -> (Writer, Reader) {
    let (incoming, read) = mpsc::unbounded::<Result<Message, String>>();
    let (write, mut outgoing) = mpsc::unbounded::<Message>();

    spawn_local(async move {
        let channel = IpcChannel::new();
        let to_page = incoming.clone();
        let onmessage = Closure::<dyn FnMut(JsValue)>::new(move |msg: JsValue| {
            if let Some(text) = msg.as_string() {
                let _ = to_page.unbounded_send(Ok(Message::Text(text)));
            }
        });
        channel.set_onmessage(onmessage.as_ref().unchecked_ref());

        let id = match invoke("wire_open", args(&[("channel", channel.into())])).await {
            Ok(id) => id,
            Err(e) => {
                let _ = incoming.unbounded_send(Err(js_error(e)));
                return;
            }
        };

        while let Some(msg) = outgoing.next().await {
            let Message::Text(text) = msg else {
                continue;
            };
            let sent = invoke("wire_send", args(&[("id", id.clone()), ("msg", text.into())])).await;
            if let Err(e) = sent {
                let _ = incoming.unbounded_send(Err(js_error(e)));
                break;
            }
        }

        let _ = invoke("wire_close", args(&[("id", id)])).await;
        drop(onmessage);
    });

    (Box::pin(write.sink_map_err(|e| e.to_string())), Box::pin(read))
}

/// The WebSocket of the server that served the page, at `/ws` (`trunk serve`
/// passes it on to the server, see `Trunk.toml`).
fn page_server() -> String {
    let location = web_sys::window().map(|window| window.location());
    let host = location
        .as_ref()
        .and_then(|location| location.host().ok())
        .unwrap_or_else(|| "127.0.0.1:8087".into());
    let secure = location
        .and_then(|location| location.protocol().ok())
        .is_some_and(|protocol| protocol == "https:");

    format!("{}://{host}/ws", if secure { "wss" } else { "ws" })
}

/// The WebSocket URL of the server at `address`: `host:port` or a URL.
fn server_url(address: &str) -> Option<String> {
    let address = address.trim().trim_end_matches('/');
    if address.is_empty() {
        return None;
    }

    let url = if address.contains("://") {
        address.to_string()
    } else {
        format!("ws://{address}")
    };
    let path = url.split("://").nth(1).is_some_and(|rest| rest.contains('/'));

    Some(if path { url } else { format!("{url}/ws") })
}

/// An object of `fields`, the arguments of a command.
fn args(fields: &[(&str, JsValue)]) -> JsValue {
    let object = Object::new();
    for (name, value) in fields {
        let _ = Reflect::set(&object, &JsValue::from_str(name), value);
    }

    object.into()
}

fn js_error(e: JsValue) -> String {
    e.as_string().unwrap_or_else(|| format!("{e:?}"))
}

fn storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok()?
}
