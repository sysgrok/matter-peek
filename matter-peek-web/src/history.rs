//! The history of the user's clicks, which the back and forward buttons step
//! through.
//!
//! A step is a page or a popup; a popup's own tabs or a turned card are not.
//! The browser keeps the entries. Each render compares the place shown with the
//! current entry and records a new one if they differ, so no click needs to
//! tell. Going back or forward opens and closes what differs, with the messages
//! a click sends.
//!
//! The browser and Android have back buttons of their own, and the desktop app
//! has none, so it shows these in its top bar.

use serde::{Deserialize, Serialize};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use yew::html::Scope;
use yew::prelude::*;

use crate::assistant::{AssistantMsg, SettingsMsg};
use crate::backup::BackupMsg;
use crate::clock::ClockMsg;
use crate::connection::ConnectionMsg;
use crate::datamodel::ModelMsg;
use crate::fabrics::FabricsMsg;
use crate::health::HealthMsg;
use crate::icons::{icon, Icon};
use crate::memory::MemoryMsg;
use crate::our_fabrics;
use crate::share::ShareMsg;
use crate::transport::{self, Endpoint};
use crate::{App, Msg, Tab};

/// A popup, or something that passes like one.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
enum Popup {
    Logs(u64),
    Network(u64),
    Memory(u64),
    Health(u64),
    Model(u64),
    Share(u64),
    Clock(u64),
    Fabrics(u64),
    Discover,
    Camera,
    Manage,
    Backup { moving: bool },
    Connection,
    AssistantModel,
    // These pass: a click on something else replaces their entry, so that going
    // back never returns to them.
    Ask,
    Drawer,
    FabricMenu,
}

impl Popup {
    fn passes(&self) -> bool {
        matches!(self, Self::Ask | Self::Drawer | Self::FabricMenu)
    }

    /// The device it is of, if any.
    fn device(&self) -> Option<u64> {
        match self {
            Self::Logs(node_id)
            | Self::Network(node_id)
            | Self::Memory(node_id)
            | Self::Health(node_id)
            | Self::Model(node_id)
            | Self::Share(node_id)
            | Self::Clock(node_id)
            | Self::Fabrics(node_id) => Some(*node_id),
            _ => None,
        }
    }

    /// What opens it, unless it passes.
    fn open(&self) -> Option<Msg> {
        Some(match self {
            Self::Logs(node_id) => Msg::OpenLogs(*node_id),
            Self::Network(node_id) => Msg::OpenNetwork(*node_id),
            Self::Memory(node_id) => Msg::Memory(MemoryMsg::Open(*node_id)),
            Self::Health(node_id) => Msg::Health(HealthMsg::Open(*node_id)),
            Self::Model(node_id) => Msg::Model(ModelMsg::Open(*node_id)),
            Self::Share(node_id) => Msg::Share(ShareMsg::Open(*node_id)),
            Self::Clock(node_id) => Msg::Clock(ClockMsg::Open(*node_id)),
            Self::Fabrics(node_id) => Msg::Fabrics(FabricsMsg::Open(*node_id)),
            Self::Discover => Msg::OpenDiscover,
            Self::Camera => Msg::OpenCamera,
            Self::Manage => Msg::OurFabrics(our_fabrics::FabricsMsg::OpenManage),
            Self::Backup { moving } => Msg::Backup(BackupMsg::Open { moving: *moving }),
            Self::Connection => Msg::Connection(ConnectionMsg::Open),
            Self::AssistantModel => Msg::Assistant(AssistantMsg::Settings(SettingsMsg::Open)),
            Self::Ask | Self::Drawer | Self::FabricMenu => return None,
        })
    }

    fn close(&self) -> Msg {
        match self {
            Self::Logs(_) => Msg::CloseLogs,
            Self::Network(_) => Msg::CloseNetwork,
            Self::Memory(_) => Msg::Memory(MemoryMsg::Close),
            Self::Health(_) => Msg::Health(HealthMsg::Close),
            Self::Model(_) => Msg::Model(ModelMsg::Close),
            Self::Share(_) => Msg::Share(ShareMsg::Close),
            Self::Clock(_) => Msg::Clock(ClockMsg::Close),
            Self::Fabrics(_) => Msg::Fabrics(FabricsMsg::Close),
            Self::Discover => Msg::CloseDiscover,
            Self::Camera => Msg::CloseCamera,
            Self::Manage => Msg::OurFabrics(our_fabrics::FabricsMsg::CloseManage),
            Self::Backup { .. } => Msg::Backup(BackupMsg::Close),
            Self::Connection => Msg::Connection(ConnectionMsg::Close),
            Self::AssistantModel => Msg::Assistant(AssistantMsg::Settings(SettingsMsg::Close)),
            Self::Ask => Msg::AskCancelled,
            Self::Drawer => Msg::ToggleDrawer,
            Self::FabricMenu => Msg::OurFabrics(our_fabrics::FabricsMsg::ToggleMenu),
        }
    }
}

/// What is shown.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
struct Place {
    tab: Tab,
    /// On a phone, the Assistant page's list of chats rather than its chat.
    chats: bool,
    popups: Vec<Popup>,
}

impl Place {
    /// Whether `other` is this place with fewer popups.
    fn closes_to(&self, other: &Self) -> bool {
        self.tab == other.tab
            && self.chats == other.chats
            && other.popups.len() < self.popups.len()
            && other.popups.iter().all(|popup| self.popups.contains(popup))
    }
}

/// An entry of the browser's history, kept as JSON in its state: node IDs do not
/// fit in a JavaScript number.
#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    /// How many entries of ours are before it.
    index: u32,
    place: Place,
    /// The place of the entry before it.
    previous: Option<Place>,
}

impl Entry {
    fn current_state() -> Option<Self> {
        let state = web_sys::window()?
            .history()
            .ok()?
            .state()
            .ok()?
            .as_string()?;

        serde_json::from_str(&state).ok()
    }

    fn state(&self) -> Option<JsValue> {
        serde_json::to_string(self)
            .ok()
            .map(|state| JsValue::from_str(&state))
    }

    fn push(&self) {
        if let (Some(history), Some(state)) = (browser_history(), self.state()) {
            let _ = history.push_state(&state, "");
        }
    }

    fn replace(&self) {
        if let (Some(history), Some(state)) = (browser_history(), self.state()) {
            let _ = history.replace_state(&state, "");
        }
    }
}

/// Where the page is in the history.
pub struct History {
    current: Entry,
    /// The index of the last entry that can be gone forward to.
    last: u32,
}

impl Default for History {
    fn default() -> Self {
        Self {
            current: Entry {
                index: 0,
                place: Place {
                    tab: Tab::Devices,
                    chats: false,
                    popups: Vec::new(),
                },
                previous: None,
            },
            last: 0,
        }
    }
}

impl App {
    /// What is shown now. The connection popup that the page cannot do without
    /// stays out: going back should not close it.
    fn place(&self) -> Place {
        let mut popups = Vec::new();
        let mut add = |popup: Option<Popup>| popups.extend(popup);

        add(self.logs.as_ref().map(|view| Popup::Logs(view.node_id)));
        add(self
            .network_view
            .as_ref()
            .map(|view| Popup::Network(view.node_id)));
        add(self.memory.as_ref().map(|view| Popup::Memory(view.node_id)));
        add(self
            .health_view
            .as_ref()
            .map(|view| Popup::Health(view.node_id)));
        add(self
            .model_view
            .as_ref()
            .map(|view| Popup::Model(view.node_id)));
        add(self.share.as_ref().map(|view| Popup::Share(view.node_id)));
        add(self.clock.as_ref().map(|view| Popup::Clock(view.node_id)));
        add(self.fabrics_of.map(Popup::Fabrics));
        add(self.discover_open.then_some(Popup::Discover));
        add(self.camera.is_some().then_some(Popup::Camera));
        add(self.manage.is_some().then_some(Popup::Manage));
        add(self.backup.as_ref().map(|view| Popup::Backup {
            moving: view.moving,
        }));
        add((self.connection.is_some()
            && !self.refused
            && transport::endpoint() != Endpoint::Unset)
            .then_some(Popup::Connection));
        add(self
            .assistant
            .settings
            .is_some()
            .then_some(Popup::AssistantModel));
        add(self.ask.is_some().then_some(Popup::Ask));
        add(self.drawer_open.then_some(Popup::Drawer));
        add(self.fabric_menu.then_some(Popup::FabricMenu));

        Place {
            tab: self.tab,
            chats: self.tab == Tab::Assistant && self.assistant.show_list,
            popups,
        }
    }

    /// Take over the entry the page loaded on (after a reload, one of ours),
    /// and follow the back and forward buttons.
    pub(crate) fn start_history(&mut self, link: &Scope<Self>) {
        let (index, previous) =
            Entry::current_state().map_or((0, None), |entry| (entry.index, entry.previous));

        self.history.current = Entry {
            index,
            place: self.place(),
            previous,
        };
        self.history.last = index;
        self.history.current.replace();

        let Some(window) = web_sys::window() else {
            return;
        };
        let link = link.clone();
        let moved = Closure::<dyn Fn()>::new(move || link.send_message(Msg::HistoryMoved));
        let _ = window.add_event_listener_with_callback("popstate", moved.as_ref().unchecked_ref());
        // For as long as the page lives.
        moved.forget();
    }

    /// Record what the last clicks changed.
    pub(crate) fn record_history(&mut self, link: &Scope<Self>) {
        let place = self.place();
        let current = &mut self.history.current;

        if place == current.place {
            return;
        }

        if current.place.closes_to(&place) && current.previous.as_ref() == Some(&place) {
            // Closing what the last click opened is going back: going forward
            // opens it again. Going back brings the entry's `previous`.
            *current = Entry {
                index: current.index.saturating_sub(1),
                place,
                previous: None,
            };
            if let Some(history) = browser_history() {
                let _ = history.back();
            }
        } else if current.place.popups.iter().any(Popup::passes) {
            current.place = place;
            current.replace();
        } else {
            *current = Entry {
                index: current.index + 1,
                previous: Some(current.place.clone()),
                place,
            };
            current.push();
            self.history.last = self.history.current.index;
        }

        // The buttons were drawn before this.
        if transport::in_desktop_app() {
            link.send_message(Msg::HistoryRecorded);
        }
    }

    /// The back or forward button took the page to another entry: show it.
    pub(crate) fn history_moved(&mut self, ctx: &Context<Self>) -> bool {
        let Some(mut entry) = Entry::current_state() else {
            return false;
        };

        let told = entry.place.popups.len();
        if entry.place.popups.iter().any(Popup::passes) {
            // Only the last entry can be of something that passes, and that
            // cannot be opened again: stay.
            if entry.index > self.history.current.index {
                if let Some(history) = browser_history() {
                    let _ = history.back();
                }
                return false;
            }
            entry.place.popups.retain(|popup| !popup.passes());
        }
        // A device may have gone since.
        entry.place.popups.retain(|popup| {
            popup.device().is_none_or(|node_id| {
                self.all_devices
                    .iter()
                    .any(|device| device.handle.node_id == node_id)
            })
        });
        if entry.place.popups.len() != told {
            entry.replace();
        }

        let shown = self.place();
        for popup in &shown.popups {
            if !entry.place.popups.contains(popup) {
                <Self as Component>::update(self, ctx, popup.close());
            }
        }
        if entry.place.tab != shown.tab {
            <Self as Component>::update(self, ctx, Msg::SelectTab(entry.place.tab));
        }
        if entry.place.tab == Tab::Assistant {
            self.assistant.show_list = entry.place.chats;
        }
        for popup in &entry.place.popups {
            if !shown.popups.contains(popup) {
                if let Some(open) = popup.open() {
                    <Self as Component>::update(self, ctx, open);
                }
            }
        }

        self.history.last = self.history.last.max(entry.index);
        self.history.current = entry;

        true
    }

    /// The back and forward buttons of the desktop app.
    pub(crate) fn view_history_buttons(&self) -> Html {
        if !transport::in_desktop_app() {
            return html! {};
        }

        let index = self.history.current.index;
        let back = Callback::from(|_: MouseEvent| {
            if let Some(history) = browser_history() {
                let _ = history.back();
            }
        });
        let forward = Callback::from(|_: MouseEvent| {
            if let Some(history) = browser_history() {
                let _ = history.forward();
            }
        });

        html! {
            <div class="appbar-history">
                <button class="icon-action" title="Back" aria-label="Back"
                    disabled={index == 0} onclick={back}>
                    { icon(Icon::ArrowLeft, 18) }
                </button>
                <button class="icon-action" title="Forward" aria-label="Forward"
                    disabled={index >= self.history.last} onclick={forward}>
                    { icon(Icon::ArrowRight, 18) }
                </button>
            </div>
        }
    }
}

fn browser_history() -> Option<web_sys::History> {
    web_sys::window()?.history().ok()
}
