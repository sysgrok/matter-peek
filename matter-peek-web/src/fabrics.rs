//! The fabrics a device is on, in a popup: the controllers (ecosystems) it
//! belongs to, as its subscription reports them.

use yew::html::Scope;
use yew::prelude::*;

use matter_peek_core::api::{display, DeviceFabric, DeviceHealth};

use crate::icons::{icon, Icon};
use crate::{ui, App, Msg};

/// What happens in the fabrics popup.
pub enum FabricsMsg {
    Open(u64),
    Close,
}

impl App {
    pub(crate) fn update_fabrics(&mut self, msg: FabricsMsg) -> bool {
        self.fabrics_of = match msg {
            FabricsMsg::Open(node_id) => Some(node_id),
            FabricsMsg::Close => None,
        };

        true
    }

    /// Popup with the fabrics of a device.
    pub(crate) fn view_fabrics_modal(&self, link: &Scope<Self>) -> Html {
        let Some(node_id) = self.fabrics_of else {
            return html! {};
        };
        let title = self.device_title(node_id);
        let health = self.subs.get(&node_id).map(|sub| &sub.health);
        let refreshing = self.refreshing(node_id);

        html! {
            <div class="modal is-active">
                <div class="modal-background" onclick={link.callback(|_| fabrics(FabricsMsg::Close))}></div>
                <div class="modal-card fabrics-card">
                    { ui::modal_head("Fabrics", &title, html! {
                        <button class={classes!("button", "is-small", refreshing.then_some("is-loading"))}
                            title="Read them again" disabled={refreshing}
                            onclick={link.callback(move |_| Msg::Refresh(node_id))}>
                            <span class="icon is-small">{ icon(Icon::Refresh, 16) }</span>
                            <span>{ "Refresh" }</span>
                        </button>
                    }, link.callback(|_| fabrics(FabricsMsg::Close))) }
                    <section class="modal-card-body">
                        <p class="fabrics-intro">
                            { "The controllers it belongs to: one fabric each, set up by the app that commissioned it there." }
                        </p>
                        { match health.and_then(|health| health.fabrics.as_ref()) {
                            None => html! {
                                <p class="fabrics-none">{ "Not reported yet: refresh to read them." }</p>
                            },
                            Some(list) => html! {
                                <div class="fabrics-list">
                                    { for list.iter().map(|fabric| view_fabric(node_id, fabric, link)) }
                                </div>
                            },
                        } }
                        <div class="fabrics-footer">
                            <span>{ health.map(count).unwrap_or_default() }</span>
                            <button class="button is-small is-soft" title="Let another app commission it too"
                                onclick={link.batch_callback(move |_| vec![
                                    fabrics(FabricsMsg::Close),
                                    Msg::Share(crate::share::ShareMsg::Open(node_id)),
                                ])}>
                                <span class="icon is-small">{ icon(Icon::Share, 16) }</span>
                                <span>{ "Share with another app" }</span>
                            </button>
                        </div>
                    </section>
                </div>
            </div>
        }
    }
}

fn view_fabric(node_id: u64, fabric: &DeviceFabric, link: &Scope<App>) -> Html {
    let remove = fabric.index.filter(|_| !fabric.ours).map(|index| {
        let name = fabric.name();
        html! {
            <button class="icon-action is-danger" title={format!("Remove {name} from the device")}
                onclick={link.callback(move |_| Msg::Ask(crate::Ask::RemoveFabric(node_id, index, name.clone())))}>
                { icon(Icon::Trash, 17) }
            </button>
        }
    });

    html! {
        <div class={classes!("fabric-row", fabric.ours.then_some("is-ours"))}>
            <span class="tile-icon is-other">{ icon(Icon::Fabrics, 20) }</span>
            <div class="fabric-main">
                <p class="fabric-title">
                    <span>{ fabric.name() }</span>
                    if fabric.ours {
                        { ui::pill("Ours", Some("is-up"), Some("Our own fabric".into())) }
                    }
                </p>
                <p class="fabric-sub">
                    { format!("Commissioned by a controller of {}", display::vendor_label(fabric.vendor_id, None)) }
                </p>
            </div>
            <div class="fabric-ids mono">
                if let Some(index) = fabric.index {
                    <span title="The device's index of the fabric">{ format!("#{index}") }</span>
                }
                <span title="Fabric ID">{ format!("fabric {}", display::hex_u64(fabric.fabric_id)) }</span>
                <span title="The device's node ID on the fabric">{ format!("node {}", display::hex_u64(fabric.node_id)) }</span>
            </div>
            { remove.unwrap_or_default() }
        </div>
    }
}

/// `2 of 5 fabrics`, or as much as is known.
pub(crate) fn count(health: &DeviceHealth) -> String {
    match (
        health.fabrics.as_ref().map(Vec::len),
        health.supported_fabrics,
    ) {
        (Some(n), Some(supported)) => format!("On {n} of {supported} fabrics"),
        (Some(n), None) => format!("On {n} fabrics"),
        _ => String::new(),
    }
}

fn fabrics(msg: FabricsMsg) -> Msg {
    Msg::Fabrics(msg)
}
