//! The Commission page: how to reach the device, its setup code, and how the
//! commission goes; and the popup discovering commissionable devices.

use std::net::SocketAddr;

use yew::html::Scope;
use yew::prelude::*;

use matter_peek_core::api::{self, display, wire};

use crate::icons::{icon, Icon};
use crate::ui;
use crate::{
    checkbox_checked, input_value, picked_file, textarea_value, valid_bt_addr, valid_hex,
    valid_setup_code, App, Msg, StepLine, Tab, Via,
};

/// How a step of the commission went, as its line shows it.
#[derive(Clone, Copy, PartialEq)]
enum StepState {
    Done,
    /// The commission is at it.
    Running,
    /// The commission failed at it.
    Failed,
    /// A cancel stopped the commission at it.
    Stopped,
}

impl App {
    pub(crate) fn view_commission_tab(&self, link: &Scope<Self>) -> Html {
        let choice = |via: Via, glyph: Icon, title: &str, text: &str, usable: bool| {
            html! {
                <button class={classes!("choice", (self.via == via).then_some("is-selected"))}
                    disabled={self.running || !usable}
                    onclick={link.callback(move |_| Msg::SetVia(via))}>
                    <span class="choice-icon">{ icon(glyph, 22) }</span>
                    <span class="choice-title">{ title.to_string() }</span>
                    <span class="choice-text">{ text.to_string() }</span>
                </button>
            }
        };

        html! {
            <div class="narrow">
                { ui::page_header("Commission", match self.visible_fabric() {
                    Some(fabric) => html! {
                        <span class="commission-into">
                            { "Add a device to " }
                            { crate::our_fabrics::dot(fabric.color) }
                            <strong>{ fabric.name.clone() }</strong>
                            <span class="commission-into-hint">{ " · the fabric shown, at the top" }</span>
                        </span>
                    },
                    None => html! { "Add a device to our fabric" },
                }, html! {}) }
                { self.view_recommission_warning(link) }

                <div class="form-card">
                    { step(1, "How to reach it", html! {
                        <div class="choices">
                            if self.can_play {
                                { choice(Via::PlayServices, Icon::QrCode, "With Google Play services",
                                    "Android scans its QR code and puts it on your Wi-Fi or Thread network.", true) }
                            }
                            { choice(Via::Network, Icon::Globe, "Over the network",
                                "It is on your network already, with its commissioning window open.", true) }
                            // Unless the controller was built without Bluetooth (all but Linux);
                            // disabled, saying why, while it cannot use it.
                            if let Some(unavailable) = self.ble_choice() {
                                { match unavailable {
                                    None => choice(Via::Bluetooth, Icon::Bluetooth, "Over Bluetooth",
                                        "A new device: it joins your Wi-Fi or Thread network as it is commissioned.", true),
                                    Some(why) => choice(Via::Bluetooth, Icon::Bluetooth, "Over Bluetooth",
                                        &format!("Not available: {why}."), false),
                                } }
                            }
                        </div>
                    }) }

                    if self.via == Via::PlayServices {
                        { step(2, "The device", self.view_play_device()) }
                    } else {
                        { step(2, "Which device", if self.via == Via::Bluetooth {
                            self.view_ble_fields(link)
                        } else {
                            self.view_ip_fields(link)
                        }) }

                        { self.view_setup_code(link) }
                    }

                    { step(if self.via == Via::PlayServices { 3 } else { 4 }, "Keep a record", html! {
                        <label class="switch-row">
                            <input type="checkbox" class="switch" checked={self.remember}
                                onchange={link.callback(|e: Event| Msg::SetRemember(checkbox_checked(e)))} />
                            <span>
                                <span class="switch-title">{ "Remember it" }</span>
                                <span class="switch-text">
                                    { "So it can be tagged, and stays listed while offline. Either way, it stays on our fabric." }
                                </span>
                            </span>
                        </label>
                    }) }

                    <div class="form-actions">
                        <button class={classes!("button", "is-link", "is-medium", self.running.then_some("is-loading"))}
                            disabled={self.running || !self.can_commission()}
                            onclick={link.callback(|_| Msg::Commission)}>
                            <span class="icon">{ icon(Icon::Commission, 20) }</span>
                            <span>{ "Commission" }</span>
                        </button>
                    </div>
                </div>

                { self.view_progress(link) }
            </div>
        }
    }

    /// Whether to offer commissioning over Bluetooth: `None` if the controller
    /// was built without it, else why it cannot now, if it cannot.
    fn ble_choice(&self) -> Option<Option<String>> {
        match &self.can_ble {
            Some((false, _)) => None,
            Some((true, unavailable)) => Some(unavailable.clone()),
            // Not known yet.
            None => Some(None),
        }
    }

    /// Its setup code, typed or read from its QR code.
    fn view_setup_code(&self, link: &Scope<Self>) -> Html {
        let code = self.setup_code.trim();
        let code_valid = valid_setup_code(&self.setup_code);
        let code_invalid = !code.is_empty() && !code_valid;

        step(
            3,
            "Its setup code",
            html! {
                <>
                    <div class="field has-addons">
                        <div class="control is-expanded has-icons-left has-icons-right">
                            <input class={classes!("input", "mono",
                                code_invalid.then_some("is-danger"), code_valid.then_some("is-success"))}
                                placeholder="20202021, 3497-0112-332 or MT:…"
                                value={self.setup_code.clone()}
                                oninput={link.callback(|e: InputEvent| Msg::SetupCode(input_value(e)))} />
                            <span class="icon is-left">{ icon(Icon::Key, 18) }</span>
                            if code_valid {
                                <span class="icon is-right has-text-success">{ icon(Icon::Check, 18) }</span>
                            }
                        </div>
                        <div class="control">
                            <button class="button" title="Scan the device's QR code with the camera"
                                disabled={self.running}
                                onclick={link.callback(|_| Msg::OpenCamera)}>
                                <span class="icon">{ icon(Icon::Camera, 18) }</span>
                                <span class="is-hidden-mobile">{ "Camera" }</span>
                            </button>
                        </div>
                        <div class="control">
                            <div class="file">
                                <label class="file-label"
                                    title="Read the setup code from a picture of the device's QR code">
                                    <input class="file-input" type="file" accept="image/png,image/jpeg"
                                        onchange={link.batch_callback(|e: Event| picked_file(e).map(Msg::QrImage))} />
                                    <span class="file-cta">
                                        <span class="file-icon">{ icon(Icon::QrCode, 18) }</span>
                                        <span class="file-label is-hidden-mobile">{ "Picture" }</span>
                                    </span>
                                </label>
                            </div>
                        </div>
                    </div>
                    <p class="help">{ "Its passcode, manual pairing code or QR code payload, as printed on it or its box." }</p>
                </>
            },
        )
    }

    /// With Google Play services: what happens, or the device shared with us.
    fn view_play_device(&self) -> Html {
        match &self.shared {
            Some(shared) => {
                let name = shared.name.as_deref().unwrap_or("A device");
                let room = shared
                    .room
                    .as_deref()
                    .map(|room| format!(", in {room}"))
                    .unwrap_or_default();

                html! {
                    <p>
                        <strong>{ name.to_string() }</strong>{ room }
                        { ": another Matter app shared it with us. Google Play services hands it over, and it is commissioned onto our fabric." }
                    </p>
                }
            }
            None => html! {
                <p class="help">
                    { "Google Play services scans its QR code and, over Bluetooth, puts it on your Wi-Fi or Thread network. It then hands it over, and it is commissioned onto our fabric." }
                </p>
            },
        }
    }

    /// Commissioning over the network: the device's address.
    fn view_ip_fields(&self, link: &Scope<Self>) -> Html {
        let addr = self.peer_addr.trim();
        let addr_invalid = !addr.is_empty() && addr.parse::<SocketAddr>().is_err();

        html! {
            <div class="field">
                <div class="field has-addons mb-1">
                    <div class="control is-expanded has-icons-left">
                        <input class={classes!("input", "mono", addr_invalid.then_some("is-danger"))}
                            placeholder="[IPv6]:port or IPv4:port"
                            value={self.peer_addr.clone()}
                            oninput={link.callback(|e: InputEvent| Msg::PeerAddr(input_value(e)))} />
                        <span class="icon is-left">{ icon(Icon::Globe, 18) }</span>
                    </div>
                    <div class="control">{ self.discover_button(link) }</div>
                </div>
                <p class="help">{ "Its address, or Discover finds it." }</p>
            </div>
        }
    }

    /// The button opening the popup of commissionable devices.
    fn discover_button(&self, link: &Scope<Self>) -> Html {
        html! {
            <button class="button is-link"
                title="Find commissionable devices, on the network and over Bluetooth"
                disabled={self.running}
                onclick={link.callback(|_| Msg::OpenDiscover)}>
                <span class="icon">{ icon(Icon::Radar, 18) }</span>
                <span class="is-hidden-mobile">{ "Discover" }</span>
            </button>
        }
    }

    /// Commissioning over Bluetooth: which device, and the networks it may join.
    fn view_ble_fields(&self, link: &Scope<Self>) -> Html {
        let addr = self.ble_addr.trim();
        let addr_invalid = !addr.is_empty() && !valid_bt_addr(addr);

        html! {
            <>
                <div class="field">
                    <div class="field has-addons mb-1">
                        <div class="control is-expanded has-icons-left">
                            <input class={classes!("input", "mono", addr_invalid.then_some("is-danger"))}
                                placeholder="Any device matching the setup code"
                                value={self.ble_addr.clone()}
                                oninput={link.callback(|e: InputEvent| Msg::BleAddr(input_value(e)))} />
                            <span class="icon is-left">{ icon(Icon::Bluetooth, 18) }</span>
                        </div>
                        <div class="control">{ self.discover_button(link) }</div>
                    </div>
                    <p class="help">{ "Its Bluetooth address, if several are in pairing mode." }</p>
                </div>

                <div class="field">
                    <label class="label is-small">{ "The network it joins" }</label>
                    <p class="help mt-0 mb-2">
                        { "It is asked whether it joins Wi-Fi or Thread, and gets the network of that kind." }
                    </p>
                    { self.view_saved_networks(link) }
                    { self.view_wifi_fields(link) }
                    { self.view_thread_field(link) }
                    <p class="help">
                        { if self.saved_networks.is_empty() {
                            "Give one of each kind if unsure. The network a device joins is saved for the next one."
                        } else {
                            "One given here goes before the saved one of its kind. The network a device joins is saved."
                        } }
                    </p>
                </div>
            </>
        }
    }

    /// The saved networks, each with a button to forget it.
    fn view_saved_networks(&self, link: &Scope<Self>) -> Html {
        if self.saved_networks.is_empty() {
            return html! {};
        }

        html! {
            <div class="saved-networks">
                { for self.saved_networks.iter().map(|network| {
                    let glyph = match network.kind {
                        api::NetworkKind::Thread => Icon::Thread,
                        _ => Icon::Wifi,
                    };
                    let id = network.id.clone();

                    html! {
                        <span class="tag-chip" title={format!("Saved {} network", network.kind.label())}>
                            { icon(glyph, 12) }
                            { network.name.clone() }
                            <button class="tag-chip-remove" disabled={self.running}
                                title={format!("Forget {}, with its {}", network.name,
                                    if network.kind == api::NetworkKind::Thread { "dataset" } else { "password" })}
                                onclick={link.callback(move |_| Msg::ForgetNetwork(id.clone()))}>
                                { icon(Icon::Close, 12) }
                            </button>
                        </span>
                    }
                }) }
            </div>
        }
    }

    /// The Wi-Fi network to give a device that joins Wi-Fi.
    fn view_wifi_fields(&self, link: &Scope<Self>) -> Html {
        html! {
            <div class="columns is-variable is-3 mb-0">
                <div class="column">
                    <div class="control has-icons-left">
                        <input class="input" placeholder="Wi-Fi network name (SSID)"
                            value={self.wifi_ssid.clone()}
                            oninput={link.callback(|e: InputEvent| Msg::WifiSsid(input_value(e)))} />
                        <span class="icon is-left">{ icon(Icon::Wifi, 18) }</span>
                    </div>
                </div>
                <div class="column">
                    <div class="field has-addons">
                        <div class="control has-icons-left is-expanded">
                            <input class="input" placeholder="Wi-Fi password"
                                type={if self.show_wifi_password { "text" } else { "password" }}
                                value={self.wifi_password.clone()}
                                oninput={link.callback(|e: InputEvent| Msg::WifiPassword(input_value(e)))} />
                            <span class="icon is-left">{ icon(Icon::Lock, 18) }</span>
                        </div>
                        <div class="control">
                            <button class="button"
                                title={if self.show_wifi_password { "Hide the password" } else { "Show the password" }}
                                onclick={link.callback(|_| Msg::ToggleWifiPassword)}>
                                <span class="icon">
                                    { icon(if self.show_wifi_password { Icon::EyeOff } else { Icon::Eye }, 18) }
                                </span>
                            </button>
                        </div>
                    </div>
                </div>
            </div>
        }
    }

    /// The Thread network to give a device that joins Thread.
    fn view_thread_field(&self, link: &Scope<Self>) -> Html {
        let dataset = self.thread_dataset.trim();
        let dataset_invalid = !dataset.is_empty() && !valid_hex(dataset);

        html! {
            <div class="field mb-1">
                <textarea class={classes!("textarea", "mono", dataset_invalid.then_some("is-danger"))} rows="2"
                    placeholder="Thread operational dataset, in hex (as `ot-ctl dataset active -x` prints it)"
                    value={self.thread_dataset.clone()}
                    oninput={link.callback(|e: InputEvent| Msg::ThreadDataset(textarea_value(e)))} />
            </div>
        }
    }

    /// The device stopped the commission for a network of `kind`: the fields to
    /// give one, and the button to commission it again.
    fn view_network_needed(&self, link: &Scope<Self>, kind: api::NetworkKind) -> Html {
        let given = match kind {
            api::NetworkKind::Thread => {
                let dataset = self.thread_dataset.trim();
                !dataset.is_empty() && valid_hex(dataset)
            }
            _ => !self.wifi_ssid.is_empty(),
        };

        html! {
            <div class="network-needed">
                { match kind {
                    api::NetworkKind::Thread => self.view_thread_field(link),
                    _ => self.view_wifi_fields(link),
                } }
                <button class="button is-link is-small"
                    disabled={!given || !self.can_commission()}
                    onclick={link.callback(|_| Msg::Commission)}>
                    <span class="icon">{ icon(Icon::Commission, 16) }</span>
                    <span>{ "Commission again" }</span>
                </button>
            </div>
        }
    }

    /// The "you just commissioned this device" guard: re-commissioning a device
    /// whose window has closed will hang, so steer the user to the Devices page
    /// or require an explicit "Commission anyway".
    fn view_recommission_warning(&self, link: &Scope<Self>) -> Html {
        if !self.confirm_recommission {
            return html! {};
        }

        html! {
            <div class="notification note is-warning">
                <span class="note-icon">{ icon(Icon::Alert, 20) }</span>
                <div class="note-text">
                    <p class="mb-3">
                        <strong>{ "This device was just commissioned." }</strong>
                        { " Its commissioning window has most likely closed, so commissioning it \
                           again will hang. To re-add it, first put it back into pairing mode." }
                    </p>
                    <div class="buttons">
                        <button class="button is-small is-link"
                            onclick={link.callback(|_| Msg::SelectTab(Tab::Devices))}>
                            { "Go to Devices" }
                        </button>
                        <button class="button is-small is-warning"
                            onclick={link.callback(|_| Msg::CommissionAnyway)}>
                            { "Commission anyway" }
                        </button>
                        <button class="button is-small is-text"
                            onclick={link.callback(|_| Msg::DismissRecommission)}>
                            { "Dismiss" }
                        </button>
                    </div>
                </div>
            </div>
        }
    }

    /// How the commission goes, or how it went: a bar, and the steps it got to.
    fn view_progress(&self, link: &Scope<Self>) -> Html {
        let failed = self.steps.last().and_then(|line| match &line.status {
            api::StepStatus::Failed { message } => Some(format!(
                "{} failed: {message}",
                display::commission_step_label(line.step)
            )),
            _ => None,
        });
        let timeout = display::short_duration(self.commission_timeout_secs().into());

        let (class, glyph, title, text, action) = if self.running {
            let (title, text) = match (self.cancelling, self.timed_out) {
                (false, _) => (
                    "Commissioning…".to_string(),
                    format!("This takes up to {timeout}."),
                ),
                (true, false) => (
                    "Cancelling…".to_string(),
                    "Stopping the commission, and rolling the device back.".to_string(),
                ),
                (true, true) => (
                    "Timed out, cancelling…".to_string(),
                    format!(
                        "It ran longer than {timeout}: stopping it, and rolling the device back."
                    ),
                ),
            };
            // Google Play services' own screens cannot be cancelled from here:
            // the commission proper starts once it hands the device over.
            let action = if self.via != Via::PlayServices || self.handing_over {
                ui::icon_button(
                    Icon::Close,
                    if self.cancelling {
                        "Cancelling"
                    } else {
                        "Cancel"
                    },
                    classes!("is-small", "is-danger", "is-light"),
                    self.cancelling,
                    Some("Stop the commission, and roll the device back"),
                    link.callback(|_| Msg::Cancel),
                )
            } else {
                html! {}
            };
            ("is-running", None, title, text, action)
        } else if let Some(wire::Response::Commissioned(result)) = &self.result {
            (
                "is-done",
                Some(Icon::CheckCircle),
                "Commissioned".to_string(),
                format!(
                    "The device is on our fabric as node {}.{}",
                    display::hex_u64(result.handle.node_id),
                    self.play_note
                        .as_deref()
                        .map(|note| format!(" {note}"))
                        .unwrap_or_default(),
                ),
                ui::icon_button(
                    Icon::Devices,
                    "Show it",
                    classes!("is-small", "is-tinted"),
                    false,
                    None,
                    link.callback(|_| Msg::SelectTab(Tab::Devices)),
                ),
            )
        } else if let Some(wire::Response::Cancelled { decommissioned }) = &self.result {
            let how = if *decommissioned {
                "The device had completed it already, and was decommissioned."
            } else {
                "The device was rolled back."
            };
            if self.timed_out {
                (
                    "is-cancelled",
                    Some(Icon::Clock),
                    "Commissioning timed out".to_string(),
                    format!("It ran longer than {timeout}. {how}"),
                    html! {},
                )
            } else {
                (
                    "is-cancelled",
                    Some(Icon::Stop),
                    "Commissioning cancelled".to_string(),
                    how.to_string(),
                    html! {},
                )
            }
        } else if let Some(wire::Response::NetworkNeeded { kind }) = &self.result {
            (
                "is-cancelled",
                Some(Icon::Alert),
                format!("It joins a {} network", kind.label()),
                format!(
                    "No {} network was given or saved, so it was given nothing. Give one here, \
                     and commission it again.",
                    kind.label()
                ),
                html! {},
            )
        } else if let Some(message) = failed.or_else(|| match &self.result {
            Some(wire::Response::Error { message }) => Some(message.clone()),
            _ => None,
        }) {
            (
                "is-failed",
                Some(Icon::XCircle),
                "Commissioning failed".to_string(),
                message,
                html! {},
            )
        } else {
            return html! {};
        };

        html! {
            <div class={classes!("progress-card", class)}>
                <span class="progress-card-icon">
                    { match glyph {
                        Some(glyph) => icon(glyph, 24),
                        None => html! { <span class="loader is-medium"></span> },
                    } }
                </span>
                <div class="progress-card-text">
                    <p class="progress-card-title">{ title }</p>
                    <p>{ text }</p>
                    if let Some(wire::Response::NetworkNeeded { kind }) = &self.result {
                        { self.view_network_needed(link, *kind) }
                    }
                    if self.running {
                        { self.view_progress_bar() }
                        { self.view_steps() }
                    } else if !self.steps.is_empty() {
                        // How far it got, open where it failed.
                        <details class="commission-details" open={class == "is-failed"}>
                            <summary>{ "Steps" }</summary>
                            { self.view_steps() }
                        </details>
                    }
                </div>
                { action }
            </div>
        }
    }

    /// The bar of the running commission, and how far it is in percent.
    fn view_progress_bar(&self) -> Html {
        let percent = self
            .steps
            .iter()
            .rev()
            .find_map(|line| display::commission_progress(line.step));

        html! {
            <div class="commission-bar">
                // Without a value, until the first step, the bar is indeterminate.
                <progress class={classes!("progress", "is-small",
                    if self.cancelling { "is-warning" } else { "is-link" })}
                    max="100" value={percent.map(|percent| percent.to_string())}>
                </progress>
                if let Some(percent) = percent {
                    <span class="commission-percent">{ format!("{percent}%") }</span>
                }
            </div>
        }
    }

    /// The steps the commission got to, each with how it went.
    fn view_steps(&self) -> Html {
        let end = if self.running {
            None
        } else {
            Some(match &self.result {
                Some(wire::Response::Commissioned(_))
                | Some(wire::Response::Cancelled {
                    decommissioned: true,
                }) => StepState::Done,
                Some(wire::Response::Cancelled { .. })
                | Some(wire::Response::NetworkNeeded { .. }) => StepState::Stopped,
                _ => StepState::Failed,
            })
        };
        let lines = step_lines(&self.steps, end, self.cancelling);
        if lines.is_empty() {
            return html! {};
        }

        html! {
            <ol class="commission-steps">
                { for lines.into_iter().map(|(step, attempts, state)| {
                    let label = display::commission_step_label(step);
                    let label = if attempts > 1 {
                        format!("{label} · attempt {attempts}")
                    } else {
                        label
                    };
                    let (class, mark) = match state {
                        StepState::Done => ("is-done", icon(Icon::Check, 14)),
                        StepState::Running => ("is-running", html! { <span class="loader"></span> }),
                        StepState::Failed => ("is-failed", icon(Icon::Close, 14)),
                        StepState::Stopped => ("is-stopped", icon(Icon::Stop, 14)),
                    };

                    html! {
                        <li class={classes!("commission-step", class)}>
                            <span class="commission-step-mark">{ mark }</span>
                            <span>{ label }</span>
                        </li>
                    }
                }) }
            </ol>
        }
    }

    /// Full-screen list of the commissionable devices: picking one on the
    /// network fills the peer address, one over Bluetooth the Bluetooth device.
    pub(crate) fn view_discover_modal(&self, link: &Scope<Self>) -> Html {
        if !self.discover_open {
            return html! {};
        }

        let rescan = html! {
            <button class={classes!("button", "is-small", self.discovering.then_some("is-loading"))}
                disabled={self.discovering || self.running}
                onclick={link.callback(|_| Msg::Discover)}>
                <span class="icon is-small">{ icon(Icon::Refresh, 16) }</span>
                <span>{ "Scan" }</span>
            </button>
        };

        html! {
            <div class="modal is-active">
                <div class="modal-background"></div>
                <div class="modal-card is-fullscreen">
                    { ui::modal_head("Discover", "Commissionable devices", rescan,
                        link.callback(|_| Msg::CloseDiscover)) }
                    <section class="modal-card-body">
                        <div class="app container">
                            if let Some(e) = &self.error {
                                { ui::notification(Icon::Alert, classes!("is-danger", "is-light"), html! { e.clone() },
                                    link.callback(|_| Msg::DismissError)) }
                            }
                            <div class="field">
                                <div class="control has-icons-left">
                                    <input class="input" type="search"
                                        placeholder="Discriminator or setup code, to look for one device"
                                        value={self.discover_filter.clone()}
                                        oninput={link.callback(|e: InputEvent| Msg::DiscoverFilter(input_value(e)))} />
                                    <span class="icon is-left">{ icon(Icon::Search, 18) }</span>
                                </div>
                            </div>
                            { self.view_discovered(link) }
                        </div>
                    </section>
                </div>
            </div>
        }
    }

    fn view_discovered(&self, link: &Scope<Self>) -> Html {
        if self.discovered.is_empty() && self.discovered_ble.is_empty() {
            if self.discovering {
                return html! {
                    <div class="scanning">
                        <div class="scanning-pulse">{ icon(Icon::Radar, 34) }</div>
                        <p class="empty-state-title">{ "Looking for devices…" }</p>
                        <p class="empty-state-text">{ "On the network and over Bluetooth." }</p>
                    </div>
                };
            }
            return ui::empty_state(
                Icon::Radar,
                "No commissionable devices",
                "Put the device into pairing mode, or open its commissioning window in the app \
                 it is in, then scan again.",
                html! {},
            );
        }

        let found = |glyph: Icon,
                     class: &'static str,
                     title: String,
                     sub: String,
                     meta: String,
                     onclick: Callback<MouseEvent>| {
            html! {
                <button class="found-card" {onclick}>
                    <span class={classes!("tile-icon", class)}>{ icon(glyph, 20) }</span>
                    <span class="found-text">
                        <span class="found-title">{ title }</span>
                        <span class="found-sub">{ sub }</span>
                        <span class="found-meta">{ meta }</span>
                    </span>
                    <span class="found-go">{ icon(Icon::ChevronRight, 20) }</span>
                </button>
            }
        };

        html! {
            <>
                if !self.discovered.is_empty() {
                    <p class="section-label">
                        { icon(Icon::Globe, 16) }
                        { format!("On the network · {}", self.discovered.len()) }
                    </p>
                    <div class="found-grid">
                        { for self.discovered.iter().map(|d| {
                            let addr = d.addr.clone();
                            found(Icon::Globe, "is-ethernet", "Commissionable device".to_string(),
                                d.addr.clone(), format!("Instance {}", display::hex_u64(d.commissionable_id)),
                                link.callback(move |_| Msg::PickDevice(addr.clone())))
                        }) }
                    </div>
                }
                if !self.discovered_ble.is_empty() {
                    <p class="section-label">
                        { icon(Icon::Bluetooth, 16) }
                        { format!("Over Bluetooth · {}", self.discovered_ble.len()) }
                    </p>
                    <div class="found-grid">
                        { for self.discovered_ble.iter().map(|d| {
                            let addr = d.addr.clone();
                            found(Icon::Bluetooth, "is-wifi",
                                display::product_label(d.vendor_id, d.product_id, None),
                                display::vendor_label(d.vendor_id, None),
                                format!("Discriminator {} · {}", d.discriminator, d.addr),
                                link.callback(move |_| Msg::PickBleDevice(addr.clone())))
                        }) }
                    </div>
                }
            </>
        }
    }
}

impl App {
    /// The camera popup: its video, until a QR code in it is read.
    pub(crate) fn view_camera_modal(&self, link: &Scope<Self>) -> Html {
        let Some(camera) = &self.camera else {
            return html! {};
        };
        // Closer, and a phone's camera no longer focuses.
        let hint = if camera.can_refocus {
            "Hold the code 10–20 cm from the camera, and tap the picture to focus on it."
        } else {
            "Hold the code 10–20 cm from the camera, where it looks sharp."
        };

        html! {
            <div class="modal is-active">
                <div class="modal-background" onclick={link.callback(|_| Msg::CloseCamera)}></div>
                <div class="modal-card camera-card">
                    <header class="modal-card-head">
                        <p class="modal-card-title">{ "Scan the QR code" }</p>
                        <button class="delete" aria-label="close"
                            onclick={link.callback(|_| Msg::CloseCamera)}></button>
                    </header>
                    <section class="modal-card-body">
                        <div class={classes!("camera-view", camera.can_refocus.then_some("can-refocus"))}
                            onclick={link.callback(|_| Msg::CameraRefocus)}>
                            <video ref={self.video_ref.clone()} class="camera-video"
                                autoplay=true muted=true playsinline=true></video>
                            if camera.stream.is_some() {
                                // Keyed by the refocus count, so that each replays its pulse.
                                <div key={camera.refocused}
                                    class={classes!("camera-frame", (camera.refocused > 0).then_some("is-refocusing"))}></div>
                            }
                            if let Some(e) = &camera.error {
                                <div class="camera-status">
                                    { icon(Icon::Alert, 28) }
                                    <span>{ e.clone() }</span>
                                </div>
                            } else if camera.stream.is_none() {
                                <div class="camera-status">
                                    <span class="loader is-medium"></span>
                                    <span>{ "Starting the camera…" }</span>
                                </div>
                            }
                        </div>
                        if let Some(zoom) = camera.zoom {
                            <div class="camera-control">
                                <span>{ "Zoom" }</span>
                                <input type="range" min={zoom.min.to_string()} max={zoom.max.to_string()}
                                    step={zoom.step.to_string()} value={zoom.value.to_string()}
                                    oninput={link.callback(|e: InputEvent| {
                                        Msg::CameraZoom(input_value(e).parse().unwrap_or(1.0))
                                    })} />
                                <span class="camera-control-value">{ format!("{:.1}×", zoom.value) }</span>
                            </div>
                        }
                        if let Some(focus) = camera.focus {
                            <div class="camera-control">
                                <span>{ "Focus" }</span>
                                <input type="range" min="0" max="1" step="0.005"
                                    value={focus.position().to_string()}
                                    oninput={link.callback(|e: InputEvent| {
                                        Msg::CameraFocus(input_value(e).parse().unwrap_or(1.0))
                                    })} />
                                <span class="camera-control-value">
                                    { if focus.manual { distance(focus.distance) } else { "Auto".into() } }
                                </span>
                            </div>
                        }
                        <p class="camera-hint">{ hint }</p>
                    </section>
                </div>
            </div>
        }
    }
}

/// `meters`, in words.
fn distance(meters: f64) -> String {
    if meters < 1.0 {
        format!("{:.0} cm", meters * 100.0)
    } else {
        format!("{meters:.1} m")
    }
}

/// The steps of a commission as `steps` reported them, in order: each once,
/// with how many times in a row it was attempted, and how it went. `end` is
/// how the commission ended, `None` while it runs; `cancelling`, that it was
/// asked to stop.
fn step_lines(
    steps: &[StepLine],
    end: Option<StepState>,
    cancelling: bool,
) -> Vec<(api::CommissionStep, usize, StepState)> {
    let mut lines: Vec<(api::CommissionStep, usize, StepState)> = Vec::new();
    for line in steps
        .iter()
        .filter(|line| line.step != api::CommissionStep::Commission)
    {
        match lines.last_mut() {
            Some((step, attempts, _)) if *step == line.step => *attempts += 1,
            _ => lines.push((line.step, 1, StepState::Done)),
        }
    }

    // A rollback follows the step the commission stopped at; without one, the
    // last step is where it ended.
    let stopped = match (end, cancelling) {
        (Some(end), _) => end,
        (None, true) => StepState::Stopped,
        (None, false) => StepState::Failed,
    };
    match lines
        .iter()
        .position(|(step, ..)| *step == api::CommissionStep::Cleanup)
    {
        Some(at) if at > 0 => lines[at - 1].2 = stopped,
        Some(_) => {}
        None => {
            if let (Some(end), Some(last)) = (end, lines.last_mut()) {
                last.2 = end;
            }
        }
    }

    if end.is_none() {
        if let Some(last) = lines.last_mut() {
            last.2 = StepState::Running;
        }
    }

    lines
}

/// A numbered part of the form.
fn step(number: u8, title: &str, body: Html) -> Html {
    html! {
        <section class="form-step">
            <span class="step-number">{ number }</span>
            <div class="step-body">
                <h3 class="step-title">{ title.to_string() }</h3>
                { body }
            </div>
        </section>
    }
}
