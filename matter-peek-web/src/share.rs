//! Sharing a device with another controller (Google Home, Home Assistant, ...),
//! in a popup: a commissioning window on it, and the code to commission it
//! with while the window is open.

use wasm_bindgen_futures::spawn_local;
use yew::html::Scope;
use yew::prelude::*;

use matter_peek_core::api::{display, wire, CommissioningWindow, WindowStatus};

use crate::icons::{icon, Icon};
use crate::{play, request_loop, ui, App, Msg};

/// How long a window stays open, at most and by default, in minutes.
const MAX_MINUTES: u16 = 15;
/// … and at least.
const MIN_MINUTES: u16 = 3;

/// The modules around a QR code that are left light.
const QR_QUIET: usize = 4;

/// The share popup of one device.
pub struct ShareView {
    pub node_id: u64,
    pub loading: bool,
    /// As last read; `None` before the first read.
    pub window: Option<CommissioningWindow>,
    /// When it was read, in ms since the epoch: the countdown starts there.
    pub read_at: f64,
    pub error: Option<String>,
    /// How long a window it opens stays open.
    pub minutes: u16,
    /// Whether the countdown runs.
    pub ticking: bool,
    pub copied: bool,
    /// Android's screens to pick another app are open.
    pub sharing: bool,
    /// Pick another app once the window being opened is.
    pub share_when_open: bool,
    /// How sharing with another app went.
    pub shared: Option<String>,
}

/// What happens in the share popup.
pub enum ShareMsg {
    Open(u64),
    Close,
    Minutes(u16),
    OpenWindow,
    CloseWindow,
    Copy,
    Tick,
    Loaded(wire::Response),
    Failed(String),
    /// Share it with another Matter app on the phone (opening a window first
    /// if there is none of ours).
    PlayShare,
    /// Android's screens closed: whether an app was picked.
    PlayShared(Result<bool, String>),
}

impl App {
    pub(crate) fn update_share(&mut self, ctx: &Context<Self>, msg: ShareMsg) -> bool {
        let link = ctx.link();

        if let ShareMsg::Open(node_id) = msg {
            self.share = Some(ShareView {
                node_id,
                loading: true,
                window: None,
                read_at: 0.0,
                error: None,
                minutes: MAX_MINUTES,
                ticking: false,
                copied: false,
                sharing: false,
                share_when_open: false,
                shared: None,
            });
            send(link, wire::Request::CommissioningWindow { node_id });
            return true;
        }

        if let ShareMsg::PlayShare = msg {
            return self.play_share(ctx);
        }

        let Some(view) = self.share.as_mut() else {
            return false;
        };

        match msg {
            ShareMsg::Open(_) | ShareMsg::PlayShare => unreachable!("done above"),
            ShareMsg::PlayShared(shared) => {
                view.sharing = false;
                match shared {
                    Ok(true) => {
                        view.shared = Some(
                            "Handed to the app picked: it commissions the device while the window is open."
                                .into(),
                        )
                    }
                    Ok(false) => {}
                    Err(e) => view.error = Some(e),
                }
            }
            ShareMsg::Close => self.share = None,
            ShareMsg::Minutes(minutes) => view.minutes = minutes,
            ShareMsg::OpenWindow => {
                view.loading = true;
                view.error = None;
                send(
                    link,
                    wire::Request::OpenCommissioningWindow {
                        node_id: view.node_id,
                        timeout_secs: Some(view.minutes * 60),
                    },
                );
            }
            ShareMsg::CloseWindow => {
                view.loading = true;
                view.error = None;
                send(
                    link,
                    wire::Request::CloseCommissioningWindow {
                        node_id: view.node_id,
                    },
                );
            }
            ShareMsg::Copy => {
                if let Some(code) = view.window.as_ref().and_then(|w| w.manual_code.clone()) {
                    if let Some(window) = web_sys::window() {
                        let _ = window.navigator().clipboard().write_text(&code);
                        view.copied = true;
                    }
                }
            }
            ShareMsg::Tick => {
                view.ticking = false;
                match remaining(view) {
                    // Closed: see how the device has it.
                    Some(0) if !view.loading => {
                        view.loading = true;
                        send(
                            link,
                            wire::Request::CommissioningWindow {
                                node_id: view.node_id,
                            },
                        );
                    }
                    Some(_) => tick(link, view),
                    None => {}
                }
            }
            ShareMsg::Loaded(wire::Response::CommissioningWindow { window }) => {
                if window.node_id == view.node_id {
                    view.loading = false;
                    let ours = window.passcode.is_some();
                    view.window = Some(window);
                    view.read_at = js_sys::Date::now();
                    view.copied = false;
                    tick(link, view);
                    if std::mem::take(&mut view.share_when_open) && ours {
                        return self.play_share(ctx);
                    }
                }
            }
            ShareMsg::Loaded(wire::Response::Error { message }) | ShareMsg::Failed(message) => {
                view.loading = false;
                view.share_when_open = false;
                view.error = Some(message);
            }
            ShareMsg::Loaded(_) => {}
        }

        true
    }

    /// Share the device of the popup with another Matter app on the phone,
    /// through our window on it, opening one first if there is none.
    fn play_share(&mut self, ctx: &Context<Self>) -> bool {
        let Some(view) = &self.share else {
            return false;
        };
        let node_id = view.node_id;
        let window = view
            .window
            .as_ref()
            .and_then(|window| Some((window.discriminator?, window.passcode?)));
        let remaining_secs = remaining(view).unwrap_or_default();

        let Some((discriminator, passcode)) = window.filter(|_| remaining_secs > 0) else {
            if let Some(view) = self.share.as_mut() {
                view.share_when_open = true;
            }
            return self.update_share(ctx, ShareMsg::OpenWindow);
        };

        let device = self.stored.iter().find(|d| d.handle.node_id == node_id);
        let device = play::ToShare {
            name: self.device_title(node_id),
            vendor_id: device.and_then(|d| d.fingerprint.vendor_id),
            product_id: device.and_then(|d| d.fingerprint.product_id),
            discriminator,
            passcode,
            remaining_secs,
        };
        if let Some(view) = self.share.as_mut() {
            view.sharing = true;
            view.error = None;
            view.shared = None;
        }

        let link = ctx.link().clone();
        spawn_local(async move {
            let shared = play::share(device).await;
            link.send_message(share(ShareMsg::PlayShared(shared)));
        });
        true
    }

    /// Popup with a device's commissioning window, and its code.
    pub(crate) fn view_share_modal(&self, link: &Scope<Self>) -> Html {
        let Some(view) = &self.share else {
            return html! {};
        };
        let title = self.device_title(view.node_id);

        html! {
            <div class="modal is-active">
                <div class="modal-background" onclick={link.callback(|_| share(ShareMsg::Close))}></div>
                <div class="modal-card share-card">
                    { ui::modal_head("Share with another app", &title, self.device_state(view.node_id).1,
                        link.callback(|_| share(ShareMsg::Close))) }
                    <section class="modal-card-body">
                        if let Some(error) = &view.error {
                            <p class="share-error">{ icon(Icon::Alert, 16) }<span>{ error.clone() }</span></p>
                        }
                        { match &view.window {
                            None if view.loading => html! {
                                <div class="empty-state">
                                    <span class="loader is-large"></span>
                                    <p class="empty-state-text">{ "Reading its commissioning window…" }</p>
                                </div>
                            },
                            None => html! {},
                            Some(window) if window.manual_code.is_some() => view_code(view, window, self.can_play, link),
                            Some(window) => view_window(view, window, self.can_play, link),
                        } }
                    </section>
                </div>
            </div>
        }
    }
}

/// A window we opened: its QR code, code and countdown; with `play`, sharing
/// it with another app on the phone.
fn view_code(
    view: &ShareView,
    window: &CommissioningWindow,
    play: bool,
    link: &Scope<App>,
) -> Html {
    let code = window
        .manual_code
        .as_deref()
        .map(display::pairing_code_label)
        .unwrap_or_default();
    let left = remaining(view).unwrap_or_default();

    html! {
        <div class="share-open">
            <div class="share-qr">{ qr_svg(&window.qr_rows) }</div>
            <div class="share-details">
                <p class="share-label">{ "Pairing code" }</p>
                <div class="share-code-row">
                    <span class="share-code mono">{ code }</span>
                    <button class="button is-small is-soft" title="Copy the code"
                        onclick={link.callback(|_| share(ShareMsg::Copy))}>
                        <span class="icon is-small">{ icon(if view.copied { Icon::Check } else { Icon::Copy }, 16) }</span>
                        <span>{ if view.copied { "Copied" } else { "Copy" } }</span>
                    </button>
                </div>
                if let Some(qr) = &window.qr_code {
                    <p class="share-qr-text mono" title="The QR code's text">{ qr.clone() }</p>
                }
                <p class="share-hint">
                    { "In the other app (Google Home, Home Assistant, Apple Home, …), add a Matter device, then scan the QR code or enter the code. It works only while this window is open." }
                </p>
                <p class="share-timer">
                    { icon(Icon::Timer, 16) }
                    <span>{ if left > 0 { format!("Closes in {}", display::duration(left)) } else { "Closing…".to_string() } }</span>
                </p>
                { fabrics(window) }
                if let Some(shared) = &view.shared {
                    <p class="share-hint">{ shared.clone() }</p>
                }
                <div class="share-actions">
                    if play {
                        { play_button(view, link) }
                    }
                    <button class={classes!("button", "is-small", "is-danger", "is-soft", view.loading.then_some("is-loading"))}
                        disabled={view.loading} onclick={link.callback(|_| share(ShareMsg::CloseWindow))}>
                        <span class="icon is-small">{ icon(Icon::XCircle, 16) }</span>
                        <span>{ "Close window" }</span>
                    </button>
                </div>
            </div>
        </div>
    }
}

/// Share it with another Matter app on the phone, through Google Play
/// services.
fn play_button(view: &ShareView, link: &Scope<App>) -> Html {
    let busy = view.sharing || view.share_when_open;

    html! {
        <button class={classes!("button", "is-small", "is-link", busy.then_some("is-loading"))}
            title="Pick another Matter app on this phone (Google Home, Home Assistant, …): it commissions the device through this window"
            disabled={view.loading || busy} onclick={link.callback(|_| share(ShareMsg::PlayShare))}>
            <span class="icon is-small">{ icon(Icon::Share, 16) }</span>
            <span>{ "Share with an app on this phone" }</span>
        </button>
    }
}

/// A closed window, or one we did not open; with `play`, sharing with another
/// app on the phone (which opens one).
fn view_window(
    view: &ShareView,
    window: &CommissioningWindow,
    play: bool,
    link: &Scope<App>,
) -> Html {
    let full = matches!(
        (window.fabrics, window.supported_fabrics),
        (Some(fabrics), Some(supported)) if fabrics >= supported
    );
    let opened_by = window
        .opened_by_vendor
        .map(|vid| format!(" by {}", display::vendor_label(vid, None)))
        .unwrap_or_default();

    html! {
        <div class="share-closed">
            { match window.status {
                WindowStatus::Closed => html! {
                    <p>{ "Open a commissioning window on it, and another app (Google Home, Home Assistant, Apple Home, …) can commission it too, with a code made for this window. It stays on our fabric." }</p>
                },
                WindowStatus::Enhanced => html! {
                    <p class="share-warning">{ icon(Icon::Info, 16) }<span>{ format!("A commissioning window is open, opened{opened_by}, with a code we do not know. Close it to open one of ours.") }</span></p>
                },
                WindowStatus::Basic => html! {
                    <p class="share-warning">{ icon(Icon::Info, 16) }<span>{ format!("A basic commissioning window is open, opened{opened_by}: the device's own setup code commissions it.") }</span></p>
                },
            } }
            { fabrics(window) }
            if full {
                <p class="share-warning">{ icon(Icon::Alert, 16) }<span>{ "It is on as many fabrics as it supports: remove one before another app can commission it." }</span></p>
            }
            <div class="share-actions">
                if window.status == WindowStatus::Closed {
                    <div class="select is-small">
                        <select onchange={link.callback(|e: Event| share(ShareMsg::Minutes(
                            crate::select_value(e).parse().unwrap_or(MAX_MINUTES))))}>
                            { for (MIN_MINUTES..=MAX_MINUTES).rev().map(|minutes| html! {
                                <option value={minutes.to_string()} selected={minutes == view.minutes}>
                                    { format!("for {minutes} min") }
                                </option>
                            }) }
                        </select>
                    </div>
                    <button class={classes!("button", "is-small", if play { "is-soft" } else { "is-link" }, view.loading.then_some("is-loading"))}
                        disabled={view.loading || full} onclick={link.callback(|_| share(ShareMsg::OpenWindow))}>
                        <span class="icon is-small">{ icon(Icon::Share, 16) }</span>
                        <span>{ "Open commissioning window" }</span>
                    </button>
                    if play && !full {
                        { play_button(view, link) }
                    }
                } else {
                    <button class={classes!("button", "is-small", "is-danger", "is-soft", view.loading.then_some("is-loading"))}
                        disabled={view.loading} onclick={link.callback(|_| share(ShareMsg::CloseWindow))}>
                        <span class="icon is-small">{ icon(Icon::XCircle, 16) }</span>
                        <span>{ "Close window" }</span>
                    </button>
                }
            </div>
        </div>
    }
}

fn fabrics(window: &CommissioningWindow) -> Html {
    match (window.fabrics, window.supported_fabrics) {
        (Some(fabrics), Some(supported)) => html! {
            <p class="share-fabrics">{ format!("On {fabrics} of {supported} fabrics (controllers it can belong to).") }</p>
        },
        _ => html! {},
    }
}

/// The QR code, dark on light whatever the theme, so that phones read it.
fn qr_svg(rows: &[String]) -> Html {
    let size = rows.len() + 2 * QR_QUIET;
    let path: String = rows
        .iter()
        .enumerate()
        .flat_map(|(y, row)| {
            row.bytes()
                .enumerate()
                .filter(|(_, module)| *module == b'1')
                .map(move |(x, _)| format!("M{} {}h1v1h-1z", x + QR_QUIET, y + QR_QUIET))
        })
        .collect();

    html! {
        <svg class="share-qr-svg" viewBox={format!("0 0 {size} {size}")} shape-rendering="crispEdges"
            role="img" aria-label="QR code">
            <rect width={size.to_string()} height={size.to_string()} fill="#fff" />
            <path d={path} fill="#000" />
        </svg>
    }
}

/// Seconds left of the window we opened, counted down since it was read.
fn remaining(view: &ShareView) -> Option<u64> {
    let secs = view.window.as_ref()?.remaining_secs?;
    let gone = ((js_sys::Date::now() - view.read_at) / 1000.0).max(0.0) as u64;

    Some(secs.saturating_sub(gone))
}

/// Count down: a tick in a second, while a window of ours is open.
fn tick(link: &Scope<App>, view: &mut ShareView) {
    if view.ticking || remaining(view).is_none() {
        return;
    }

    view.ticking = true;
    let link = link.clone();
    spawn_local(async move {
        gloo_timers::future::TimeoutFuture::new(1_000).await;
        link.send_message(share(ShareMsg::Tick));
    });
}

fn share(msg: ShareMsg) -> Msg {
    Msg::Share(msg)
}

/// Send `request`; the answer goes to the popup.
fn send(link: &Scope<App>, request: wire::Request) {
    request_loop(
        link.clone(),
        request,
        |response| share(ShareMsg::Loaded(response)),
        |e| share(ShareMsg::Failed(e)),
    );
}
