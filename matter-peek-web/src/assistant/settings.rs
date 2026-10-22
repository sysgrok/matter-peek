//! The assistant's model, in a popup: its API (with presets), the API's key,
//! the model (from the models the API offers), and whether it thinks first;
//! tried before it is kept. The key is only ever sent, never shown: the server
//! keeps it.

use yew::html::Scope;
use yew::prelude::*;

use matter_peek_core::api::agent::same_api;
use matter_peek_core::api::{wire, AssistantInfo, AssistantUpdate, KeyChange};

use crate::icons::{icon, Icon};
use crate::{checkbox_checked, input_value, request_loop, send_chat_request, App, Ask, Msg};

use super::AssistantMsg;

/// Where the APIs of the usual places are.
const PRESETS: &[(&str, &str)] = &[
    ("Ollama", "http://localhost:11434/v1"),
    ("OpenAI", "https://api.openai.com/v1"),
    ("OpenRouter", "https://openrouter.ai/api/v1"),
];

/// The id of the model field's list of models.
const MODELS_LIST: &str = "assistant-models";

/// What a request's answer, and its failure, become.
type Handlers = (fn(wire::Response) -> Msg, fn(String) -> Msg);

/// The assistant's model, as being set in its popup.
pub struct ModelForm {
    /// Until the model as kept is read.
    pub loading: bool,
    /// As kept; `None`: none yet.
    pub kept: Option<AssistantInfo>,
    pub url: String,
    pub model: String,
    /// A new key, as typed.
    pub key: String,
    pub show_key: bool,
    pub remove_key: bool,
    pub think: bool,
    /// The models the API offers, or why not.
    pub models: Result<Vec<String>, String>,
    pub loading_models: bool,
    pub testing: bool,
    /// What the model said, or why it did not.
    pub tested: Option<Result<String, String>>,
    pub saving: bool,
    pub error: Option<String>,
}

impl Default for ModelForm {
    fn default() -> Self {
        Self {
            loading: true,
            kept: None,
            url: String::new(),
            model: String::new(),
            key: String::new(),
            show_key: false,
            remove_key: false,
            think: true,
            models: Ok(Vec::new()),
            loading_models: false,
            testing: false,
            tested: None,
            saving: false,
            error: None,
        }
    }
}

impl ModelForm {
    /// The change the form makes to the model as kept.
    fn update(&self) -> AssistantUpdate {
        let key = if !self.key.trim().is_empty() {
            KeyChange::Set {
                key: self.key.trim().to_string(),
            }
        } else if self.remove_key {
            KeyChange::Remove
        } else {
            KeyChange::Keep
        };

        AssistantUpdate {
            url: Some(self.url.trim().to_string()),
            model: Some(self.model.trim().to_string()),
            think: Some(self.think),
            key,
        }
    }

    fn set_by_env(&self) -> bool {
        self.kept.as_ref().is_some_and(|kept| kept.from_env)
    }

    /// The end of the kept key, while it would stay.
    fn kept_key(&self) -> Option<&str> {
        self.kept
            .as_ref()
            .filter(|kept| same_api(&kept.url, &self.url))
            .and_then(|kept| kept.key_hint.as_deref())
    }

    /// Whether a kept key goes, the API moving elsewhere.
    fn key_dropped(&self) -> bool {
        self.kept.as_ref().is_some_and(|kept| {
            kept.key_hint.is_some() && !same_api(&kept.url, &self.url) && self.key.is_empty()
        })
    }

    fn busy(&self) -> bool {
        self.loading || self.saving
    }
}

/// What happens in the popup.
pub enum SettingsMsg {
    Open,
    Close,
    Url(String),
    Preset(&'static str),
    Model(String),
    Key(String),
    ToggleKey,
    RemoveKey(bool),
    Think(bool),
    LoadModels,
    Test,
    Save,
    /// Forget the model, once confirmed.
    Forget,
    Done(Reply, wire::Response),
    Failed(Reply, String),
}

/// Which request of the popup an answer is to.
#[derive(Clone, Copy)]
pub enum Reply {
    Kept,
    Models,
    Tested,
    Saved,
}

impl App {
    pub(crate) fn update_settings(&mut self, ctx: &Context<Self>, msg: SettingsMsg) -> bool {
        let link = ctx.link();

        if let SettingsMsg::Open = msg {
            self.assistant.settings = Some(ModelForm::default());
            send(link, wire::Request::Assistant, Reply::Kept);
            return true;
        }

        let Some(form) = self.assistant.settings.as_mut() else {
            return false;
        };

        match msg {
            SettingsMsg::Open => unreachable!("opened above"),
            SettingsMsg::Close => {
                self.assistant.settings = None;
            }
            SettingsMsg::Url(url) => {
                form.url = url;
                form.tested = None;
            }
            SettingsMsg::Preset(url) => {
                form.url = url.to_string();
                form.tested = None;
                link.send_message(settings(SettingsMsg::LoadModels));
            }
            SettingsMsg::Model(model) => {
                form.model = model;
                form.tested = None;
            }
            SettingsMsg::Key(key) => {
                form.key = key;
                form.tested = None;
            }
            SettingsMsg::ToggleKey => form.show_key = !form.show_key,
            SettingsMsg::RemoveKey(remove) => {
                form.remove_key = remove;
                form.tested = None;
            }
            SettingsMsg::Think(think) => form.think = think,
            SettingsMsg::LoadModels => {
                if form.url.trim().is_empty() || form.loading_models {
                    return false;
                }
                form.loading_models = true;
                let update = (!form.set_by_env()).then(|| form.update());
                send(
                    link,
                    wire::Request::AssistantModels { update },
                    Reply::Models,
                );
            }
            SettingsMsg::Test => {
                form.testing = true;
                form.tested = None;
                let update = (!form.set_by_env()).then(|| form.update());
                send(link, wire::Request::AssistantTest { update }, Reply::Tested);
            }
            SettingsMsg::Save => {
                form.saving = true;
                form.error = None;
                let update = form.update();
                send(link, wire::Request::SetAssistant { update }, Reply::Saved);
            }
            SettingsMsg::Forget => {
                form.saving = true;
                form.error = None;
                send(link, wire::Request::ClearAssistant, Reply::Saved);
            }
            SettingsMsg::Done(reply, response) => match (reply, response) {
                (_, wire::Response::Error { message }) => {
                    return self.update_settings(ctx, SettingsMsg::Failed(reply, message));
                }
                (Reply::Kept, wire::Response::Assistant { assistant }) => {
                    form.loading = false;
                    if let Some(kept) = &assistant {
                        form.url = kept.url.clone();
                        form.model = kept.model.clone();
                        form.think = kept.think;
                    } else {
                        form.url = PRESETS[0].1.to_string();
                    }
                    form.kept = assistant;
                    link.send_message(settings(SettingsMsg::LoadModels));
                }
                (Reply::Models, wire::Response::Models { models }) => {
                    form.loading_models = false;
                    form.models = Ok(models);
                }
                (
                    Reply::Tested,
                    wire::Response::Tested {
                        model,
                        reply,
                        millis,
                    },
                ) => {
                    form.testing = false;
                    form.tested = Some(Ok(format!(
                        "{model} answered in {:.1} s: “{}”",
                        millis as f64 / 1000.0,
                        reply.trim()
                    )));
                }
                (Reply::Saved, wire::Response::Assistant { assistant }) => {
                    self.assistant.model = assistant.map(|info| info.model);
                    self.assistant.settings = None;
                    send_chat_request(link.clone(), wire::Request::ChatList);
                }
                _ => {}
            },
            SettingsMsg::Failed(reply, message) => match reply {
                Reply::Kept => {
                    form.loading = false;
                    form.error = Some(message);
                }
                Reply::Models => {
                    form.loading_models = false;
                    form.models = Err(message);
                }
                Reply::Tested => {
                    form.testing = false;
                    form.tested = Some(Err(message));
                }
                Reply::Saved => {
                    form.saving = false;
                    form.error = Some(message);
                }
            },
        }

        true
    }

    /// The popup with the assistant's model.
    pub(crate) fn view_settings_modal(&self, link: &Scope<Self>) -> Html {
        let Some(form) = &self.assistant.settings else {
            return html! {};
        };
        let from_env = form.set_by_env();
        let locked = from_env || form.busy();
        let close = link.callback(|_| settings(SettingsMsg::Close));

        html! {
            <div class="modal is-active">
                <div class="modal-background" onclick={close.clone()}></div>
                <div class="modal-card dialog model-dialog">
                    <header class="modal-card-head">
                        <span class="assistant-mark is-large">{ icon(Icon::Sparkles, 18) }</span>
                        <div class="model-dialog-title">
                            <p class="modal-card-title">{ "The assistant's model" }</p>
                            <p class="model-dialog-subtitle">
                                { "Any model behind an OpenAI-compatible API: on this network, or a provider's." }
                            </p>
                        </div>
                        <button class="delete" aria-label="close" onclick={close.clone()}></button>
                    </header>
                    <section class="modal-card-body">
                        if form.loading {
                            <div class="model-dialog-loading"><span class="loader"></span></div>
                        } else {
                            if from_env {
                                <p class="model-note">
                                    { icon(Icon::Info, 16) }
                                    <span>
                                        { "The server's environment sets the model (MATTER_PEEK_LLM_URL, \
                                           MATTER_PEEK_LLM_MODEL): change it there, or unset it to change it here." }
                                    </span>
                                </p>
                            }
                            if let Some(error) = &form.error {
                                <p class="model-note is-error">{ icon(Icon::Alert, 16) }<span>{ error.clone() }</span></p>
                            }
                            { self.view_api_field(form, locked, link) }
                            { self.view_key_field(form, locked, link) }
                            { self.view_model_field(form, locked, link) }
                            <label class="switch-row model-think">
                                <input type="checkbox" class="switch" checked={form.think} disabled={locked}
                                    onchange={link.callback(|e: Event| settings(SettingsMsg::Think(checkbox_checked(e))))} />
                                <span>
                                    <span class="switch-title">{ "Think before answering" }</span>
                                    <span class="switch-text">
                                        { "Slower, more careful. Models that do not think ignore it." }
                                    </span>
                                </span>
                            </label>
                            if let Some(tested) = &form.tested {
                                { match tested {
                                    Ok(text) => html! {
                                        <p class="model-test is-ok">{ icon(Icon::CheckCircle, 16) }<span>{ text.clone() }</span></p>
                                    },
                                    Err(text) => html! {
                                        <p class="model-test is-failed">{ icon(Icon::XCircle, 16) }<span>{ text.clone() }</span></p>
                                    },
                                } }
                            }
                        }
                    </section>
                    <footer class="modal-card-foot model-dialog-foot">
                        if form.kept.is_some() && !from_env {
                            <button class="button is-danger is-outlined" disabled={form.busy()}
                                onclick={link.callback(|_| Msg::Ask(Ask::ForgetModel))}>
                                { "Forget" }
                            </button>
                        }
                        <span class="model-dialog-spacer"></span>
                        <button class={classes!("button", "is-tinted", form.testing.then_some("is-loading"))}
                            disabled={form.busy() || form.url.trim().is_empty() || form.model.trim().is_empty()}
                            onclick={link.callback(|_| settings(SettingsMsg::Test))}>
                            { "Test" }
                        </button>
                        if !from_env {
                            <button class={classes!("button", "is-link", form.saving.then_some("is-loading"))}
                                disabled={locked || form.url.trim().is_empty() || form.model.trim().is_empty()}
                                onclick={link.callback(|_| settings(SettingsMsg::Save))}>
                                { "Save" }
                            </button>
                        }
                    </footer>
                </div>
            </div>
        }
    }

    fn view_api_field(&self, form: &ModelForm, locked: bool, link: &Scope<Self>) -> Html {
        let preset = PRESETS
            .iter()
            .find(|(_, url)| form.url.trim().trim_end_matches('/') == *url)
            .map(|&(name, _)| name);

        html! {
            <div class="field">
                <label class="label">{ "API" }</label>
                <div class="segmented model-presets">
                    { for PRESETS.iter().map(|&(name, url)| html! {
                        <button class={classes!("segment", (preset == Some(name)).then_some("is-active"))}
                            disabled={locked}
                            onclick={link.callback(move |_| settings(SettingsMsg::Preset(url)))}>
                            { name }
                        </button>
                    }) }
                    <span class={classes!("segment", "is-static", preset.is_none().then_some("is-active"))}>
                        { "Other" }
                    </span>
                </div>
                <div class="control has-icons-left">
                    <input class="input" placeholder="http://localhost:11434/v1" disabled={locked}
                        value={form.url.clone()}
                        oninput={link.callback(|e: InputEvent| settings(SettingsMsg::Url(input_value(e))))}
                        onchange={link.callback(|_| settings(SettingsMsg::LoadModels))} />
                    <span class="icon is-left">{ icon(Icon::Globe, 18) }</span>
                </div>
            </div>
        }
    }

    fn view_key_field(&self, form: &ModelForm, locked: bool, link: &Scope<Self>) -> Html {
        let kept = form.kept_key().filter(|_| !form.remove_key);
        let placeholder = match kept {
            Some(hint) => format!("Kept ({hint}); type a new one to replace it"),
            None => "Only if the API needs one".to_string(),
        };

        html! {
            <div class="field">
                <label class="label">{ "API key" }</label>
                <div class="field has-addons mb-0">
                    <div class="control has-icons-left is-expanded">
                        <input class="input" placeholder={placeholder} disabled={locked}
                            type={if form.show_key { "text" } else { "password" }}
                            autocomplete="off"
                            value={form.key.clone()}
                            oninput={link.callback(|e: InputEvent| settings(SettingsMsg::Key(input_value(e))))}
                            onchange={link.callback(|_| settings(SettingsMsg::LoadModels))} />
                        <span class="icon is-left">{ icon(Icon::Key, 18) }</span>
                    </div>
                    <div class="control">
                        <button class="button" disabled={locked}
                            title={if form.show_key { "Hide the key" } else { "Show the key" }}
                            onclick={link.callback(|_| settings(SettingsMsg::ToggleKey))}>
                            <span class="icon">{ icon(if form.show_key { Icon::EyeOff } else { Icon::Eye }, 18) }</span>
                        </button>
                    </div>
                </div>
                if form.key_dropped() {
                    <p class="help is-warning">
                        { "The kept key is for another address: it does not go to this one." }
                    </p>
                } else if form.kept_key().is_some() && form.key.is_empty() && !locked {
                    <p class="help">
                        if form.remove_key {
                            { "The key goes when you save. " }
                            <a onclick={link.callback(|_| settings(SettingsMsg::RemoveKey(false)))}>{ "Keep it" }</a>
                        } else {
                            <a onclick={link.callback(|_| settings(SettingsMsg::RemoveKey(true)))}>{ "Remove the key" }</a>
                        }
                    </p>
                } else {
                    <p class="help">{ "Kept by the server: it never comes back to this page." }</p>
                }
            </div>
        }
    }

    fn view_model_field(&self, form: &ModelForm, locked: bool, link: &Scope<Self>) -> Html {
        let help = match &form.models {
            _ if form.loading_models => html! { <>{ "Asking the API for its models…" }</> },
            Ok(models) if models.is_empty() => html! {
                <>{ "Its name as the API knows it." }</>
            },
            Ok(models) => html! {
                <>{ format!("The API offers {} model{}: pick one, or type its name.",
                    models.len(), if models.len() == 1 { "" } else { "s" }) }</>
            },
            Err(e) => {
                html! { <span class="has-text-danger">{ format!("No list of models: {e}") }</span> }
            }
        };

        html! {
            <div class="field">
                <label class="label">{ "Model" }</label>
                <div class="field has-addons mb-0">
                    <div class="control has-icons-left is-expanded">
                        <input class="input" placeholder="qwen3.5:9b" disabled={locked} list={MODELS_LIST}
                            value={form.model.clone()}
                            oninput={link.callback(|e: InputEvent| settings(SettingsMsg::Model(input_value(e))))} />
                        <span class="icon is-left">{ icon(Icon::Sparkles, 18) }</span>
                    </div>
                    <div class="control">
                        <button class={classes!("button", form.loading_models.then_some("is-loading"))}
                            title="Ask the API for its models"
                            disabled={locked || form.url.trim().is_empty()}
                            onclick={link.callback(|_| settings(SettingsMsg::LoadModels))}>
                            <span class="icon">{ icon(Icon::Refresh, 18) }</span>
                        </button>
                    </div>
                </div>
                <datalist id={MODELS_LIST}>
                    { for form.models.iter().flatten().map(|model| html! { <option value={model.clone()} /> }) }
                </datalist>
                <p class="help">{ help }</p>
            </div>
        }
    }
}

fn settings(msg: SettingsMsg) -> Msg {
    Msg::Assistant(AssistantMsg::Settings(msg))
}

/// Send a request of the popup; its answer comes back as `reply`'s.
fn send(link: &Scope<App>, request: wire::Request, reply: Reply) {
    let (done, failed): Handlers = match reply {
        Reply::Kept => (
            |r| settings(SettingsMsg::Done(Reply::Kept, r)),
            |e| settings(SettingsMsg::Failed(Reply::Kept, e)),
        ),
        Reply::Models => (
            |r| settings(SettingsMsg::Done(Reply::Models, r)),
            |e| settings(SettingsMsg::Failed(Reply::Models, e)),
        ),
        Reply::Tested => (
            |r| settings(SettingsMsg::Done(Reply::Tested, r)),
            |e| settings(SettingsMsg::Failed(Reply::Tested, e)),
        ),
        Reply::Saved => (
            |r| settings(SettingsMsg::Done(Reply::Saved, r)),
            |e| settings(SettingsMsg::Failed(Reply::Saved, e)),
        ),
    };

    request_loop(link.clone(), request, done, failed);
}
