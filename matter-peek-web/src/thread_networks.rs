//! Thread networks, in a popup of the Thread page: getting one, with its keys,
//! from a border router that shares it with a one-time code, from a Matter
//! border router, from the phone (Android) or as a dataset; and the saved ones,
//! to show or give to a Matter border router.

use wasm_bindgen_futures::spawn_local;
use yew::html::Scope;
use yew::prelude::*;

use matter_peek_core::api::{
    self, wire, NetworkKind, SavedNetwork, SharingBorderRouter, ThreadDataset, ThreadSource,
    ThreadTarget,
};

use crate::icons::{icon, Icon};
use crate::{play, request_loop, select_value, ui, App, Msg};

/// Thread Border Router Management and Thread Network Directory.
const TBRM: u32 = 0x0452;
const DIRECTORY: u32 = 0x0453;

/// The modules around a QR code that are left light.
const QR_QUIET: usize = 4;

/// Where a network is got from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Code,
    Device,
    Phone,
    Dataset,
}

/// The Thread networks popup.
pub struct ThreadNetworksView {
    pub source: Source,
    pub code: String,
    /// The border routers sharing now; `None` before looking.
    pub routers: Option<Vec<SharingBorderRouter>>,
    pub looking: bool,
    /// The one chosen, by its address.
    pub router: Option<String>,
    pub device: Option<u64>,
    pub dataset: String,
    pub busy: bool,
    pub error: Option<String>,
    /// What the last action did.
    pub done: Option<String>,
    /// The network whose dataset may be shown, once the warning is accepted.
    pub reveal: Option<String>,
    pub shown: Option<ThreadDataset>,
    pub copied: bool,
    /// The network being given.
    pub give: Option<Give>,
}

/// Giving a saved network to a device.
pub struct Give {
    pub id: String,
    pub node_id: Option<u64>,
    pub directory: bool,
    pub preferred: bool,
}

/// What happens in the popup.
pub enum ThreadNetworksMsg {
    Open,
    Close,
    Source(Source),
    Code(String),
    LookForRouters,
    Router(String),
    Device(u64),
    Dataset(String),
    Get,
    /// The phone's preferred Thread network: its dataset, if the user let us.
    PhoneGot(Result<Option<String>, String>),
    Reveal(String),
    Show,
    Hide,
    Copy,
    Give(String),
    GiveNode(u64),
    GiveDirectory(bool),
    GivePreferred(bool),
    GiveSend,
    GiveCancel,
    Forget(String),
    Loaded(wire::Response),
    Failed(String),
}

impl App {
    pub(crate) fn update_thread_networks(
        &mut self,
        ctx: &Context<Self>,
        msg: ThreadNetworksMsg,
    ) -> bool {
        let link = ctx.link();

        if let ThreadNetworksMsg::Open = msg {
            let source = if self.can_play {
                Source::Phone
            } else {
                Source::Code
            };
            self.thread_networks = Some(ThreadNetworksView {
                source,
                code: String::new(),
                routers: None,
                looking: source == Source::Code,
                router: None,
                device: None,
                dataset: String::new(),
                busy: false,
                error: None,
                done: None,
                reveal: None,
                shown: None,
                copied: false,
                give: None,
            });
            send(link, wire::Request::SavedNetworks);
            send(link, wire::Request::Capabilities);
            if source == Source::Code {
                send(link, wire::Request::SharingBorderRouters);
            }
            return true;
        }

        let Some(view) = self.thread_networks.as_mut() else {
            return false;
        };

        match msg {
            ThreadNetworksMsg::Open => unreachable!("done above"),
            ThreadNetworksMsg::Close => self.thread_networks = None,
            ThreadNetworksMsg::Source(source) => {
                view.source = source;
                view.error = None;
                if source == Source::Code && view.routers.is_none() {
                    view.looking = true;
                    send(link, wire::Request::SharingBorderRouters);
                }
            }
            ThreadNetworksMsg::Code(code) => view.code = code,
            ThreadNetworksMsg::LookForRouters => {
                view.looking = true;
                send(link, wire::Request::SharingBorderRouters);
            }
            ThreadNetworksMsg::Router(addr) => view.router = Some(addr).filter(|a| !a.is_empty()),
            ThreadNetworksMsg::Device(node_id) => view.device = Some(node_id),
            ThreadNetworksMsg::Dataset(dataset) => view.dataset = dataset,
            ThreadNetworksMsg::Get => {
                let source = match view.source {
                    Source::Code => ThreadSource::SharingCode {
                        code: view.code.trim().into(),
                        addr: view.router.clone(),
                    },
                    Source::Device => match view.device {
                        Some(node_id) => ThreadSource::Device {
                            node_id,
                            extended_pan_id: None,
                        },
                        None => return false,
                    },
                    Source::Dataset => ThreadSource::Dataset {
                        dataset: view.dataset.trim().into(),
                    },
                    Source::Phone => {
                        view.busy = true;
                        view.error = None;
                        view.done = None;
                        let link = link.clone();
                        spawn_local(async move {
                            let got = play::thread_network().await;
                            link.send_message(msg_of(ThreadNetworksMsg::PhoneGot(got)));
                        });
                        return true;
                    }
                };
                view.busy = true;
                view.error = None;
                view.done = None;
                send(link, wire::Request::GetThreadNetwork { source });
            }
            ThreadNetworksMsg::PhoneGot(Ok(Some(dataset))) => {
                send(
                    link,
                    wire::Request::GetThreadNetwork {
                        source: ThreadSource::Dataset { dataset },
                    },
                );
            }
            ThreadNetworksMsg::PhoneGot(Ok(None)) => {
                view.busy = false;
                view.done = Some("The phone did not give a Thread network.".into());
            }
            ThreadNetworksMsg::PhoneGot(Err(e)) => {
                view.busy = false;
                view.error = Some(e);
            }
            ThreadNetworksMsg::Reveal(id) => {
                view.reveal = Some(id);
                view.shown = None;
            }
            ThreadNetworksMsg::Show => {
                if let Some(id) = view.reveal.take() {
                    view.busy = true;
                    view.copied = false;
                    send(link, wire::Request::ThreadDataset { id });
                }
            }
            ThreadNetworksMsg::Hide => {
                view.reveal = None;
                view.shown = None;
            }
            ThreadNetworksMsg::Copy => {
                if let (Some(shown), Some(window)) = (&view.shown, web_sys::window()) {
                    let _ = window.navigator().clipboard().write_text(&shown.dataset);
                    view.copied = true;
                }
            }
            ThreadNetworksMsg::Give(id) => {
                view.give = Some(Give {
                    id,
                    node_id: None,
                    directory: false,
                    preferred: false,
                });
                view.error = None;
                view.done = None;
            }
            ThreadNetworksMsg::GiveNode(node_id) => {
                if let Some(give) = view.give.as_mut() {
                    give.node_id = Some(node_id);
                }
            }
            ThreadNetworksMsg::GiveDirectory(directory) => {
                if let Some(give) = view.give.as_mut() {
                    give.directory = directory;
                }
            }
            ThreadNetworksMsg::GivePreferred(preferred) => {
                if let Some(give) = view.give.as_mut() {
                    give.preferred = preferred;
                }
            }
            ThreadNetworksMsg::GiveSend => {
                let Some(Give {
                    id,
                    node_id: Some(node_id),
                    directory,
                    preferred,
                }) = view.give.as_ref()
                else {
                    return false;
                };
                let to = if *directory {
                    ThreadTarget::Directory {
                        preferred: *preferred,
                    }
                } else {
                    ThreadTarget::BorderRouter
                };
                view.busy = true;
                view.error = None;
                send(
                    link,
                    wire::Request::GiveThreadNetwork {
                        node_id: *node_id,
                        id: id.clone(),
                        to,
                    },
                );
            }
            ThreadNetworksMsg::GiveCancel => view.give = None,
            ThreadNetworksMsg::Forget(id) => {
                view.shown = None;
                view.reveal = None;
                send(link, wire::Request::ForgetNetwork { id });
            }
            ThreadNetworksMsg::Loaded(response) => match response {
                wire::Response::SavedNetworks { networks } => self.saved_networks = networks,
                wire::Response::Capabilities { capabilities } => {
                    self.can_share_codes = capabilities.sharing_codes;
                    if !capabilities.sharing_codes && view.source == Source::Code {
                        view.source = Source::Device;
                    }
                }
                wire::Response::SharingBorderRouters { routers } => {
                    view.looking = false;
                    if view
                        .router
                        .as_ref()
                        .is_some_and(|addr| !routers.iter().any(|r| &r.addr == addr))
                    {
                        view.router = None;
                    }
                    view.routers = Some(routers);
                }
                wire::Response::ThreadNetwork { network } => {
                    view.busy = false;
                    view.code.clear();
                    view.dataset.clear();
                    view.done = Some(format!(
                        "Saved the Thread network {}. The next Thread device commissioned over Bluetooth gets it.",
                        network.name
                    ));
                    send(link, wire::Request::SavedNetworks);
                }
                wire::Response::ThreadDataset(shown) => {
                    view.busy = false;
                    view.shown = Some(shown);
                }
                wire::Response::Ok => {
                    view.busy = false;
                    if let Some(give) = view.give.take() {
                        let device = give
                            .node_id
                            .map(|id| self.device_title(id))
                            .unwrap_or_default();
                        let network = network_name(&self.saved_networks, &give.id);
                        let done = if give.directory {
                            format!("{device} has {network} in its Thread Network Directory now.")
                        } else {
                            format!("{device} took {network} as its Thread network.")
                        };
                        if let Some(view) = self.thread_networks.as_mut() {
                            view.done = Some(done);
                        }
                    }
                }
                wire::Response::Error { message } => {
                    view.busy = false;
                    view.looking = false;
                    view.error = Some(message);
                }
                _ => view.busy = false,
            },
            ThreadNetworksMsg::Failed(message) => {
                view.busy = false;
                view.looking = false;
                view.error = Some(message);
            }
        }

        true
    }

    /// The popup with the saved Thread networks, and the ways to get one.
    pub(crate) fn view_thread_networks_modal(&self, link: &Scope<Self>) -> Html {
        let Some(view) = &self.thread_networks else {
            return html! {};
        };
        let networks: Vec<&SavedNetwork> = self
            .saved_networks
            .iter()
            .filter(|network| network.kind == NetworkKind::Thread)
            .collect();
        let close = link.callback(|_| msg_of(ThreadNetworksMsg::Close));

        html! {
            <div class="modal is-active">
                <div class="modal-background" onclick={close.clone()}></div>
                <div class="modal-card share-card tnet-card">
                    { ui::modal_head("Thread", "Thread networks", html! {}, close) }
                    <section class="modal-card-body">
                        if let Some(error) = &view.error {
                            <p class="share-error">{ icon(Icon::Alert, 16) }<span>{ error.clone() }</span></p>
                        }
                        if let Some(done) = &view.done {
                            <p class="share-warning">{ icon(Icon::CheckCircle, 16) }<span>{ done.clone() }</span></p>
                        }
                        <p class="thread-section-title">{ "Saved" }</p>
                        if networks.is_empty() {
                            <p class="share-hint">{ "No Thread network saved yet. Get one below." }</p>
                        }
                        <div class="tnet-list">
                            { for networks.iter().map(|network| self.view_saved_network(view, network, link)) }
                        </div>
                        <p class="thread-section-title">{ "Get a network" }</p>
                        { self.view_sources(view, link) }
                    </section>
                </div>
            </div>
        }
    }

    fn view_saved_network(
        &self,
        view: &ThreadNetworksView,
        network: &SavedNetwork,
        link: &Scope<Self>,
    ) -> Html {
        let id = network.id.clone();
        let (reveal, give, forget) = (id.clone(), id.clone(), id.clone());
        let shown = view.shown.as_ref().filter(|shown| shown.id == network.id);
        let asked = view.reveal.as_deref() == Some(network.id.as_str());
        let giving = view.give.as_ref().filter(|give| give.id == network.id);

        html! {
            <div class="thread-network">
                <div class="thread-network-head">
                    <span class="tile-icon is-thread">{ icon(Icon::Thread, 20) }</span>
                    <div class="thread-network-title">
                        <span class="thread-network-name">{ network.name.clone() }</span>
                        <span class="thread-network-id mono">{ details(network) }</span>
                    </div>
                </div>
                <div class="share-actions">
                    if shown.is_some() {
                        <button class="button is-small is-soft" onclick={link.callback(|_| msg_of(ThreadNetworksMsg::Hide))}>
                            <span class="icon is-small">{ icon(Icon::EyeOff, 16) }</span><span>{ "Hide" }</span>
                        </button>
                    } else {
                        <button class="button is-small is-soft" title="Show its dataset, with the network key"
                            onclick={link.callback(move |_| msg_of(ThreadNetworksMsg::Reveal(reveal.clone())))}>
                            <span class="icon is-small">{ icon(Icon::Eye, 16) }</span><span>{ "Show" }</span>
                        </button>
                    }
                    <button class="button is-small is-soft" title="Give it to a Matter border router"
                        onclick={link.callback(move |_| msg_of(ThreadNetworksMsg::Give(give.clone())))}>
                        <span class="icon is-small">{ icon(Icon::Router, 16) }</span><span>{ "Give to…" }</span>
                    </button>
                    <button class="button is-small is-danger is-soft" title="Forget it, with its keys"
                        onclick={link.callback(move |_| msg_of(ThreadNetworksMsg::Forget(forget.clone())))}>
                        <span class="icon is-small">{ icon(Icon::Trash, 16) }</span><span>{ "Forget" }</span>
                    </button>
                </div>
                if asked {
                    <div class="share-warning tnet-reveal">
                        { icon(Icon::Lock, 16) }
                        <div>
                            <p>{ "The dataset holds the network key: whoever has it can join this Thread network, and see and send what goes over it. Show it only to give it to a device or app you trust, and not where others can see the screen." }</p>
                            <div class="share-actions">
                                <button class={classes!("button", "is-small", "is-link", view.busy.then_some("is-loading"))}
                                    onclick={link.callback(|_| msg_of(ThreadNetworksMsg::Show))}>{ "Show it" }</button>
                                <button class="button is-small is-soft" onclick={link.callback(|_| msg_of(ThreadNetworksMsg::Hide))}>{ "Cancel" }</button>
                            </div>
                        </div>
                    </div>
                }
                if let Some(shown) = shown {
                    <div class="share-open tnet-shown">
                        if !shown.qr_rows.is_empty() {
                            <div class="share-qr">{ qr_svg(&shown.qr_rows) }</div>
                        }
                        <div class="share-details">
                            <p class="share-label">{ "Operational dataset" }</p>
                            <p class="share-qr-text mono">{ shown.dataset.clone() }</p>
                            <div class="share-actions">
                                <button class="button is-small is-soft" onclick={link.callback(|_| msg_of(ThreadNetworksMsg::Copy))}>
                                    <span class="icon is-small">{ icon(if view.copied { Icon::Check } else { Icon::Copy }, 16) }</span>
                                    <span>{ if view.copied { "Copied" } else { "Copy" } }</span>
                                </button>
                            </div>
                            <p class="share-hint">{ "Paste it where a Thread border router or app takes a dataset in hex (e.g. `ot-ctl dataset set active`), or scan the QR code." }</p>
                        </div>
                    </div>
                }
                if let Some(give) = giving {
                    { self.view_give(view, give, link) }
                }
            </div>
        }
    }

    fn view_give(&self, view: &ThreadNetworksView, give: &Give, link: &Scope<Self>) -> Html {
        let cluster = if give.directory { DIRECTORY } else { TBRM };
        let devices: Vec<&api::DeviceInfo> = self
            .stored
            .iter()
            .filter(|device| device.details.clusters.contains(&cluster))
            .collect();

        html! {
            <div class="tnet-form">
                <div class="choices">
                    <button class={classes!("choice", (!give.directory).then_some("is-selected"))}
                        onclick={link.callback(|_| msg_of(ThreadNetworksMsg::GiveDirectory(false)))}>
                        <span class="choice-icon">{ icon(Icon::Router, 22) }</span>
                        <span class="choice-title">{ "As its network" }</span>
                        <span class="choice-text">{ "A Matter Thread border router that has no network yet forms or joins this one." }</span>
                    </button>
                    <button class={classes!("choice", give.directory.then_some("is-selected"))}
                        onclick={link.callback(|_| msg_of(ThreadNetworksMsg::GiveDirectory(true)))}>
                        <span class="choice-icon">{ icon(Icon::Table, 22) }</span>
                        <span class="choice-title">{ "Into its directory" }</span>
                        <span class="choice-text">{ "Kept in the device's Thread Network Directory, for other controllers to find." }</span>
                    </button>
                </div>
                if devices.is_empty() {
                    <p class="share-hint">{ if give.directory {
                        "None of the listed devices has a Thread Network Directory."
                    } else {
                        "None of the listed devices is a Matter Thread border router."
                    } }</p>
                } else {
                    <div class="select is-small tnet-select">
                        <select onchange={link.callback(|e: Event| msg_of(
                            ThreadNetworksMsg::GiveNode(select_value(e).parse().unwrap_or_default())))}>
                            <option value="" selected={give.node_id.is_none()} disabled=true>{ "Pick a device" }</option>
                            { for devices.iter().map(|device| {
                                let node_id = device.handle.node_id;
                                html! {
                                    <option value={node_id.to_string()} selected={give.node_id == Some(node_id)}>
                                        { self.device_title(node_id) }
                                    </option>
                                }
                            }) }
                        </select>
                    </div>
                }
                if give.directory {
                    <label class="checkbox tnet-check">
                        <input type="checkbox" checked={give.preferred}
                            onchange={link.callback(|e: Event| msg_of(ThreadNetworksMsg::GivePreferred(crate::checkbox_checked(e))))} />
                        { " As the preferred network" }
                    </label>
                }
                <div class="share-actions">
                    <button class={classes!("button", "is-small", "is-link", view.busy.then_some("is-loading"))}
                        disabled={give.node_id.is_none() || view.busy}
                        onclick={link.callback(|_| msg_of(ThreadNetworksMsg::GiveSend))}>{ "Give" }</button>
                    <button class="button is-small is-soft" onclick={link.callback(|_| msg_of(ThreadNetworksMsg::GiveCancel))}>{ "Cancel" }</button>
                </div>
            </div>
        }
    }

    fn view_sources(&self, view: &ThreadNetworksView, link: &Scope<Self>) -> Html {
        let choice = |source: Source, glyph: Icon, title: &str, text: &str| {
            html! {
                <button class={classes!("choice", (view.source == source).then_some("is-selected"))}
                    onclick={link.callback(move |_| msg_of(ThreadNetworksMsg::Source(source)))}>
                    <span class="choice-icon">{ icon(glyph, 22) }</span>
                    <span class="choice-title">{ title.to_string() }</span>
                    <span class="choice-text">{ text.to_string() }</span>
                </button>
            }
        };
        let routers: Vec<&api::DeviceInfo> = self
            .stored
            .iter()
            .filter(|d| {
                d.details
                    .clusters
                    .iter()
                    .any(|c| *c == TBRM || *c == DIRECTORY)
            })
            .collect();
        let get = |label: &str, disabled: bool| {
            html! {
                <div class="share-actions">
                    <button class={classes!("button", "is-link", view.busy.then_some("is-loading"))}
                        disabled={disabled || view.busy}
                        onclick={link.callback(|_| msg_of(ThreadNetworksMsg::Get))}>
                        <span class="icon">{ icon(Icon::Download, 18) }</span><span>{ label.to_string() }</span>
                    </button>
                </div>
            }
        };

        html! {
            <>
                <div class="choices">
                    if self.can_play {
                        { choice(Source::Phone, Icon::Key, "From this phone",
                            "The Thread network Google Home and other apps keep on this phone, once you allow it.") }
                    }
                    if self.can_share_codes {
                        { choice(Source::Code, Icon::Share, "With a sharing code",
                            "A Thread 1.4 border router shares its network with a one-time code it shows.") }
                    }
                    { choice(Source::Device, Icon::Router, "From a Matter border router",
                        "A listed Matter Thread border router gives its own network.") }
                    { choice(Source::Dataset, Icon::Braces, "Paste a dataset",
                        "An operational dataset in hex, e.g. from `ot-ctl dataset active -x`.") }
                </div>
                <div class="tnet-form">
                    { match view.source {
                        Source::Phone => html! {
                            <>
                                <p class="share-hint">{ "Android asks whether matter-peek may have the phone's preferred Thread network: the one Google Home made, or set." }</p>
                                { get("Get the phone's network", false) }
                            </>
                        },
                        Source::Code => html! {
                            <>
                                <p class="share-hint">{ "On the border router that has the network (its app, its page, or e.g. \"Share Thread network credentials\" on a Google TV Streamer), ask it to share its network. Enter the code it shows within its few minutes: a mistyped code costs one of its few attempts." }</p>
                                <input class="input mono tnet-code" type="text" inputmode="numeric" autocomplete="off"
                                    placeholder="123 456 789" value={view.code.clone()}
                                    oninput={link.callback(|e: InputEvent| msg_of(ThreadNetworksMsg::Code(crate::input_value(e))))} />
                                <div class="tnet-routers">
                                    { match &view.routers {
                                        None => html! {},
                                        Some(routers) if routers.is_empty() => html! {
                                            <span class="share-hint">{ "No border router shares its network now." }</span>
                                        },
                                        Some(routers) => html! {
                                            <div class="select is-small tnet-select">
                                                <select onchange={link.callback(|e: Event| msg_of(ThreadNetworksMsg::Router(select_value(e))))}>
                                                    if routers.len() > 1 {
                                                        <option value="" selected={view.router.is_none()}>{ "Pick the border router" }</option>
                                                    }
                                                    { for routers.iter().map(|router| html! {
                                                        <option value={router.addr.clone()}
                                                            selected={view.router.as_deref() == Some(router.addr.as_str())}>
                                                            { format!("{} ({})", router.name, router.addr) }
                                                        </option>
                                                    }) }
                                                </select>
                                            </div>
                                        },
                                    } }
                                    <button class={classes!("button", "is-small", "is-soft", view.looking.then_some("is-loading"))}
                                        title="Look for the border routers sharing their network now"
                                        disabled={view.looking} onclick={link.callback(|_| msg_of(ThreadNetworksMsg::LookForRouters))}>
                                        <span class="icon is-small">{ icon(Icon::Refresh, 16) }</span><span>{ "Look again" }</span>
                                    </button>
                                </div>
                                { get("Get the network", view.code.trim().is_empty()) }
                            </>
                        },
                        Source::Device => html! {
                            <>
                                if routers.is_empty() {
                                    <p class="share-hint">{ "None of the listed devices is a Matter Thread border router." }</p>
                                } else {
                                    <div class="select tnet-select">
                                        <select onchange={link.callback(|e: Event| msg_of(
                                            ThreadNetworksMsg::Device(select_value(e).parse().unwrap_or_default())))}>
                                            <option value="" selected={view.device.is_none()} disabled=true>{ "Pick a border router" }</option>
                                            { for routers.iter().map(|device| {
                                                let node_id = device.handle.node_id;
                                                html! {
                                                    <option value={node_id.to_string()} selected={view.device == Some(node_id)}>
                                                        { self.device_title(node_id) }
                                                    </option>
                                                }
                                            }) }
                                        </select>
                                    </div>
                                }
                                { get("Get its network", view.device.is_none()) }
                            </>
                        },
                        Source::Dataset => html! {
                            <>
                                <textarea class="textarea mono" rows="3" placeholder="0e08000000000001000000030000…"
                                    value={view.dataset.clone()}
                                    oninput={link.callback(|e: InputEvent| msg_of(ThreadNetworksMsg::Dataset(crate::textarea_value(e))))} />
                                { get("Save it", view.dataset.trim().is_empty()) }
                            </>
                        },
                    } }
                </div>
            </>
        }
    }
}

/// A saved network's id, channel and PAN ID.
fn details(network: &SavedNetwork) -> String {
    let mut parts = vec![network.id.clone()];
    if let Some(channel) = network.channel {
        parts.push(format!("channel {channel}"));
    }
    if let Some(pan_id) = network.pan_id {
        parts.push(format!("PAN ID 0x{pan_id:04X}"));
    }
    parts.join(" · ")
}

fn network_name(networks: &[SavedNetwork], id: &str) -> String {
    networks
        .iter()
        .find(|network| network.id == id)
        .map(|network| network.name.clone())
        .unwrap_or_else(|| id.into())
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

fn msg_of(msg: ThreadNetworksMsg) -> Msg {
    Msg::ThreadNetworks(msg)
}

/// Send `request`; the answer goes to the popup.
fn send(link: &Scope<App>, request: wire::Request) {
    request_loop(
        link.clone(),
        request,
        |response| msg_of(ThreadNetworksMsg::Loaded(response)),
        |e| msg_of(ThreadNetworksMsg::Failed(e)),
    );
}
