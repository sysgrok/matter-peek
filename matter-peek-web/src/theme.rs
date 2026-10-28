//! Light or dark colours: the system's at first (as the browser, or the app's
//! WebView, tells it), then the ones picked with the switch, which the page
//! keeps in local storage. In the app, the window around the page follows
//! (its title bar; on Android, the colour behind the system bars).

use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;
use yew::html::Scope;
use yew::prelude::*;

use crate::icons::Icon;
use crate::{transport, ui, App, Msg};

/// Where the page keeps the colours picked: `light` or `dark`.
const THEME_KEY: &str = "matter-peek.theme";

/// The colour of the top bar, also behind the system bars and in the
/// browser's chrome.
const LIGHT_BAR: &str = "#ffffff";
const DARK_BAR: &str = "#14161a";

/// The system asks for dark colours.
const SYSTEM_DARK: &str = "(prefers-color-scheme: dark)";

/// Whether the colours are dark: as picked, else as the system has them.
pub fn dark() -> bool {
    picked().unwrap_or_else(system_dark)
}

/// Pick dark colours, or light ones, from now on.
pub fn pick(dark: bool) {
    if let Some(storage) = storage() {
        let _ = storage.set_item(THEME_KEY, if dark { "dark" } else { "light" });
    }
    apply();
}

/// Show the colours [`dark`] says: on the page, and around it.
pub fn apply() {
    let dark = dark();
    let Some(document) = web_sys::window().and_then(|window| window.document()) else {
        return;
    };

    // Without a pick, the page follows the system as it changes.
    if let Some(root) = document.document_element() {
        let _ = match picked() {
            Some(dark) => root.set_attribute("data-theme", if dark { "dark" } else { "light" }),
            None => root.remove_attribute("data-theme"),
        };
    }

    let bar = if dark { DARK_BAR } else { LIGHT_BAR };
    if let Ok(metas) = document.query_selector_all("meta[name=theme-color]") {
        for i in 0..metas.length() {
            if let Some(meta) = metas
                .item(i)
                .and_then(|node| node.dyn_into::<web_sys::Element>().ok())
            {
                let _ = meta.set_attribute("content", bar);
            }
        }
    }

    if transport::in_app() {
        spawn_local(async move {
            let args = transport::args(&[("dark", JsValue::from(dark))]);
            if let Err(e) = transport::invoke("plugin:theme|set_theme", args).await {
                log::debug!(
                    "theme: the window keeps its colours: {}",
                    transport::js_error(e)
                );
            }
        });
    }
}

/// Have `on_change` called when the system's colours change.
pub fn watch_system(on_change: impl Fn() + 'static) {
    let Some(query) =
        web_sys::window().and_then(|window| window.match_media(SYSTEM_DARK).ok().flatten())
    else {
        return;
    };

    let changed = Closure::<dyn Fn()>::new(on_change);
    query.set_onchange(Some(changed.as_ref().unchecked_ref()));
    // For as long as the page lives.
    changed.forget();
}

/// The colours picked with the switch, if any.
fn picked() -> Option<bool> {
    match storage()?.get_item(THEME_KEY).ok().flatten()?.as_str() {
        "dark" => Some(true),
        "light" => Some(false),
        _ => None,
    }
}

fn system_dark() -> bool {
    web_sys::window()
        .and_then(|window| window.match_media(SYSTEM_DARK).ok().flatten())
        .is_some_and(|query| query.matches())
}

fn storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok()?
}

impl App {
    /// The switch between light and dark colours, showing the ones in use;
    /// `class` places it, `compact` leaves out the labels.
    pub(crate) fn view_theme_switch(
        &self,
        class: &'static str,
        compact: bool,
        link: &Scope<Self>,
    ) -> Html {
        html! {
            <div class={classes!("theme-switch", class)}>
                { ui::two_way(
                    (Icon::Sun, "Light", "Light colours"),
                    (Icon::Moon, "Dark", "Dark colours"),
                    Some(self.dark),
                    compact,
                    link.callback(Msg::PickTheme),
                ) }
            </div>
        }
    }
}
