//! The Updates page: the OTA images we offer the devices.

use yew::html::Scope;
use yew::prelude::*;

use matter_peek_core::api::display;

use crate::icons::{icon, Icon};
use crate::ui;
use crate::{picked_file, App, Ask, Msg};

impl App {
    pub(crate) fn view_updates_tab(&self, link: &Scope<Self>) -> Html {
        let count = self.ota_images.len();
        let subtitle = if count == 0 {
            "The OTA images we offer the devices".to_string()
        } else {
            format!(
                "{count} image{} we offer the devices",
                if count == 1 { "" } else { "s" }
            )
        };

        let actions = html! {
            <button class="button is-icon" title="Read the images again"
                onclick={link.callback(|_| Msg::OtaImages)}>
                { icon(Icon::Refresh, 18) }
            </button>
        };

        html! {
            <div class="narrow">
                { ui::page_header("Updates", html! { subtitle }, actions) }

                <label class={classes!("dropzone", self.uploading.is_some().then_some("is-busy"))}>
                    // Over the whole zone, so a file can be dropped as well as picked.
                    <input class="dropzone-input" type="file"
                        disabled={self.uploading.is_some()}
                        onchange={link.batch_callback(|e: Event| picked_file(e).map(Msg::OtaFile))} />
                    <span class="dropzone-icon">{ icon(Icon::Upload, 26) }</span>
                    if let Some(percent) = self.uploading {
                        <span class="dropzone-title">{ format!("Uploading… {percent}%") }</span>
                        <progress class="progress is-small is-link" value={percent.to_string()} max="100"></progress>
                    } else {
                        <span class="dropzone-title">{ "Drop an OTA image here, or pick one" }</span>
                        <span class="dropzone-text">
                            { "Its header and digest are checked. Devices see it if it is for them and newer than what they run." }
                        </span>
                    }
                </label>

                if count == 0 {
                    { ui::empty_state(Icon::Updates, "No images yet",
                        "Upload an image to offer it to the devices it is for.", html! {}) }
                } else {
                    <div class="image-list">
                        { for self.ota_images.iter().map(|image| {
                            let name = image.name.clone();
                            let devices = self
                                .stored
                                .iter()
                                .filter(|d| self.applicable(d).any(|other| other.name == image.name))
                                .count();
                            let range = match (image.min_applicable_version, image.max_applicable_version) {
                                (Some(min), Some(max)) => Some(format!(
                                    "For versions {} to {}", display::hex_u32(min), display::hex_u32(max))),
                                (Some(min), None) => Some(format!("For versions from {}", display::hex_u32(min))),
                                (None, Some(max)) => Some(format!("For versions up to {}", display::hex_u32(max))),
                                (None, None) => None,
                            };

                            html! {
                                <div class="image-card">
                                    <span class="tile-icon is-other">{ icon(Icon::Updates, 22) }</span>
                                    <div class="image-main">
                                        <p class="image-title">
                                            <span>{ image.version_str.clone() }</span>
                                            { ui::pill(&display::hex_u32(image.version), None, Some("Its software version number".into())) }
                                        </p>
                                        <p class="image-sub">
                                            { display::vendor_label(image.vendor_id, None) }
                                            <span class="dot-sep"></span>
                                            { display::product_label(image.vendor_id, image.product_id, None) }
                                        </p>
                                        <div class="image-chips">
                                            <span class="net-chip mono" title="File">{ image.name.clone() }</span>
                                            <span class="net-chip" title="Size">{ ui::bytes(image.size) }</span>
                                            if let Some(range) = range {
                                                <span class="net-chip">{ range }</span>
                                            }
                                            if devices > 0 {
                                                { ui::pill(
                                                    &format!("{devices} device{} can update", if devices == 1 { "" } else { "s" }),
                                                    Some("is-up"), None) }
                                            }
                                            if let Some(url) = &image.release_notes_url {
                                                <a class="net-chip" href={url.clone()} target="_blank" rel="noopener">
                                                    { icon(Icon::External, 14) }{ "Release notes" }
                                                </a>
                                            }
                                        </div>
                                    </div>
                                    <button class="icon-action is-danger" title="Delete it"
                                        onclick={link.callback(move |_| Msg::Ask(Ask::DeleteImage(name.clone())))}>
                                        { icon(Icon::Trash, 18) }
                                    </button>
                                </div>
                            }
                        }) }
                    </div>
                }
            </div>
        }
    }
}
