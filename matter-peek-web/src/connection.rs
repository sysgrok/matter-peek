//! Where the page's API is, the core in the app or a server, and the popup to
//! change it; and the server's access token, which the popup asks for when the
//! server refuses the one kept (in a browser, that is all it asks).

use yew::html::Scope;
use yew::prelude::*;

use crate::icons::{icon, Icon};
use crate::transport::{self, Endpoint};
use crate::{input_value, ui, App, Msg};

/// What the connection popup does.
pub enum ConnectionMsg {
    Open,
    Input(String),
    Token(String),
    /// Use the server typed in, with the token typed in.
    UseServer,
    /// Use the token typed in, with the server the page came from.
    UseToken,
    /// Use the core in the app.
    UseApp,
    /// The server refused the access token: ask for it.
    Refused,
    Close,
}

impl App {
    pub(crate) fn update_connection(&mut self, msg: ConnectionMsg) -> bool {
        match msg {
            ConnectionMsg::Open => {
                self.connection = Some(match transport::endpoint() {
                    Endpoint::Server(url) if transport::in_app() => url,
                    _ => String::new(),
                });
                self.connection_token = transport::token().unwrap_or_default();
            }
            ConnectionMsg::Input(text) => self.connection = Some(text),
            ConnectionMsg::Token(text) => self.connection_token = text,
            ConnectionMsg::UseServer => {
                if let Some(address) = self.connection.take() {
                    transport::use_server(Some(&address), &self.connection_token);
                }
            }
            ConnectionMsg::UseToken => transport::set_token(self.connection_token.trim()),
            ConnectionMsg::UseApp => {
                self.connection = None;
                transport::use_server(None, "");
            }
            ConnectionMsg::Refused => {
                if self.refused {
                    return false;
                }
                self.refused = true;
                self.update_connection(ConnectionMsg::Open);
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

        if !transport::in_app() {
            return self.view_token_popup(link, close);
        }

        let on_app = endpoint == Endpoint::App;
        let title = if endpoint == Endpoint::Unset {
            "Where should matter-peek run?"
        } else {
            "Where matter-peek runs"
        };

        html! {
            <div class="modal is-active">
                <div class="modal-background" onclick={close.clone()}></div>
                <div class={classes!("modal-card", "connection-card", (endpoint == Endpoint::Unset).then_some("is-required"))}>
                    { ui::modal_head("Connection", title, html! {}, close) }
                    <section class="modal-card-body">
                        <p class="connection-text">
                            { "On this device, its own controller keeps its own fabrics and devices. \
                               On a server, this app shows that server's." }
                        </p>
                        if self.refused {
                            <p class="help is-danger connection-warning">
                                { "The server refused the access token: give the one it shows." }
                            </p>
                        }
                        <div class="field">
                            <input class="input" type="text" placeholder="192.168.10.9:17335"
                                value={address.clone()} spellcheck="false" autocomplete="off"
                                oninput={link.callback(|e: InputEvent| connection(ConnectionMsg::Input(input_value(e))))} />
                        </div>
                        <div class="field has-addons">
                            <div class="control is-expanded">
                                <input class="input mono" type="password" placeholder="Its access token"
                                    value={self.connection_token.clone()} spellcheck="false" autocomplete="off"
                                    oninput={link.callback(|e: InputEvent| connection(ConnectionMsg::Token(input_value(e))))} />
                            </div>
                            <div class="control">
                                <button class="button is-link" disabled={address.trim().is_empty()}
                                    onclick={link.callback(|_| connection(ConnectionMsg::UseServer))}>
                                    { "Use this server" }
                                </button>
                            </div>
                        </div>
                        <p class="help connection-warning">
                            { "The server shows its address with the access token when it starts, and \
                               `matter-peek token` prints the token on its machine: paste either. \
                               There is no TLS yet: use a server on a network you trust, or reach it \
                               through a VPN or an SSH tunnel." }
                        </p>
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

    /// In a browser: the server refused the access token, so ask for it.
    fn view_token_popup(&self, link: &Scope<Self>, close: Callback<MouseEvent>) -> Html {
        let token = self.connection_token.trim();

        html! {
            <div class="modal is-active">
                <div class="modal-background" onclick={close.clone()}></div>
                <div class="modal-card connection-card">
                    { ui::modal_head("Connection", "The server's access token", html! {}, close) }
                    <section class="modal-card-body">
                        <p class="connection-text">
                            { "The server wants its access token. It shows its address with the token \
                               when it starts, and `matter-peek token` prints the token on its machine." }
                        </p>
                        <div class="field has-addons">
                            <div class="control is-expanded">
                                <input class="input mono" type="password" placeholder="Its access token"
                                    value={self.connection_token.clone()} spellcheck="false" autocomplete="off"
                                    oninput={link.callback(|e: InputEvent| connection(ConnectionMsg::Token(input_value(e))))} />
                            </div>
                            <div class="control">
                                <button class="button is-link" disabled={token.is_empty()}
                                    onclick={link.callback(|_| connection(ConnectionMsg::UseToken))}>
                                    { "Use it" }
                                </button>
                            </div>
                        </div>
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
