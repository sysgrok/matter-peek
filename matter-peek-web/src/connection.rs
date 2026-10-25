//! In the app: where the page's API is, the core in the app or a server, and
//! the popup to change it.

use yew::html::Scope;
use yew::prelude::*;

use crate::icons::{icon, Icon};
use crate::transport::{self, Endpoint};
use crate::{input_value, ui, App, Msg};

/// What the connection popup does.
pub enum ConnectionMsg {
    Open,
    Input(String),
    /// Use the server typed in.
    UseServer,
    /// Use the core in the app.
    UseApp,
    Close,
}

impl App {
    pub(crate) fn update_connection(&mut self, msg: ConnectionMsg) -> bool {
        match msg {
            ConnectionMsg::Open => {
                self.connection = Some(match transport::endpoint() {
                    Endpoint::Server(url) => url,
                    Endpoint::App | Endpoint::Unset => String::new(),
                });
            }
            ConnectionMsg::Input(text) => self.connection = Some(text),
            ConnectionMsg::UseServer => {
                if let Some(address) = self.connection.take() {
                    transport::use_server(Some(&address));
                }
            }
            ConnectionMsg::UseApp => {
                self.connection = None;
                transport::use_server(None);
            }
            // It is chosen first.
            ConnectionMsg::Close if transport::endpoint() == Endpoint::Unset => return false,
            ConnectionMsg::Close => self.connection = None,
        }

        true
    }

    /// In the app, what it is on, opening the popup; `class` places it.
    pub(crate) fn view_connection_button(&self, class: &'static str, link: &Scope<Self>) -> Html {
        if !transport::in_app() {
            return html! {};
        }

        let (glyph, label) = match transport::endpoint() {
            Endpoint::App => (Icon::Chip, "This device".to_string()),
            Endpoint::Server(url) => (Icon::Globe, server_host(&url)),
            Endpoint::Unset => (Icon::Globe, "Not connected".to_string()),
        };

        html! {
            <button class={classes!("connection-button", class)}
                title="Where matter-peek runs: on this device, or on a server"
                onclick={link.callback(|_| connection(ConnectionMsg::Open))}>
                { icon(glyph, 15) }
                <span>{ label }</span>
            </button>
        }
    }

    pub(crate) fn view_connection_popup(&self, link: &Scope<Self>) -> Html {
        let Some(address) = &self.connection else {
            return html! {};
        };
        let close = link.callback(|_| connection(ConnectionMsg::Close));
        let endpoint = transport::endpoint();
        let on_app = endpoint == Endpoint::App;
        let title = if endpoint == Endpoint::Unset {
            "Where should matter-peek run?"
        } else {
            "Where matter-peek runs"
        };

        html! {
            <div class="modal is-active">
                <div class="modal-background" onclick={close.clone()}></div>
                <div class="modal-card connection-card">
                    { ui::modal_head("Connection", title, html! {}, close) }
                    <section class="modal-card-body">
                        <p class="connection-text">
                            { "On this device, its own controller keeps its own fabrics and devices. \
                               On a server, this app shows that server's." }
                        </p>
                        <div class="field has-addons">
                            <div class="control is-expanded">
                                <input class="input" type="text" placeholder="192.168.10.9:8087"
                                    value={address.clone()} spellcheck="false" autocomplete="off"
                                    oninput={link.callback(|e: InputEvent| connection(ConnectionMsg::Input(input_value(e))))} />
                            </div>
                            <div class="control">
                                <button class="button is-link" disabled={address.trim().is_empty()}
                                    onclick={link.callback(|_| connection(ConnectionMsg::UseServer))}>
                                    { "Use this server" }
                                </button>
                            </div>
                        </div>
                        <button class={classes!("button", "is-soft", on_app.then_some("is-active"))}
                            onclick={link.callback(|_| connection(ConnectionMsg::UseApp))}>
                            <span class="icon">{ icon(Icon::Chip, 16) }</span>
                            <span>{ "Run on this device" }</span>
                        </button>
                    </section>
                </div>
            </div>
        }
    }
}

/// The host and port of WebSocket URL `url`.
fn server_host(url: &str) -> String {
    let rest = url.split("://").nth(1).unwrap_or(url);

    rest.split('/').next().unwrap_or(rest).to_string()
}

fn connection(msg: ConnectionMsg) -> Msg {
    Msg::Connection(msg)
}
