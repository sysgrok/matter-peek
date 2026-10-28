//! A device's health, in a popup: its last check, checked again on opening,
//! area by area.

use yew::html::Scope;
use yew::prelude::*;

use matter_peek_core::api::{
    wire, HealthArea, HealthAreaKind, HealthCheck, HealthVerdict, NetworkKind,
};

use crate::icons::{icon, kind_icon, Icon};
use crate::{request_loop, ui, App, Msg};

/// The health popup of one device.
pub struct HealthView {
    pub node_id: u64,
    pub loading: bool,
    /// As last checked: from the device list until the check answers.
    pub check: Option<HealthCheck>,
    pub error: Option<String>,
}

/// What happens in the health popup.
pub enum HealthMsg {
    Open(u64),
    Refresh,
    Close,
    Loaded(wire::Response),
    Failed(String),
}

impl App {
    pub(crate) fn update_health(&mut self, ctx: &Context<Self>, msg: HealthMsg) -> bool {
        let link = ctx.link();

        if let HealthMsg::Open(node_id) = msg {
            let check = self
                .stored
                .iter()
                .find(|d| d.handle.node_id == node_id)
                .and_then(|d| d.health_check.clone());
            self.health_view = Some(HealthView {
                node_id,
                loading: true,
                check,
                error: None,
            });
            check_health(link, node_id);
            return true;
        }

        let Some(view) = self.health_view.as_mut() else {
            return false;
        };

        match msg {
            HealthMsg::Open(_) => unreachable!("opened above"),
            HealthMsg::Refresh => {
                view.loading = true;
                view.error = None;
                check_health(link, view.node_id);
            }
            HealthMsg::Close => self.health_view = None,
            HealthMsg::Loaded(wire::Response::Health { node_id, check }) => {
                if node_id == view.node_id {
                    view.loading = false;
                    view.check = Some(check);
                }
            }
            HealthMsg::Loaded(wire::Response::Error { message }) | HealthMsg::Failed(message) => {
                view.loading = false;
                view.error = Some(message);
            }
            HealthMsg::Loaded(_) => {}
        }

        true
    }

    /// Full-screen popup with a device's health check.
    pub(crate) fn view_health_modal(&self, link: &Scope<Self>) -> Html {
        let Some(view) = &self.health_view else {
            return html! {};
        };
        let node_id = view.node_id;
        let title = self.device_title(node_id);
        let network = self
            .stored
            .iter()
            .find(|d| d.handle.node_id == node_id)
            .and_then(|d| d.network);

        html! {
            <div class="modal is-active">
                <div class="modal-background"></div>
                <div class="modal-card is-fullscreen">
                    { ui::modal_head("Health", &title, html! {
                        <>
                        { self.device_state(node_id).1 }
                        <button
                            class={classes!("button", "is-small", view.loading.then_some("is-loading"))}
                            title="Check it again"
                            disabled={view.loading}
                            onclick={link.callback(|_| health(HealthMsg::Refresh))}>
                            <span class="icon is-small">{ icon(Icon::Refresh, 16) }</span>
                            <span>{ "Refresh" }</span>
                        </button>
                        </>
                    }, link.callback(|_| health(HealthMsg::Close))) }
                    <section class="modal-card-body">
                        <div class="app container">
                            if let Some(error) = &view.error {
                                <p class="tasks-error">{ icon(Icon::Alert, 16) }<span>{ error.clone() }</span></p>
                            }
                            { match &view.check {
                                None if view.loading => html! {
                                    <div class="empty-state">
                                        <span class="loader is-large"></span>
                                        <p class="empty-state-text">{ "Checking its health…" }</p>
                                    </div>
                                },
                                None => html! {},
                                Some(check) => view_check(check, node_id, network, link),
                            } }
                        </div>
                    </section>
                </div>
            </div>
        }
    }
}

/// A check, in one card: the verdict, then a row per area.
fn view_check(
    check: &HealthCheck,
    node_id: u64,
    network: Option<NetworkKind>,
    link: &Scope<App>,
) -> Html {
    html! {
        <div class="health-card">
            { view_verdict(check) }
            { for check.areas.iter().map(|area| view_area(area, check.crash_signs, node_id, network, link)) }
            <p class="health-note">
                { "The verdicts follow rules of thumb: a stack is judged by the share of it used at its \
                   peak and by the bytes never used, which should not fall below a floor that grows with \
                   the device's heap." }
            </p>
        </div>
    }
}

/// The verdict of a check, and when it was taken.
fn view_verdict(check: &HealthCheck) -> Html {
    let (glyph, text) = match check.verdict {
        HealthVerdict::Good => (Icon::CheckCircle, "Nothing wrong found."),
        HealthVerdict::Fair => (Icon::Alert, "Something to keep an eye on."),
        HealthVerdict::Poor => (Icon::XCircle, "Something is wrong."),
        HealthVerdict::Unknown => (Icon::Info, "It reports too little to tell."),
    };

    html! {
        <div class={classes!("health-verdict", verdict_class(check.verdict))}>
            <span class="health-verdict-icon">{ icon(glyph, 22) }</span>
            <div>
                <p class="health-verdict-title">{ check.verdict.label() }</p>
                <p class="health-verdict-text">
                    { text }
                    if let Some(ms) = check.checked_ms {
                        { format!(" Checked {}.", checked_label(ms)) }
                    }
                </p>
            </div>
        </div>
    }
}

/// One area of a check, as a row: its verdict, summary and findings (with the
/// crash signs, for the crashes), and the popup with more of it.
fn view_area(
    area: &HealthArea,
    crash_signs: bool,
    node_id: u64,
    network: Option<NetworkKind>,
    link: &Scope<App>,
) -> Html {
    let (glyph, label, open): (Icon, &str, fn(u64) -> Msg) = match area.kind {
        HealthAreaKind::Crashes => (Icon::Reboot, "Logs", Msg::OpenLogs),
        HealthAreaKind::Heap | HealthAreaKind::Stacks => (Icon::Chip, "Memory", |node_id| {
            Msg::Memory(crate::memory::MemoryMsg::Open(node_id))
        }),
        HealthAreaKind::Link => (
            kind_icon(network).unwrap_or(Icon::Globe),
            "Network",
            Msg::OpenNetwork,
        ),
    };
    let crash_note = crash_signs && area.kind == HealthAreaKind::Crashes;

    html! {
        <div class="health-row">
            <div class="health-row-name">
                <span class="health-tile-icon">{ icon(glyph, 16) }</span>
                <span class="health-row-title">{ area.kind.label() }</span>
            </div>
            <div class="health-row-body">
                <p class={classes!("health-row-summary",
                    (area.verdict == HealthVerdict::Unknown).then_some("is-unknown"))}>
                    { area.summary.clone() }
                </p>
                { for area.findings.iter().map(|finding| html! {
                    <p class={classes!("health-finding", finding_class(finding.verdict))}>
                        { icon(Icon::Alert, 14) }
                        <span>{ finding.text.clone() }</span>
                    </p>
                }) }
                if crash_note {
                    <p class="health-finding is-info">
                        { icon(Icon::Info, 14) }
                        <span>
                            { "There are signs of a crash: its crash log, if it keeps one, may tell more. \
                               The device may delete the log once it is retrieved." }
                        </span>
                    </p>
                }
            </div>
            <div class="health-row-end">
                { verdict_pill(area.verdict) }
                <button class="button is-small is-soft"
                    onclick={link.batch_callback(move |_| vec![health(HealthMsg::Close), open(node_id)])}>
                    <span>{ label }</span>
                    <span class="icon is-small">{ icon(Icon::ChevronRight, 16) }</span>
                </button>
            </div>
        </div>
    }
}

/// A verdict as a pill: a coloured dot and its word.
pub(crate) fn verdict_pill(verdict: HealthVerdict) -> Html {
    ui::pill(verdict.label(), verdict_class(verdict), None)
}

fn verdict_class(verdict: HealthVerdict) -> Option<&'static str> {
    match verdict {
        HealthVerdict::Good => Some("is-up"),
        HealthVerdict::Fair => Some("is-warning"),
        HealthVerdict::Poor => Some("is-danger"),
        HealthVerdict::Unknown => None,
    }
}

fn finding_class(verdict: HealthVerdict) -> &'static str {
    match verdict {
        HealthVerdict::Poor => "is-danger",
        _ => "is-warning",
    }
}

/// When a check was taken: in our local time, only the time if today.
fn checked_label(ms: u64) -> String {
    let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms as f64));
    let today = js_sys::Date::new_0().to_date_string() == date.to_date_string();
    let time = String::from(date.to_locale_time_string("en-GB"));

    if today {
        format!("at {time}")
    } else {
        let day = date.to_locale_date_string("en-GB", &wasm_bindgen::JsValue::UNDEFINED);
        format!("on {} at {time}", String::from(day))
    }
}

fn health(msg: HealthMsg) -> Msg {
    Msg::Health(msg)
}

/// Check the device's health; the answer goes to the popup.
fn check_health(link: &Scope<App>, node_id: u64) {
    request_loop(
        link.clone(),
        wire::Request::CheckHealth { node_id },
        |response| health(HealthMsg::Loaded(response)),
        |e| health(HealthMsg::Failed(e)),
    );
}
