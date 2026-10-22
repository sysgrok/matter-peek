//! A device's clock, in a popup: its time and how far from ours, where it
//! takes its time from, its time zone; and setting it from ours (we are a time
//! source for our devices).

use yew::html::Scope;
use yew::prelude::*;

use matter_peek_core::api::{display, wire, DeviceTime, TimeSetupStep};

use crate::icons::{icon, Icon};
use crate::{request_loop, ui, App, Msg};

/// The clock popup of one device.
pub struct ClockView {
    pub node_id: u64,
    pub loading: bool,
    /// As last read; `None` before the first read.
    pub time: Option<DeviceTime>,
    /// What the last setting of it did.
    pub steps: Vec<TimeSetupStep>,
    pub error: Option<String>,
}

/// What happens in the clock popup.
pub enum ClockMsg {
    Open(u64),
    Close,
    Read,
    Set,
    Loaded(wire::Response),
    Failed(String),
}

impl App {
    pub(crate) fn update_clock(&mut self, ctx: &Context<Self>, msg: ClockMsg) -> bool {
        let link = ctx.link();

        if let ClockMsg::Open(node_id) = msg {
            self.clock = Some(ClockView {
                node_id,
                loading: true,
                time: None,
                steps: Vec::new(),
                error: None,
            });
            send(link, wire::Request::DeviceTime { node_id });
            return true;
        }

        let Some(view) = self.clock.as_mut() else {
            return false;
        };

        match msg {
            ClockMsg::Open(_) => unreachable!("opened above"),
            ClockMsg::Close => self.clock = None,
            ClockMsg::Read | ClockMsg::Set => {
                let node_id = view.node_id;
                let request = if matches!(msg, ClockMsg::Set) {
                    wire::Request::SetDeviceTime { node_id }
                } else {
                    wire::Request::DeviceTime { node_id }
                };
                view.loading = true;
                view.error = None;
                view.steps.clear();
                send(link, request);
            }
            ClockMsg::Loaded(wire::Response::DeviceTime { time }) => {
                if time.node_id == view.node_id {
                    view.loading = false;
                    view.time = Some(time);
                }
            }
            ClockMsg::Loaded(wire::Response::TimeSetup { setup }) => {
                if setup.time.node_id == view.node_id {
                    view.loading = false;
                    view.steps = setup.steps;
                    view.time = Some(setup.time);
                }
            }
            ClockMsg::Loaded(wire::Response::Error { message }) | ClockMsg::Failed(message) => {
                view.loading = false;
                view.error = Some(message);
            }
            ClockMsg::Loaded(_) => {}
        }

        true
    }

    /// Popup with a device's clock.
    pub(crate) fn view_clock_modal(&self, link: &Scope<Self>) -> Html {
        let Some(view) = &self.clock else {
            return html! {};
        };
        let title = self.device_title(view.node_id);
        let supported = view.time.as_ref().is_some_and(|time| time.supported);

        html! {
            <div class="modal is-active">
                <div class="modal-background" onclick={link.callback(|_| clock(ClockMsg::Close))}></div>
                <div class="modal-card clock-card">
                    { ui::modal_head("Clock", &title, html! {},
                        link.callback(|_| clock(ClockMsg::Close))) }
                    <section class="modal-card-body">
                        if let Some(error) = &view.error {
                            <p class="share-error">{ icon(Icon::Alert, 16) }<span>{ error.clone() }</span></p>
                        }
                        if !view.steps.is_empty() {
                            <ul class="clock-steps">
                                { for view.steps.iter().map(view_step) }
                            </ul>
                        }
                        { match &view.time {
                            None if view.loading => html! {
                                <div class="empty-state">
                                    <span class="loader is-large"></span>
                                    <p class="empty-state-text">{ "Reading its clock…" }</p>
                                </div>
                            },
                            None => html! {},
                            Some(time) if !time.supported => html! {
                                <p>{ "It keeps no clock we can see or set: it has no Time Synchronization cluster." }</p>
                            },
                            Some(time) => view_time(time),
                        } }
                        <div class="share-actions">
                            <button class={classes!("button", "is-small", "is-soft", view.loading.then_some("is-loading"))}
                                disabled={view.loading} onclick={link.callback(|_| clock(ClockMsg::Read))}>
                                <span class="icon is-small">{ icon(Icon::Refresh, 16) }</span>
                                <span>{ "Read again" }</span>
                            </button>
                            if supported {
                                <button class={classes!("button", "is-small", "is-link", view.loading.then_some("is-loading"))}
                                    disabled={view.loading}
                                    title="Its UTC time, time zone and daylight saving time from ours, and us as its time source"
                                    onclick={link.callback(|_| clock(ClockMsg::Set))}>
                                    <span class="icon is-small">{ icon(Icon::Clock, 16) }</span>
                                    <span>{ "Set from ours" }</span>
                                </button>
                            }
                        </div>
                    </section>
                </div>
            </div>
        }
    }
}

fn view_step(step: &TimeSetupStep) -> Html {
    match &step.error {
        None => html! {
            <li class="is-done">{ icon(Icon::Check, 15) }<span>{ format!("Set {}", step.what) }</span></li>
        },
        Some(error) => html! {
            <li class="is-failed">{ icon(Icon::Alert, 15) }<span>{ format!("Not set {}: {error}", step.what) }</span></li>
        },
    }
}

fn view_time(time: &DeviceTime) -> Html {
    let f = &time.features;
    let utc = time
        .utc_unix_ms
        .map(display::utc_label)
        .unwrap_or_else(|| "None: it has no time".into());
    let accuracy = match time.time_source {
        Some(source) => format!(
            "{}, from {}",
            time.granularity.label(),
            display::time_source_label(source)
        ),
        None => time.granularity.label().to_string(),
    };
    let features: Vec<&str> = [
        (f.time_zone, "time zone"),
        (f.ntp_client, "NTP client"),
        (f.ntp_server, "NTP server"),
        (f.time_sync_client, "takes time from a node"),
    ]
    .into_iter()
    .filter_map(|(has, what)| has.then_some(what))
    .collect();

    html! {
        <dl class="device-facts clock-facts">
            <div>
                <dt>{ "UTC" }</dt>
                <dd class="mono">{ utc }</dd>
            </div>
            <div>
                <dt>{ "From ours" }</dt>
                <dd>{ time.offset_ms.map(display::clock_offset_label).unwrap_or_else(|| "—".into()) }</dd>
            </div>
            <div>
                <dt>{ "Accuracy" }</dt>
                <dd>{ accuracy }</dd>
            </div>
            if f.time_sync_client {
                <div>
                    <dt>{ "Takes its time from" }</dt>
                    <dd>{ time.trusted_source_label().unwrap_or_else(|| "No node".into()) }</dd>
                </div>
            }
            if f.ntp_client {
                <div>
                    <dt>{ "NTP server" }</dt>
                    <dd class="mono">{ time.default_ntp.clone().unwrap_or_else(|| "—".into()) }</dd>
                </div>
            }
            if f.time_zone {
                <div>
                    <dt>{ "Time zone" }</dt>
                    <dd>
                        { for time.time_zones.iter().map(|zone| html! {
                            <span class="clock-line">
                                { display::utc_offset_label(zone.offset_secs) }
                                { zone.name.as_deref().map(|name| format!(" {name}")).unwrap_or_default() }
                                { zone.valid_at_unix_ms.map(|ms| format!(" from {}", display::utc_label(ms))).unwrap_or_default() }
                            </span>
                        }) }
                    </dd>
                </div>
                <div>
                    <dt>{ "Daylight saving" }</dt>
                    <dd>
                        if time.dst_offsets.is_empty() {
                            { "None set" }
                        }
                        { for time.dst_offsets.iter().map(|dst| html! {
                            <span class="clock-line">
                                { display::utc_offset_label(dst.offset_secs) }
                                { dst.from_unix_ms.map(|ms| format!(" from {}", display::utc_label(ms))).unwrap_or_default() }
                                { dst.until_unix_ms.map(|ms| format!(" until {}", display::utc_label(ms))).unwrap_or_default() }
                            </span>
                        }) }
                    </dd>
                </div>
                <div>
                    <dt>{ "Local time" }</dt>
                    <dd class="mono">{ time.local_unix_ms.map(display::local_time_label).unwrap_or_else(|| "—".into()) }</dd>
                </div>
            }
            <div>
                <dt>{ "Features" }</dt>
                <dd>{ if features.is_empty() { "None (UTC time only)".to_string() } else { features.join(", ") } }</dd>
            </div>
        </dl>
    }
}

fn clock(msg: ClockMsg) -> Msg {
    Msg::Clock(msg)
}

/// Send `request`; the answer goes to the popup.
fn send(link: &Scope<App>, request: wire::Request) {
    request_loop(
        link.clone(),
        request,
        |response| clock(ClockMsg::Loaded(response)),
        |e| clock(ClockMsg::Failed(e)),
    );
}
