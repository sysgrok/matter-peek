//! Installing the web UI as an app of its own, where the browser offers it
//! (Chrome and Edge, on localhost or over HTTPS): the browser's
//! `beforeinstallprompt` event, kept for a button of ours. Elsewhere, such as
//! Safari's "Add to Home Screen", the browser's own menu does it.

use std::cell::RefCell;

use js_sys::{Function, Reflect};
use wasm_bindgen::prelude::*;
use yew::html::Scope;
use yew::prelude::*;

use crate::icons::{icon, Icon};
use crate::{transport, App, Msg};

thread_local! {
    /// The browser's offer to install the page, while it stands.
    static OFFER: RefCell<Option<JsValue>> = const { RefCell::new(None) };
}

/// From now on, `on_offer(true)` when the browser offers to install the page,
/// and `on_offer(false)` once it is installed. Not in the app.
pub fn watch(on_offer: impl Fn(bool) + 'static) {
    let Some(window) = web_sys::window().filter(|_| !transport::in_app()) else {
        return;
    };

    let on_offer = std::rc::Rc::new(on_offer);
    let offered = on_offer.clone();
    let offer = Closure::<dyn Fn(JsValue)>::new(move |event: JsValue| {
        // Our button asks, instead of the browser's own banner.
        event.unchecked_ref::<web_sys::Event>().prevent_default();
        OFFER.with(|offer| *offer.borrow_mut() = Some(event));
        offered(true);
    });
    let installed = Closure::<dyn Fn(JsValue)>::new(move |_: JsValue| {
        OFFER.with(|offer| *offer.borrow_mut() = None);
        on_offer(false);
    });

    let _ = window
        .add_event_listener_with_callback("beforeinstallprompt", offer.as_ref().unchecked_ref());
    let _ =
        window.add_event_listener_with_callback("appinstalled", installed.as_ref().unchecked_ref());
    // For as long as the page lives.
    offer.forget();
    installed.forget();
}

/// Ask the browser to install the page, as it offered.
pub fn install() {
    let Some(offer) = OFFER.with(|offer| offer.borrow_mut().take()) else {
        return;
    };

    if let Ok(prompt) = Reflect::get(&offer, &JsValue::from_str("prompt")) {
        if let Ok(prompt) = prompt.dyn_into::<Function>() {
            let _ = prompt.call0(&offer);
        }
    }
}

impl App {
    /// Install the page as an app, while the browser offers it; `class`
    /// places it.
    pub(crate) fn view_install_button(&self, class: &'static str, link: &Scope<Self>) -> Html {
        if !self.installable {
            return html! {};
        }

        html! {
            <button class={classes!("connection-button", class)}
                title="Install matter-peek as an app of its own, in its own window"
                onclick={link.callback(|_| Msg::Install)}>
                { icon(Icon::Download, 15) }
                <span>{ "Install" }</span>
            </button>
        }
    }
}
