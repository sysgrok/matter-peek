//! The device filter: its box, with completions, and what it shows (see
//! [`query`]).

use yew::html::Scope;
use yew::prelude::*;

use matter_peek_core::api::query::{self, Facts, Known, Query, QueryError};
use matter_peek_core::api::{self, display};

use crate::icons::{icon, Icon};
use crate::{input_value, App, Msg, SubConn};

/// How many completions the box offers at most.
const MAX_SUGGESTIONS: usize = 40;

/// What the filter box does.
pub enum FilterMsg {
    Input(String),
    Focus(bool),
    /// A key pressed in the box while it offers completions.
    Key(String),
    /// Take a completion: the whole filter, completed.
    Pick(String),
    /// Add a term, unless the filter has it.
    Add(String),
    Clear,
}

impl App {
    pub(crate) fn update_filter(&mut self, msg: FilterMsg) -> bool {
        match msg {
            FilterMsg::Input(text) => {
                self.filter = text;
                self.filter_pick = 0;
            }
            FilterMsg::Focus(focus) => {
                self.filter_focus = focus;
                self.filter_pick = 0;
            }
            FilterMsg::Key(key) => {
                let suggestions = self.suggestions();
                match key.as_str() {
                    "ArrowDown" => {
                        self.filter_pick = (self.filter_pick + 1) % suggestions.len().max(1)
                    }
                    "ArrowUp" => {
                        self.filter_pick = self
                            .filter_pick
                            .checked_sub(1)
                            .unwrap_or(suggestions.len().saturating_sub(1))
                    }
                    "Enter" | "Tab" => {
                        if let Some(suggestion) = suggestions.get(self.filter_pick) {
                            self.filter = suggestion.text.clone();
                            self.filter_pick = 0;
                        }
                    }
                    "Escape" => self.filter_focus = false,
                    _ => return false,
                }
            }
            FilterMsg::Pick(text) => {
                self.filter = text;
                self.filter_pick = 0;
            }
            FilterMsg::Add(term) => {
                if !self.filter.split_whitespace().any(|t| t == term) {
                    let filter = self.filter.trim_end();
                    self.filter = if filter.is_empty() {
                        term
                    } else {
                        format!("{filter} {term}")
                    };
                }
            }
            FilterMsg::Clear => self.filter.clear(),
        }

        true
    }

    /// What the filter knows of `device`: its record, and what this page
    /// knows of it now.
    pub(crate) fn device_facts(&self, device: &api::DeviceInfo) -> Facts {
        let node_id = device.handle.node_id;
        let sub = self.subs.get(&node_id);
        let online = match sub.map(|sub| sub.conn) {
            Some(SubConn::Live { .. }) => Some(true),
            Some(SubConn::Offline) => Some(false),
            _ => None,
        };
        let known = Known {
            online,
            health: sub.map(|sub| &sub.health),
            ota: self.ota_progress.get(&node_id),
            images: &self.ota_images,
        };

        Facts::of(device, &known)
    }

    /// The filter, unless it is empty; why not, if it does not read.
    pub(crate) fn filter_query(&self) -> Result<Option<Query>, QueryError> {
        Query::parse(&self.filter).map(|query| (!query.is_empty()).then_some(query))
    }

    /// The devices shown: those the filter matches, all while it does not
    /// read.
    pub(crate) fn visible_devices(&self) -> Vec<&api::DeviceInfo> {
        match self.filter_query() {
            Ok(Some(query)) => self
                .stored
                .iter()
                .filter(|d| query.matches(&self.device_facts(d)))
                .collect(),
            _ => self.stored.iter().collect(),
        }
    }

    /// The box, and its completions while focused.
    pub(crate) fn view_filter_box(&self, link: &Scope<Self>) -> Html {
        let suggestions = if self.filter_focus {
            self.suggestions()
        } else {
            Vec::new()
        };
        let open = !suggestions.is_empty();
        let invalid = self.filter_query().is_err();

        let onkeydown = link.batch_callback(move |e: KeyboardEvent| {
            let key = e.key();
            let ours = matches!(
                key.as_str(),
                "ArrowDown" | "ArrowUp" | "Enter" | "Tab" | "Escape"
            );
            (open && ours).then(|| {
                e.prevent_default();
                filter(FilterMsg::Key(key))
            })
        });

        html! {
            <div class="control has-icons-left search-box">
                <input class={classes!("input", invalid.then_some("is-danger"))} type="search"
                    placeholder="Filter: kitchen, net:thread, is:offline, -eco:google…"
                    value={self.filter.clone()} spellcheck="false" autocomplete="off"
                    oninput={link.callback(|e: InputEvent| filter(FilterMsg::Input(input_value(e))))}
                    onfocus={link.callback(|_| filter(FilterMsg::Focus(true)))}
                    onblur={link.callback(|_| filter(FilterMsg::Focus(false)))}
                    {onkeydown} />
                <span class="icon is-left">{ icon(Icon::Search, 18) }</span>
                if open {
                    <ul class="filter-suggestions" role="listbox">
                        { for suggestions.into_iter().enumerate().map(|(i, s)| {
                            let text = s.text.clone();
                            html! {
                                <li class={classes!("filter-suggestion", (i == self.filter_pick).then_some("is-active"))}
                                    role="option"
                                    onmousedown={link.callback(move |e: MouseEvent| {
                                        // Keep the focus in the box.
                                        e.prevent_default();
                                        filter(FilterMsg::Pick(text.clone()))
                                    })}>
                                    <span class="filter-suggestion-term">{ s.label }</span>
                                    <span class="filter-suggestion-help">{ s.help }</span>
                                </li>
                            }
                        }) }
                    </ul>
                }
            </div>
        }
    }

    /// Why the filter does not read, if it does not.
    pub(crate) fn view_filter_error(&self) -> Html {
        match self.filter_query() {
            Err(e) => html! { <p class="filter-error">{ format!("Not filtered: {e}") }</p> },
            Ok(_) => html! {},
        }
    }

    /// A chip that adds `term` to the filter.
    pub(crate) fn filter_chip(&self, term: String, body: Html, link: &Scope<Self>) -> Html {
        let title = format!("Filter by {term}");

        html! {
            <span class="filter-chip" {title}
                onclick={link.callback(move |_| filter(FilterMsg::Add(term.clone())))}>
                { body }
            </span>
        }
    }

    /// The completions of the filter's last term, from the devices of the
    /// fabric shown.
    fn suggestions(&self) -> Vec<query::Suggestion> {
        let facts: Vec<Facts> = self.stored.iter().map(|d| self.device_facts(d)).collect();

        query::suggest(&self.filter, &facts, MAX_SUGGESTIONS)
    }
}

/// The filter term for `device`'s vendor.
pub(crate) fn vendor_term(device: &api::DeviceInfo) -> Option<String> {
    let vid = device.fingerprint.vendor_id?;
    let name = Some(device.vendor_name.clone())
        .filter(|name| !name.is_empty())
        .or_else(|| display::vendor_name(vid))
        .map(|name| query::slug(&name))
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| display::hex_u16(vid));

    Some(format!("vendor:{name}"))
}

pub(crate) fn filter(msg: FilterMsg) -> Msg {
    Msg::Filter(msg)
}
