//! Our fabrics: the one this page shows (each page of the app works in it),
//! the switcher in the top bar, and the popup that manages them.

use yew::html::Scope;
use yew::prelude::*;

use matter_peek_core::api::{display, wire, DeviceInfo, FabricDeletion, FabricInfo, FabricRef};

use crate::icons::{icon, Icon};
use crate::{request_loop, ui, App, Ask, Msg};

/// Where the page remembers the fabric it shows.
const VISIBLE_KEY: &str = "matter-peek.fabric";

/// The fabrics popup.
#[derive(Default)]
pub struct ManageView {
    pub new_name: String,
    /// The fabric being renamed, and its new name.
    pub renaming: Option<(u8, String)>,
    /// The fabric whose deletion is being chosen.
    pub deleting: Option<u8>,
    pub busy: bool,
    pub error: Option<String>,
}

/// What happens with our fabrics.
pub enum FabricsMsg {
    Load,
    Loaded(wire::Response),
    Failed(String),
    /// Show this fabric.
    Show(u8),
    ToggleMenu,
    OpenManage,
    CloseManage,
    NewName(String),
    Create,
    StartRename(u8),
    RenameText(String),
    Rename,
    CancelRename,
    SetDefault(u8),
    /// Delete it: asked, or chosen what becomes of its devices first.
    Delete(u8),
    CancelDelete,
    /// Deletion confirmed.
    Deleted(u8, FabricDeletion),
}

impl App {
    pub(crate) fn update_fabrics_list(&mut self, ctx: &Context<Self>, msg: FabricsMsg) -> bool {
        let link = ctx.link();

        match msg {
            FabricsMsg::Load => send(link, wire::Request::Fabrics),
            FabricsMsg::Loaded(wire::Response::Fabrics { fabrics }) => {
                let deleted = self.fabrics.len() > fabrics.len();
                self.fabrics = fabrics;
                if let Some(view) = self.manage.as_mut() {
                    view.busy = false;
                    view.error = None;
                    view.renaming = None;
                    view.deleting = None;
                    view.new_name.clear();
                }
                self.show_devices(ctx);
                // Its devices went with a deleted fabric.
                if deleted {
                    link.send_message(Msg::ListDevices);
                }
            }
            FabricsMsg::Loaded(wire::Response::Error { message }) | FabricsMsg::Failed(message) => {
                match self.manage.as_mut() {
                    Some(view) => {
                        view.busy = false;
                        view.error = Some(message);
                    }
                    None => self.error = Some(message),
                }
            }
            FabricsMsg::Loaded(_) => return false,
            FabricsMsg::Show(index) => {
                self.fabric_menu = false;
                self.drawer_open = false;
                if self.visible_fabric().map(|fabric| fabric.index) != Some(index) {
                    self.fabric = Some(index);
                    remember_visible(index);
                    // The Thread page read another fabric's devices.
                    self.thread = Default::default();
                    self.show_devices(ctx);
                }
            }
            FabricsMsg::ToggleMenu => self.fabric_menu = !self.fabric_menu,
            FabricsMsg::OpenManage => {
                self.fabric_menu = false;
                self.drawer_open = false;
                self.manage = Some(ManageView::default());
                send(link, wire::Request::Fabrics);
            }
            FabricsMsg::CloseManage => self.manage = None,
            msg => {
                let Some(view) = self.manage.as_mut() else {
                    return false;
                };
                let request = match msg {
                    FabricsMsg::NewName(name) => {
                        view.new_name = name;
                        None
                    }
                    FabricsMsg::Create => Some(wire::Request::CreateFabric {
                        name: view.new_name.clone(),
                    }),
                    FabricsMsg::StartRename(index) => {
                        let name = self
                            .fabrics
                            .iter()
                            .find(|fabric| fabric.index == index)
                            .map(|fabric| fabric.name.clone())
                            .unwrap_or_default();
                        view.renaming = Some((index, name));
                        None
                    }
                    FabricsMsg::RenameText(name) => {
                        if let Some((_, text)) = view.renaming.as_mut() {
                            *text = name;
                        }
                        None
                    }
                    FabricsMsg::Rename => {
                        view.renaming
                            .clone()
                            .map(|(index, name)| wire::Request::RenameFabric {
                                fabric: FabricRef::Index { index },
                                name,
                            })
                    }
                    FabricsMsg::CancelRename => {
                        view.renaming = None;
                        None
                    }
                    FabricsMsg::SetDefault(index) => Some(wire::Request::SetDefaultFabric {
                        fabric: FabricRef::Index { index },
                    }),
                    FabricsMsg::Delete(index) => {
                        let fabric = self.fabrics.iter().find(|fabric| fabric.index == index);
                        match fabric {
                            Some(fabric) if fabric.devices == 0 => {
                                self.ask = Some(Ask::DeleteFabric(index, FabricDeletion::Refuse));
                            }
                            Some(_) => view.deleting = Some(index),
                            None => {}
                        }
                        None
                    }
                    FabricsMsg::CancelDelete => {
                        view.deleting = None;
                        None
                    }
                    FabricsMsg::Deleted(index, deletion) => Some(wire::Request::DeleteFabric {
                        fabric: FabricRef::Index { index },
                        deletion,
                    }),
                    _ => None,
                };

                if let Some(request) = request {
                    view.busy = true;
                    view.error = None;
                    send(link, request);
                }
            }
        }

        true
    }

    /// The fabric this page shows: the one picked, else the default; `None`
    /// until our fabrics are read.
    pub(crate) fn visible_fabric(&self) -> Option<&FabricInfo> {
        self.fabric
            .and_then(|index| self.fabrics.iter().find(|fabric| fabric.index == index))
            .or_else(|| self.fabrics.iter().find(|fabric| fabric.default))
    }

    /// The fabric a request of this page is about.
    pub(crate) fn fabric_ref(&self) -> FabricRef {
        match self.visible_fabric() {
            Some(fabric) => FabricRef::Index {
                index: fabric.index,
            },
            None => FabricRef::Default,
        }
    }

    /// Take the device list of all our fabrics, and show those of the
    /// visible one.
    pub(crate) fn set_devices(&mut self, ctx: &Context<Self>, devices: Vec<DeviceInfo>) {
        self.all_devices = devices;
        self.show_devices(ctx);
    }

    /// Show the devices of the visible fabric, and follow them.
    fn show_devices(&mut self, ctx: &Context<Self>) {
        let visible = self.visible_fabric().map(|fabric| fabric.index);

        self.stored = self
            .all_devices
            .iter()
            .filter(|d| visible.is_none_or(|index| d.handle.fabric_index == index))
            .cloned()
            .collect();
        self.sync_subscriptions(ctx);
    }

    /// The switcher in the top bar: the fabric shown, and the menu of ours.
    pub(crate) fn view_fabric_switcher(&self, link: &Scope<Self>) -> Html {
        let Some(visible) = self.visible_fabric() else {
            return html! {};
        };

        html! {
            <div class="fabric-switcher">
                <button class={classes!("fabric-switch", self.fabric_menu.then_some("is-open"))}
                    title="The fabric shown: switch, or manage our fabrics"
                    onclick={link.callback(|_| fabrics(FabricsMsg::ToggleMenu))}>
                    { dot(visible.color) }
                    <span class="fabric-switch-name">{ visible.name.clone() }</span>
                    { icon(Icon::ChevronDown, 14) }
                </button>
                if self.fabric_menu {
                    <div class="fabric-menu-backdrop" onclick={link.callback(|_| fabrics(FabricsMsg::ToggleMenu))}></div>
                    <div class="fabric-menu">
                        { self.view_fabric_choices(link) }
                    </div>
                }
            </div>
        }
    }

    /// Our fabrics to pick from, and the way to manage them.
    pub(crate) fn view_fabric_choices(&self, link: &Scope<Self>) -> Html {
        let visible = self.visible_fabric().map(|fabric| fabric.index);

        html! {
            <>
                <p class="fabric-menu-title">{ "Fabrics" }</p>
                { for self.fabrics.iter().map(|fabric| {
                    let index = fabric.index;
                    html! {
                        <a class={classes!("fabric-choice", (visible == Some(index)).then_some("is-active"))}
                            onclick={link.callback(move |_| fabrics(FabricsMsg::Show(index)))}>
                            { dot(fabric.color) }
                            <span class="fabric-choice-name">{ fabric.name.clone() }</span>
                            if fabric.default {
                                <span class="fabric-choice-default" title="The default fabric">{ "default" }</span>
                            }
                            <span class="fabric-choice-count">{ fabric.devices }</span>
                        </a>
                    }
                }) }
                <a class="fabric-choice is-manage" onclick={link.callback(|_| fabrics(FabricsMsg::OpenManage))}>
                    { icon(Icon::Sliders, 15) }
                    <span class="fabric-choice-name">{ "Manage fabrics…" }</span>
                </a>
                { self.view_backup_choice(link) }
            </>
        }
    }

    /// The popup that manages our fabrics.
    pub(crate) fn view_manage_modal(&self, link: &Scope<Self>) -> Html {
        let Some(view) = &self.manage else {
            return html! {};
        };

        html! {
            <div class="modal is-active">
                <div class="modal-background" onclick={link.callback(|_| fabrics(FabricsMsg::CloseManage))}></div>
                <div class="modal-card manage-card">
                    { ui::modal_head("Our fabrics", "The homes devices are commissioned into", html! {},
                        link.callback(|_| fabrics(FabricsMsg::CloseManage))) }
                    <section class="modal-card-body">
                        <p class="manage-intro">
                            { "Each fabric has its root CA of its own: a home, a customer, or test and production apart. The page shows one at a time; commissioning puts a device into the one shown. The default fabric is what a command or tool naming none gets, and can't be deleted." }
                        </p>
                        if let Some(error) = &view.error {
                            <p class="share-error">{ icon(Icon::Alert, 16) }<span>{ error.clone() }</span></p>
                        }
                        <div class="manage-list">
                            { for self.fabrics.iter().map(|fabric| view_fabric_row(view, fabric, link)) }
                        </div>
                        <form class="manage-create" onsubmit={link.callback(|e: SubmitEvent| {
                            e.prevent_default();
                            fabrics(FabricsMsg::Create)
                        })}>
                            <input class="input is-small" type="text" maxlength="32" placeholder="A new fabric's name"
                                value={view.new_name.clone()}
                                oninput={link.callback(|e: InputEvent| fabrics(FabricsMsg::NewName(crate::input_value(e))))} />
                            <button class={classes!("button", "is-small", "is-link", view.busy.then_some("is-loading"))}
                                type="submit" disabled={view.busy || view.new_name.trim().is_empty()}>
                                <span class="icon is-small">{ icon(Icon::Plus, 16) }</span>
                                <span>{ "Create" }</span>
                            </button>
                        </form>
                    </section>
                </div>
            </div>
        }
    }
}

fn view_fabric_row(view: &ManageView, fabric: &FabricInfo, link: &Scope<App>) -> Html {
    let index = fabric.index;
    let renaming = view
        .renaming
        .as_ref()
        .filter(|(of, _)| *of == index)
        .map(|(_, name)| name.clone());

    html! {
        <div class={classes!("manage-row", fabric.default.then_some("is-default"))}>
            { dot(fabric.color) }
            <div class="manage-main">
                if let Some(name) = renaming {
                    <form class="manage-rename" onsubmit={link.callback(|e: SubmitEvent| {
                        e.prevent_default();
                        fabrics(FabricsMsg::Rename)
                    })}>
                        <input class="input is-small" type="text" maxlength="32" value={name}
                            oninput={link.callback(|e: InputEvent| fabrics(FabricsMsg::RenameText(crate::input_value(e))))} />
                        <button class="button is-small is-link" type="submit" disabled={view.busy}>{ "Save" }</button>
                        <button class="button is-small" type="button"
                            onclick={link.callback(|_| fabrics(FabricsMsg::CancelRename))}>{ "Cancel" }</button>
                    </form>
                } else {
                    <p class="manage-title">
                        <span>{ fabric.name.clone() }</span>
                        if fabric.default {
                            { ui::pill("Default", Some("is-up"), Some("What a command or tool naming no fabric gets".into())) }
                        }
                    </p>
                }
                <p class="manage-sub mono">
                    { format!("#{index} · fabric {} · our node {} · {} device(s)",
                        display::hex_u64(fabric.fabric_id), display::hex_u64(fabric.node_id), fabric.devices) }
                </p>
                if view.deleting == Some(index) {
                    <div class="manage-delete">
                        <span>{ format!("Its {} device(s):", fabric.devices) }</span>
                        <button class="button is-small is-danger is-soft"
                            title="Remove the fabric from each device first (if it is reached), and drop them from the list"
                            onclick={link.callback(move |_| Msg::Ask(Ask::DeleteFabric(index, FabricDeletion::Decommission)))}>
                            { "Decommission them" }
                        </button>
                        <button class="button is-small is-soft"
                            title="Drop them from the list: they keep the dead fabric until another controller removes it"
                            onclick={link.callback(move |_| Msg::Ask(Ask::DeleteFabric(index, FabricDeletion::Forget)))}>
                            { "Forget them" }
                        </button>
                        <button class="button is-small" onclick={link.callback(|_| fabrics(FabricsMsg::CancelDelete))}>{ "Cancel" }</button>
                    </div>
                }
            </div>
            <div class="manage-actions">
                if !fabric.default {
                    <button class="icon-action" title="Make it the default" disabled={view.busy}
                        onclick={link.callback(move |_| fabrics(FabricsMsg::SetDefault(index)))}>
                        { icon(Icon::Check, 17) }
                    </button>
                }
                <button class="icon-action" title="Rename it" disabled={view.busy}
                    onclick={link.callback(move |_| fabrics(FabricsMsg::StartRename(index)))}>
                    { icon(Icon::Edit, 17) }
                </button>
                if !fabric.default {
                    <button class="icon-action is-danger" title="Delete it" disabled={view.busy}
                        onclick={link.callback(move |_| fabrics(FabricsMsg::Delete(index)))}>
                        { icon(Icon::Trash, 17) }
                    </button>
                }
            </div>
        </div>
    }
}

/// A fabric's color.
pub(crate) fn dot(color: u8) -> Html {
    html! { <span class={classes!("fabric-dot", format!("is-color-{}", color % 8))}></span> }
}

/// The fabric the page showed last, if it remembers one.
pub(crate) fn remembered_visible() -> Option<u8> {
    let storage = web_sys::window()?.local_storage().ok()??;

    storage.get_item(VISIBLE_KEY).ok()??.parse().ok()
}

fn remember_visible(index: u8) {
    if let Some(Ok(Some(storage))) = web_sys::window().map(|window| window.local_storage()) {
        let _ = storage.set_item(VISIBLE_KEY, &index.to_string());
    }
}

fn fabrics(msg: FabricsMsg) -> Msg {
    Msg::OurFabrics(msg)
}

/// Send `request` on a connection of its own; the answer goes to
/// [`FabricsMsg`].
fn send(link: &Scope<App>, request: wire::Request) {
    request_loop(
        link.clone(),
        request,
        |response| fabrics(FabricsMsg::Loaded(response)),
        |e| fabrics(FabricsMsg::Failed(e)),
    );
}
