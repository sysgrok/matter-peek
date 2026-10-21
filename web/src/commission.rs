//! The Commission page: how to reach the device, its setup code, and how the
//! commission goes; and the popup discovering commissionable devices.

use std::net::SocketAddr;

use yew::html::Scope;
use yew::prelude::*;

use matter_peek_core::api::{self, display, wire};

use crate::icons::{icon, Icon};
use crate::ui;
use crate::{
    checkbox_checked, input_value, phase_name, picked_file, textarea_value, valid_bt_addr,
    valid_hex, valid_setup_code, App, Msg, Network, Tab, BLE_COMMISSION_TIMEOUT_SECS,
    COMMISSION_TIMEOUT_SECS,
};

impl App {
    pub(crate) fn view_commission_tab(&self, link: &Scope<Self>) -> Html {
        let code = self.setup_code.trim();
        let code_valid = valid_setup_code(&self.setup_code);
        let code_invalid = !code.is_empty() && !code_valid;

        let choice = |ble: bool, glyph: Icon, title: &str, text: &str| {
            html! {
                <button class={classes!("choice", (self.via_ble == ble).then_some("is-selected"))}
                    disabled={self.running}
                    onclick={link.callback(move |_| Msg::SetViaBle(ble))}>
                    <span class="choice-icon">{ icon(glyph, 22) }</span>
                    <span class="choice-title">{ title.to_string() }</span>
                    <span class="choice-text">{ text.to_string() }</span>
                </button>
            }
        };

        html! {
            <div class="narrow">
                { ui::page_header("Commission", html! { "Add a device to our fabric" }, html! {}) }
                { self.view_recommission_warning(link) }

                <div class="form-card">
                    { step(1, "How to reach it", html! {
                        <div class="choices">
                            { choice(false, Icon::Globe, "Over the network",
                                "It is on your network already, with its commissioning window open.") }
                            { choice(true, Icon::Bluetooth, "Over Bluetooth",
                                "A new device: it joins your Wi-Fi or Thread network as it is commissioned.") }
                        </div>
                    }) }

                    { step(2, "Which device", if self.via_ble {
                        self.view_ble_fields(link)
                    } else {
                        self.view_ip_fields(link)
                    }) }

                    { step(3, "Its setup code", html! {
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
                    }) }

                    { step(4, "Keep a record", html! {
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
                        if self.running {
                            { ui::icon_button(Icon::Close, "Cancel", classes!("is-danger", "is-light"), false,
                                None, link.callback(|_| Msg::Cancel)) }
                        }
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

    /// Commissioning over Bluetooth: which device, and the network it joins.
    fn view_ble_fields(&self, link: &Scope<Self>) -> Html {
        let addr = self.ble_addr.trim();
        let addr_invalid = !addr.is_empty() && !valid_bt_addr(addr);
        let dataset_invalid =
            !self.thread_dataset.trim().is_empty() && !valid_hex(&self.thread_dataset);

        let network = |network: Network, glyph: Icon, label: &str| {
            html! {
                <button class={classes!("segment", (self.network == network).then_some("is-active"))}
                    onclick={link.callback(move |_| Msg::SetNetwork(network))}>
                    { icon(glyph, 16) }
                    <span>{ label.to_string() }</span>
                </button>
            }
        };

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
                    <div class="segmented">
                        { network(Network::Wifi, Icon::Wifi, "Wi-Fi") }
                        { network(Network::Thread, Icon::Thread, "Thread") }
                        { network(Network::None, Icon::Ethernet, "None: it is on one") }
                    </div>
                </div>

                {
                    match self.network {
                        Network::Wifi => html! {
                            <div class="columns is-variable is-3 mb-0">
                                <div class="column">
                                    <div class="control has-icons-left">
                                        <input class="input" placeholder="Network name (SSID)"
                                            value={self.wifi_ssid.clone()}
                                            oninput={link.callback(|e: InputEvent| Msg::WifiSsid(input_value(e)))} />
                                        <span class="icon is-left">{ icon(Icon::Wifi, 18) }</span>
                                    </div>
                                </div>
                                <div class="column">
                                    <div class="field has-addons">
                                        <div class="control has-icons-left is-expanded">
                                            <input class="input" placeholder="Password"
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
                        },
                        Network::Thread => html! {
                            <div class="field">
                                <textarea class={classes!("textarea", "mono", dataset_invalid.then_some("is-danger"))} rows="3"
                                    placeholder="Operational dataset, in hex"
                                    value={self.thread_dataset.clone()}
                                    oninput={link.callback(|e: InputEvent| Msg::ThreadDataset(textarea_value(e)))} />
                                <p class="help">{ "As `ot-ctl dataset active -x` prints it." }</p>
                            </div>
                        },
                        Network::None => html! {},
                    }
                }
            </>
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

    /// How the commission goes, or how it went.
    fn view_progress(&self, link: &Scope<Self>) -> Html {
        let failed = self.steps.last().and_then(|line| match &line.status {
            api::StepStatus::Failed { message } => {
                Some(format!("{} failed: {message}", phase_name(line.step)))
            }
            _ => None,
        });

        let (class, glyph, title, text, action) = if self.running {
            let secs = if self.via_ble {
                BLE_COMMISSION_TIMEOUT_SECS
            } else {
                COMMISSION_TIMEOUT_SECS
            };
            (
                "is-running",
                None,
                "Commissioning…".to_string(),
                format!("This takes up to {secs} s; Cancel stops it."),
                html! {},
            )
        } else if let Some(wire::Response::Commissioned(result)) = &self.result {
            (
                "is-done",
                Some(Icon::CheckCircle),
                "Commissioned".to_string(),
                format!(
                    "The device is on our fabric as node {}.",
                    display::hex_u64(result.handle.node_id)
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
                    if self.running {
                        <progress class="progress is-small is-link mt-2" max="100"></progress>
                    }
                </div>
                { action }
            </div>
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
                        <div class="camera-view">
                            <video ref={self.video_ref.clone()} class="camera-video"
                                autoplay=true muted=true playsinline=true></video>
                            if camera.stream.is_some() {
                                <div class="camera-frame"></div>
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
                        <p class="camera-hint">
                            { "Hold the QR code on the device or its box inside the frame." }
                        </p>
                    </section>
                </div>
            </div>
        }
    }
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
