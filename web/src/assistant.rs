//! The Assistant page: chats with the server's assistant (see
//! `matter_peek_core::api::agent`), which looks into the devices with the API's
//! tools, and changes them once the user approves.

use std::collections::HashMap;

use yew::html::Scope;
use yew::prelude::*;

use matter_peek_core::api::agent::{Message, ToolCall, STOPPED};
use matter_peek_core::api::{display, wire};

use wire::{Approval, ChatEvent, ChatSummary, ChatView};

use crate::icons::{icon, Icon};
use crate::{send_chat_request, textarea_value, ui, App, Ask, Msg};

/// Questions to start a chat with.
const SUGGESTIONS: &[&str] = &[
    "Which of my devices are offline?",
    "Show my devices as a table, with their network and uptime.",
    "How good is the Wi-Fi signal of my devices?",
    "One of my devices keeps dropping off. How do I find out why?",
];

/// How long to wait before looking again at a chat the assistant is at work in
/// for another page.
const POLL_MS: u32 = 2_000;

/// The Assistant page.
#[derive(Default)]
pub struct Assistant {
    /// Whether the chats were listed: until then, whether there is an
    /// assistant is not known.
    pub loaded: bool,
    /// The model; `None`: no assistant.
    pub model: Option<String>,
    pub chats: Vec<ChatSummary>,
    /// The chat shown; `None`: a new one. A new chat asked about has no id
    /// until the server's first word.
    pub chat: Option<ChatView>,
    pub draft: String,
    pub allow_changes: bool,
    /// A question of this page is being answered.
    pub asking: bool,
    /// The model is being asked.
    pub thinking: bool,
    pub error: Option<String>,
    /// On phones: the list of chats, rather than the chat.
    pub show_list: bool,
}

impl Assistant {
    /// Apply what the assistant did in chat `chat_id`: whether the chat shown
    /// changed.
    fn apply(&mut self, chat_id: &str, update: ChatEvent) -> bool {
        let Some(chat) = self.chat.as_mut() else {
            return false;
        };
        if chat.id.is_empty() && self.asking {
            chat.id = chat_id.to_string();
        }
        if chat.id != chat_id {
            return false;
        }

        match update {
            ChatEvent::Thinking => self.thinking = true,
            ChatEvent::Text { text } => {
                self.thinking = false;
                chat.messages.push(Message::Assistant {
                    text,
                    calls: Vec::new(),
                });
            }
            ChatEvent::ToolCall { call } => {
                self.thinking = false;
                match chat.messages.last_mut() {
                    Some(Message::Assistant { calls, .. }) => calls.push(call),
                    _ => chat.messages.push(Message::Assistant {
                        text: String::new(),
                        calls: vec![call],
                    }),
                }
            }
            ChatEvent::ToolResult {
                call_id,
                result,
                is_error,
            } => chat.messages.push(Message::Tool {
                call_id,
                result: if is_error {
                    format!("ERROR: {result}")
                } else {
                    result
                },
            }),
            ChatEvent::Approve { call } => {
                self.thinking = false;
                chat.pending = Some(call);
            }
            ChatEvent::Error { message } => {
                self.thinking = false;
                self.error = Some(message);
            }
        }

        true
    }
}

/// What happens on the Assistant page.
pub enum AssistantMsg {
    /// List the chats.
    Load,
    /// The server's answer to a chat request.
    Response(wire::Response),
    /// A chat request did not reach the server.
    Failed(String),
    /// What the assistant did in a chat.
    Update(String, ChatEvent),
    Open(String),
    New,
    Draft(String),
    Send,
    Suggest(&'static str),
    /// Answer the assistant's request to run a tool.
    Answer(String, Approval),
    Stop,
    AllowChanges(bool),
    /// Look again at a chat the assistant is at work in for another page.
    Poll(String),
    ShowList,
}

impl App {
    pub(crate) fn update_assistant(&mut self, ctx: &Context<Self>, msg: AssistantMsg) -> bool {
        let link = ctx.link();
        let assistant = &mut self.assistant;

        match msg {
            AssistantMsg::Load => {
                send_chat_request(link.clone(), wire::Request::ChatList);
                false
            }
            AssistantMsg::Response(wire::Response::Chats { model, chats }) => {
                assistant.loaded = true;
                assistant.model = model;
                assistant.chats = chats;
                // A chat deleted elsewhere goes.
                let gone = assistant.chat.as_ref().is_some_and(|chat| {
                    !chat.id.is_empty() && !assistant.chats.iter().any(|c| c.id == chat.id)
                });
                if gone && !assistant.asking {
                    assistant.chat = None;
                }
                true
            }
            AssistantMsg::Response(wire::Response::Chat { chat }) => {
                let chat = *chat;
                let asked = assistant.asking
                    && assistant
                        .chat
                        .as_ref()
                        .is_none_or(|shown| shown.id.is_empty() || shown.id == chat.id);
                if asked {
                    assistant.asking = false;
                    assistant.thinking = false;
                    send_chat_request(link.clone(), wire::Request::ChatList);
                }

                if chat.running && !assistant.asking {
                    let id = chat.id.clone();
                    let link = link.clone();
                    wasm_bindgen_futures::spawn_local(async move {
                        gloo_timers::future::TimeoutFuture::new(POLL_MS).await;
                        link.send_message(Msg::Assistant(AssistantMsg::Poll(id)));
                    });
                }

                assistant.allow_changes = chat.allow_changes;
                assistant.chat = Some(chat);
                self.scroll_chat = true;
                true
            }
            AssistantMsg::Response(wire::Response::Error { message })
            | AssistantMsg::Failed(message) => {
                assistant.error = Some(message);
                assistant.asking = false;
                assistant.thinking = false;
                true
            }
            AssistantMsg::Response(_) => false,
            AssistantMsg::Update(chat_id, update) => {
                let new = assistant
                    .chat
                    .as_ref()
                    .is_some_and(|chat| chat.id.is_empty());
                let changed = assistant.apply(&chat_id, update);
                // A new chat is listed from its first word on.
                if new
                    && assistant
                        .chat
                        .as_ref()
                        .is_some_and(|chat| chat.id == chat_id)
                {
                    send_chat_request(link.clone(), wire::Request::ChatList);
                }
                self.scroll_chat |= changed;
                changed
            }
            AssistantMsg::Open(chat_id) => {
                if assistant.asking {
                    return false;
                }
                assistant.error = None;
                assistant.show_list = false;
                send_chat_request(link.clone(), wire::Request::ChatOpen { chat_id });
                true
            }
            AssistantMsg::New => {
                if assistant.asking {
                    return false;
                }
                assistant.chat = None;
                assistant.error = None;
                assistant.draft.clear();
                assistant.allow_changes = false;
                assistant.show_list = false;
                true
            }
            AssistantMsg::Draft(draft) => {
                assistant.draft = draft;
                true
            }
            AssistantMsg::Send => {
                let text = assistant.draft.trim().to_string();
                let busy =
                    assistant.asking || assistant.chat.as_ref().is_some_and(|chat| chat.running);
                if text.is_empty() || busy {
                    return false;
                }

                assistant.draft.clear();
                assistant.error = None;
                assistant.asking = true;
                assistant.thinking = true;

                let allow_changes = assistant.allow_changes;
                let chat = assistant.chat.get_or_insert_with(|| ChatView {
                    id: String::new(),
                    title: text.clone(),
                    updated: 0,
                    allow_changes,
                    running: true,
                    messages: Vec::new(),
                    pending: None,
                });
                chat.messages.push(Message::User(text.clone()));
                chat.running = true;
                let chat_id = (!chat.id.is_empty()).then(|| chat.id.clone());

                send_chat_request(
                    link.clone(),
                    wire::Request::ChatAsk {
                        chat_id,
                        text,
                        allow_changes,
                    },
                );
                self.scroll_chat = true;
                true
            }
            AssistantMsg::Suggest(text) => {
                assistant.draft = text.to_string();
                link.send_message(Msg::Assistant(AssistantMsg::Send));
                true
            }
            AssistantMsg::Answer(call_id, answer) => {
                let Some(chat) = assistant.chat.as_mut() else {
                    return false;
                };
                chat.pending = None;
                assistant.thinking = answer != Approval::No;
                send_chat_request(
                    link.clone(),
                    wire::Request::ChatApprove {
                        chat_id: chat.id.clone(),
                        call_id,
                        answer,
                    },
                );
                true
            }
            AssistantMsg::Stop => {
                if let Some(chat) = assistant.chat.as_ref().filter(|chat| !chat.id.is_empty()) {
                    send_chat_request(
                        link.clone(),
                        wire::Request::ChatStop {
                            chat_id: chat.id.clone(),
                        },
                    );
                }
                false
            }
            AssistantMsg::AllowChanges(on) => {
                assistant.allow_changes = on;
                true
            }
            AssistantMsg::Poll(chat_id) => {
                let still = assistant
                    .chat
                    .as_ref()
                    .is_some_and(|chat| chat.id == chat_id && chat.running);
                if still && !assistant.asking {
                    send_chat_request(link.clone(), wire::Request::ChatOpen { chat_id });
                }
                false
            }
            AssistantMsg::ShowList => {
                assistant.show_list = true;
                true
            }
        }
    }

    /// The Assistant page.
    pub(crate) fn view_assistant_tab(&self, link: &Scope<Self>) -> Html {
        let assistant = &self.assistant;

        if !assistant.loaded {
            return html! {
                <div class="empty-state chat-state">
                    <span class="loader is-large"></span>
                    <p class="empty-state-text">{ "Looking for the assistant…" }</p>
                </div>
            };
        }
        if assistant.model.is_none() {
            return html! {
                <div class="chat-state">
                    { ui::empty_state(
                        Icon::Sparkles,
                        "No assistant yet",
                        "The assistant is a model behind an OpenAI-compatible API, such as a local Ollama. \
                         Start matter-peek-server with its address and name:",
                        html! {
                            <pre class="chat-setup">{ "MATTER_PEEK_LLM_URL=http://localhost:11434/v1 \\\n\
                                MATTER_PEEK_LLM_MODEL=qwen3.5:9b \\\n\
                                matter-peek-server" }</pre>
                        },
                    ) }
                </div>
            };
        }

        html! {
            <div class={classes!("assistant", assistant.show_list.then_some("is-listing"))}>
                { self.view_chat_list(link) }
                <section class="chat">
                    { self.view_chat_head(link) }
                    <div class="chat-scroll" ref={self.chat_scroll.clone()}>
                        <div class="chat-thread">{ self.view_thread(link) }</div>
                    </div>
                    { self.view_composer(link) }
                </section>
            </div>
        }
    }

    fn view_chat_list(&self, link: &Scope<Self>) -> Html {
        let assistant = &self.assistant;
        let shown = assistant.chat.as_ref().map(|chat| chat.id.as_str());

        html! {
            <aside class="chat-list">
                <div class="chat-list-head">
                    <span class="chat-list-title">{ "Chats" }</span>
                    { ui::icon_button(Icon::Plus, "New chat", classes!("is-small", "is-tinted"),
                        assistant.asking, None,
                        link.callback(|_| Msg::Assistant(AssistantMsg::New))) }
                </div>
                <nav class="chat-items">
                    if assistant.chats.is_empty() {
                        <p class="chat-list-empty">{ "No chats yet." }</p>
                    }
                    { for assistant.chats.iter().map(|chat| {
                        let open = chat.id.clone();
                        let delete = chat.id.clone();
                        html! {
                            <div class={classes!("chat-item",
                                (shown == Some(chat.id.as_str())).then_some("is-active"),
                                assistant.asking.then_some("is-disabled"))}
                                onclick={link.callback(move |_| Msg::Assistant(AssistantMsg::Open(open.clone())))}>
                                <span class="chat-item-text">
                                    <span class="chat-item-title">{ chat.title.clone() }</span>
                                    <span class="chat-item-time">{ ago(chat.updated) }</span>
                                </span>
                                <button class="icon-action is-danger chat-item-delete" title="Delete this chat"
                                    onclick={link.callback(move |e: MouseEvent| {
                                        e.stop_propagation();
                                        Msg::Ask(Ask::DeleteChat(delete.clone()))
                                    })}>
                                    { icon(Icon::Trash, 16) }
                                </button>
                            </div>
                        }
                    }) }
                </nav>
            </aside>
        }
    }

    fn view_chat_head(&self, link: &Scope<Self>) -> Html {
        let assistant = &self.assistant;
        let title = assistant
            .chat
            .as_ref()
            .map_or("New chat", |chat| chat.title.as_str());

        html! {
            <header class="chat-head">
                <button class="icon-action chat-back" title="Chats"
                    onclick={link.callback(|_| Msg::Assistant(AssistantMsg::ShowList))}>
                    { icon(Icon::ArrowLeft, 20) }
                </button>
                <div class="chat-head-title">
                    <span class="chat-title">{ title.to_string() }</span>
                    <span class="chat-model">
                        { icon(Icon::Sparkles, 13) }
                        { assistant.model.clone().unwrap_or_default() }
                    </span>
                </div>
                <label class="switch-row chat-allow"
                    title="Also offer the assistant the tools that change devices; it asks you before each">
                    <input type="checkbox" class="switch" checked={assistant.allow_changes}
                        disabled={assistant.asking}
                        onchange={link.callback(|e: Event| {
                            Msg::Assistant(AssistantMsg::AllowChanges(crate::checkbox_checked(e)))
                        })} />
                    <span class="switch-title">{ "Allow changes" }</span>
                </label>
            </header>
        }
    }

    fn view_thread(&self, link: &Scope<Self>) -> Html {
        let assistant = &self.assistant;
        let Some(chat) = assistant
            .chat
            .as_ref()
            .filter(|chat| !chat.messages.is_empty())
        else {
            return self.view_welcome(link);
        };

        // The results of the tool calls, by call.
        let results: HashMap<&str, &str> = chat
            .messages
            .iter()
            .filter_map(|message| match message {
                Message::Tool { call_id, result } => Some((call_id.as_str(), result.as_str())),
                _ => None,
            })
            .collect();

        let at_work = assistant.asking || chat.running;
        let pending = chat.pending.as_ref().map(|call| call.id.as_str());

        html! {
            <>
                { for blocks(&chat.messages).into_iter().map(|block| match block {
                    Block::User(text) => html! {
                        <div class="msg is-user"><div class="bubble">{ text.to_string() }</div></div>
                    },
                    Block::Assistant(parts) => html! {
                        <div class="msg is-assistant">
                            <span class="msg-avatar">{ icon(Icon::Sparkles, 16) }</span>
                            <div class="msg-body">
                                { for parts.into_iter().map(|part| match part {
                                    Part::Text(text) => html! { <div class="content md">{ markdown(text) }</div> },
                                    Part::Call(call) => self.view_tool_call(
                                        call,
                                        results.get(call.id.as_str()).copied(),
                                        at_work,
                                        pending == Some(call.id.as_str()),
                                    ),
                                }) }
                            </div>
                        </div>
                    },
                }) }

                if let Some(call) = &chat.pending {
                    { self.view_approval(call, link) }
                } else if assistant.thinking && assistant.asking {
                    <div class="msg is-assistant">
                        <span class="msg-avatar">{ icon(Icon::Sparkles, 16) }</span>
                        <div class="typing"><i></i><i></i><i></i></div>
                    </div>
                } else if chat.running && !assistant.asking {
                    <p class="chat-note"><span class="loader"></span>{ "The assistant is at work on this chat…" }</p>
                }

                if let Some(error) = &assistant.error {
                    if error == STOPPED {
                        <p class="chat-note">{ icon(Icon::Stop, 14) }{ "Stopped." }</p>
                    } else {
                        <p class="chat-note is-error">{ icon(Icon::Alert, 16) }{ error.clone() }</p>
                    }
                }
            </>
        }
    }

    fn view_welcome(&self, link: &Scope<Self>) -> Html {
        let model = self.assistant.model.clone().unwrap_or_default();

        html! {
            <div class="chat-welcome">
                <div class="chat-welcome-icon">{ icon(Icon::Sparkles, 30) }</div>
                <h2 class="chat-welcome-title">{ "Ask about your devices" }</h2>
                <p class="chat-welcome-text">
                    { format!("{model} looks into them with matter-peek's tools, and changes them only with your OK.") }
                </p>
                <div class="suggestions">
                    { for SUGGESTIONS.iter().map(|text| html! {
                        <button class="suggestion" disabled={self.assistant.asking}
                            onclick={link.callback(move |_| Msg::Assistant(AssistantMsg::Suggest(text)))}>
                            { *text }
                        </button>
                    }) }
                </div>
            </div>
        }
    }

    /// A tool call: what it did, opening onto its arguments and result.
    fn view_tool_call(
        &self,
        call: &ToolCall,
        result: Option<&str>,
        at_work: bool,
        asking: bool,
    ) -> Html {
        let error = result.and_then(|result| result.strip_prefix("ERROR: "));
        let (class, status) = match (result, error) {
            (Some(_), Some(_)) => ("is-failed", icon(Icon::XCircle, 16)),
            (Some(_), None) => ("is-done", icon(Icon::CheckCircle, 16)),
            (None, _) if asking => ("is-asking", icon(Icon::Alert, 16)),
            (None, _) if at_work => ("is-running", html! { <span class="loader"></span> }),
            (None, _) => ("is-skipped", icon(Icon::Close, 16)),
        };

        html! {
            <details class={classes!("tool-call", class)}>
                <summary>
                    <span class="tool-call-status">{ status }</span>
                    <span class="tool-call-name">{ tool_name(&call.name) }</span>
                    <span class="tool-call-target">{ self.target(&call.arguments) }</span>
                    if let Some(error) = error {
                        <span class="tool-call-error">{ error.to_string() }</span>
                    }
                </summary>
                <div class="tool-call-body">
                    <p class="tool-call-label">{ "Arguments" }</p>
                    <pre>{ pretty_json(&call.arguments) }</pre>
                    if let Some(result) = result {
                        <p class="tool-call-label">{ if error.is_some() { "Error" } else { "Result" } }</p>
                        <pre>{ pretty_json(error.unwrap_or(result)) }</pre>
                    }
                </div>
            </details>
        }
    }

    /// The assistant asks to run `call`.
    fn view_approval(&self, call: &ToolCall, link: &Scope<Self>) -> Html {
        let answer = |answer: Approval| {
            let call_id = call.id.clone();
            link.callback(move |_| Msg::Assistant(AssistantMsg::Answer(call_id.clone(), answer)))
        };
        let target = self.target(&call.arguments);

        html! {
            <div class="approval">
                <div class="approval-head">
                    <span class="approval-icon">{ icon(Icon::Alert, 18) }</span>
                    <div>
                        <p class="approval-title">
                            { format!("The assistant asks to run “{}”", tool_name(&call.name)) }
                        </p>
                        if !target.is_empty() {
                            <p class="approval-target">{ target }</p>
                        }
                    </div>
                </div>
                <pre class="approval-args">{ pretty_json(&call.arguments) }</pre>
                <div class="buttons">
                    <button class="button is-link" onclick={answer(Approval::Yes)}>{ "Allow" }</button>
                    <button class="button is-tinted" title="Allow it for the rest of this chat"
                        onclick={answer(Approval::Always)}>
                        { "Always allow" }
                    </button>
                    <button class="button is-danger is-light" onclick={answer(Approval::No)}>{ "Deny" }</button>
                </div>
            </div>
        }
    }

    fn view_composer(&self, link: &Scope<Self>) -> Html {
        let assistant = &self.assistant;
        let rows = assistant.draft.lines().count().clamp(1, 6);
        let busy = assistant.asking || assistant.chat.as_ref().is_some_and(|chat| chat.running);

        html! {
            <footer class="composer">
                <div class="composer-box">
                    <textarea class="composer-input" rows={rows.to_string()}
                        placeholder="Ask about your devices…"
                        value={assistant.draft.clone()}
                        oninput={link.callback(|e: InputEvent| Msg::Assistant(AssistantMsg::Draft(textarea_value(e))))}
                        onkeydown={link.batch_callback(|e: KeyboardEvent| {
                            (e.key() == "Enter" && !e.shift_key()).then(|| {
                                e.prevent_default();
                                Msg::Assistant(AssistantMsg::Send)
                            })
                        })} />
                    if busy {
                        <button class="composer-button is-stop" title="Stop the assistant"
                            disabled={!assistant.asking}
                            onclick={link.callback(|_| Msg::Assistant(AssistantMsg::Stop))}>
                            { icon(Icon::Stop, 18) }
                        </button>
                    } else {
                        <button class="composer-button" title="Send"
                            disabled={assistant.draft.trim().is_empty()}
                            onclick={link.callback(|_| Msg::Assistant(AssistantMsg::Send))}>
                            { icon(Icon::ArrowUp, 20) }
                        </button>
                    }
                </div>
                <p class="composer-hint">{ "Enter sends; Shift+Enter starts a new line." }</p>
            </footer>
        }
    }

    /// The device a tool call is about, and the network, as the user knows
    /// them.
    fn target(&self, arguments: &str) -> String {
        let Ok(arguments) = serde_json::from_str::<serde_json::Value>(arguments) else {
            return String::new();
        };

        let node_id = match &arguments["node_id"] {
            serde_json::Value::Number(number) => number.as_u64(),
            serde_json::Value::String(text) => {
                let text = text.trim();
                match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
                    Some(hex) => u64::from_str_radix(hex, 16).ok(),
                    None => text.parse().ok(),
                }
            }
            _ => None,
        };
        let device = node_id.map(|node_id| {
            self.stored
                .iter()
                .find(|d| d.handle.node_id == node_id)
                .map(|d| d.title())
                .unwrap_or_else(|| display::hex_u64(node_id))
        });

        [device, arguments["ssid"].as_str().map(str::to_string)]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ")
    }
}

/// A turn of the conversation, as shown.
enum Block<'a> {
    User(&'a str),
    /// The assistant's text and tool calls, up to the user's next question.
    Assistant(Vec<Part<'a>>),
}

enum Part<'a> {
    Text(&'a str),
    Call(&'a ToolCall),
}

fn blocks(messages: &[Message]) -> Vec<Block<'_>> {
    let mut blocks = Vec::new();

    for message in messages {
        match message {
            Message::User(text) => blocks.push(Block::User(text)),
            Message::Assistant { text, calls } => {
                let mut parts: Vec<Part> = Vec::new();
                if !text.is_empty() {
                    parts.push(Part::Text(text));
                }
                parts.extend(calls.iter().map(Part::Call));

                match blocks.last_mut() {
                    Some(Block::Assistant(turn)) => turn.extend(parts),
                    _ => blocks.push(Block::Assistant(parts)),
                }
            }
            Message::System(_) | Message::Tool { .. } => {}
        }
    }

    blocks
}

/// The model's Markdown, as HTML; HTML in it shows as text.
fn markdown(text: &str) -> Html {
    use pulldown_cmark::{html, Event, Options, Parser};

    let parser = Parser::new_ext(text, Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH).map(
        |event| match event {
            Event::Html(raw) | Event::InlineHtml(raw) => Event::Text(raw),
            other => other,
        },
    );
    let mut out = String::new();
    html::push_html(&mut out, parser);

    Html::from_html_unchecked(AttrValue::from(out))
}

/// `read_network` as "Read network".
fn tool_name(name: &str) -> String {
    let mut name = name.replace('_', " ");
    if let Some(first) = name.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    name
}

/// JSON text, indented; other text as it is.
fn pretty_json(text: &str) -> String {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|value| serde_json::to_string_pretty(&value).ok())
        .unwrap_or_else(|| text.to_string())
}

/// How long ago `secs` (since the Unix epoch) was.
fn ago(secs: u64) -> String {
    let now = (js_sys::Date::now() / 1000.0) as u64;

    match now.saturating_sub(secs) {
        0..60 => "just now".into(),
        since @ 60..3_600 => format!("{} min ago", since / 60),
        since @ 3_600..86_400 => format!("{} h ago", since / 3_600),
        86_400..172_800 => "yesterday".into(),
        since => format!("{} days ago", since / 86_400),
    }
}
