//! The assistant: its chats, conversations with a model (see
//! [`matter_peek_core::api::agent`]) run here against the controller and kept
//! as JSON files in the data directory, and its model, kept in the
//! controller's store (or set by the environment, see
//! [`crate::llm::model_from_env`]). Its key never leaves this process.
//!
//! What the assistant does goes, as [`Event::Chat`]s, to the connection that
//! asked; its requests to run tools are answered by any connection
//! ([`Request::ChatApprove`]), so are stops.

use core::cell::{Cell, RefCell};

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_io::Timer;
use futures_lite::FutureExt as _;
use log::{info, warn};
use serde::{Deserialize, Serialize};

use matter_peek_core::api::agent::openai::OpenAi;
use matter_peek_core::api::agent::{
    Agent, AgentListener, Message, ModelConfig, Provider as _, Think, ToolCall, Usage,
};
use matter_peek_core::api::mcp::{Caps, Dispatch};
use matter_peek_core::api::wire::{
    Approval, ChatEvent, ChatSummary, ChatView, ClientMsg, Event, Request, Response, ServerMsg,
    WireSession,
};
use matter_peek_core::api::{AssistantUpdate, FabricRef, MatterPeek};

use crate::llm::Ureq;

/// How long the assistant waits for its request to run a tool to be answered;
/// then it takes it as a no.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// How long a chat's title, from its first question, may be.
const TITLE_MAX: usize = 60;

/// The assistant's chats, and its model.
pub(crate) struct Chats {
    /// The model the environment sets, over the one in the store.
    env: Option<ModelConfig>,
    http: Ureq,
    peek: Arc<MatterPeek>,
    dir: PathBuf,
    chats: RefCell<BTreeMap<String, Chat>>,
    next_id: Cell<u32>,
}

/// A chat, as kept.
#[derive(Clone, Serialize, Deserialize)]
struct Record {
    id: String,
    title: String,
    created: u64,
    updated: u64,
    allow_changes: bool,
    messages: Vec<Message>,
}

/// A chat, as kept and as it goes.
struct Chat {
    record: Record,
    running: bool,
    stop: bool,
    /// The tools approved for the rest of the chat.
    always: BTreeSet<String>,
    /// The tool the assistant asks to run, and where its answer goes.
    pending: Option<(ToolCall, async_channel::Sender<Approval>)>,
}

impl Chats {
    /// The chats kept in `dir`, run with the model `env` sets, else with the
    /// one in `peek`'s store.
    pub(crate) fn new(peek: Arc<MatterPeek>, env: Option<ModelConfig>, dir: PathBuf) -> Rc<Self> {
        let mut chats = BTreeMap::new();

        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_none_or(|ext| ext != "json") {
                    continue;
                }

                match std::fs::read(&path)
                    .map_err(|e| e.to_string())
                    .and_then(|json| {
                        serde_json::from_slice::<Record>(&json).map_err(|e| e.to_string())
                    }) {
                    Ok(record) => {
                        chats.insert(
                            record.id.clone(),
                            Chat {
                                record,
                                running: false,
                                stop: false,
                                always: BTreeSet::new(),
                                pending: None,
                            },
                        );
                    }
                    Err(e) => warn!("chats: cannot read {}: {e}", path.display()),
                }
            }
        }

        info!("chats: {} in {}", chats.len(), dir.display());
        if let Some(env) = &env {
            info!(
                "chats: the environment sets the assistant's model: {} at {}",
                env.model, env.url
            );
        }

        Rc::new(Self {
            env,
            http: Ureq::new(),
            peek,
            dir,
            chats: RefCell::new(chats),
            next_id: Cell::new(0),
        })
    }

    /// Run the chat request of `msg`, sending its events and then its response
    /// to `out`.
    pub(crate) async fn handle(
        self: Rc<Self>,
        msg: ClientMsg,
        out: async_channel::Sender<ServerMsg>,
    ) {
        let response = match msg.request {
            Request::ChatList => Ok(self.list().await),
            Request::ChatOpen { chat_id } => self.view(&chat_id),
            Request::ChatAsk {
                chat_id,
                text,
                allow_changes,
                fabric,
            } => self.ask(chat_id, text, allow_changes, fabric, &out).await,
            Request::ChatApprove {
                chat_id,
                call_id,
                answer,
            } => self.approve(&chat_id, &call_id, answer),
            Request::ChatStop { chat_id } => self.stop(&chat_id),
            Request::ChatDelete { chat_id } => self.delete(&chat_id).await,
            Request::Assistant => self.assistant().await,
            Request::SetAssistant { update } => self.set_assistant(update).await,
            Request::ClearAssistant => self.clear_assistant().await,
            Request::AssistantModels { update } => self.models(update).await,
            Request::AssistantTest { update } => self.test(update).await,
            _ => Err("not a request for the assistant".into()),
        };

        let response = response.unwrap_or_else(|message| Response::Error { message });
        let _ = out
            .send(ServerMsg::Response {
                req_id: msg.req_id,
                response,
            })
            .await;
    }

    async fn list(&self) -> Response {
        let model = self.stored_or_env().await.ok().flatten();

        let mut chats: Vec<ChatSummary> = self
            .chats
            .borrow()
            .values()
            .map(|chat| ChatSummary {
                id: chat.record.id.clone(),
                title: chat.record.title.clone(),
                updated: chat.record.updated,
            })
            .collect();
        chats.sort_by_key(|chat| core::cmp::Reverse(chat.updated));

        Response::Chats {
            model: model.map(|model| model.model),
            chats,
        }
    }

    /// The model the environment sets, else the one in the store, if any.
    async fn stored_or_env(&self) -> Result<Option<ModelConfig>, String> {
        match &self.env {
            Some(env) => Ok(Some(env.clone())),
            None => self.peek.assistant_model().await.map_err(|e| e.to_string()),
        }
    }

    /// The model, to ask.
    async fn model(&self) -> Result<ModelConfig, String> {
        self.stored_or_env().await?.ok_or_else(|| {
            "there is no assistant yet: give it a model on the Assistant page, \
             or with `matter-peek assistant set`"
                .into()
        })
    }

    /// The model as it would be after `update`; without one, as it is.
    async fn candidate(&self, update: Option<AssistantUpdate>) -> Result<ModelConfig, String> {
        match update {
            Some(update) => {
                let current = self.stored_or_env().await?;
                ModelConfig::updated(current.as_ref(), update)
            }
            None => self.model().await,
        }
    }

    async fn assistant(&self) -> Result<Response, String> {
        let assistant = match &self.env {
            Some(env) => Some(env.info(true)),
            None => self.peek.assistant().await.map_err(|e| e.to_string())?,
        };

        Ok(Response::Assistant { assistant })
    }

    async fn set_assistant(&self, update: AssistantUpdate) -> Result<Response, String> {
        self.not_from_env()?;

        let info = self
            .peek
            .set_assistant(update)
            .await
            .map_err(|e| e.to_string())?;
        info!(
            "chats: the assistant's model is now {} at {}",
            info.model, info.url
        );

        Ok(Response::Assistant {
            assistant: Some(info),
        })
    }

    async fn clear_assistant(&self) -> Result<Response, String> {
        self.not_from_env()?;

        self.peek
            .clear_assistant()
            .await
            .map_err(|e| e.to_string())?;
        info!("chats: the assistant has no model now");

        Ok(Response::Assistant { assistant: None })
    }

    fn not_from_env(&self) -> Result<(), String> {
        match self.env {
            Some(_) => Err("the environment sets the assistant's model \
                            (MATTER_PEEK_LLM_URL, MATTER_PEEK_LLM_MODEL): \
                            change it there, or unset it to change it here"
                .into()),
            None => Ok(()),
        }
    }

    async fn models(&self, update: Option<AssistantUpdate>) -> Result<Response, String> {
        let candidate = self.candidate(update).await?;
        let api = OpenAi::new(self.http.clone(), &candidate.url, "", candidate.api_key);

        Ok(Response::Models {
            models: api.models().await?,
        })
    }

    async fn test(&self, update: Option<AssistantUpdate>) -> Result<Response, String> {
        let candidate = self.candidate(update).await?;
        if candidate.model.is_empty() {
            return Err("name the model".into());
        }

        let api = OpenAi::new(
            self.http.clone(),
            &candidate.url,
            &candidate.model,
            candidate.api_key,
        );
        let messages = [
            Message::System("Answer in one short sentence.".into()),
            Message::User("Say hello to the user of matter-peek.".into()),
        ];

        let started = Instant::now();
        let reply = api.complete(&messages, &[], Think::Off).await?;
        let reply = if reply.text.is_empty() {
            reply.thinking
        } else {
            reply.text
        };

        Ok(Response::Tested {
            model: candidate.model,
            reply,
            millis: started.elapsed().as_millis() as u64,
        })
    }

    fn view(&self, chat_id: &str) -> Result<Response, String> {
        let chats = self.chats.borrow();
        let chat = chats.get(chat_id).ok_or_else(|| no_chat(chat_id))?;

        Ok(Response::Chat {
            chat: Box::new(ChatView {
                id: chat.record.id.clone(),
                title: chat.record.title.clone(),
                updated: chat.record.updated,
                allow_changes: chat.record.allow_changes,
                running: chat.running,
                messages: chat.record.messages.clone(),
                pending: chat.pending.as_ref().map(|(call, _)| call.clone()),
            }),
        })
    }

    async fn ask(
        &self,
        chat_id: Option<String>,
        text: String,
        allow_changes: bool,
        fabric: FabricRef,
        out: &async_channel::Sender<ServerMsg>,
    ) -> Result<Response, String> {
        let llm = self.model().await?;
        let text = text.trim();
        if text.is_empty() {
            return Err("ask something".into());
        }

        let chat_id = match chat_id {
            Some(chat_id) => chat_id,
            None => self.create(text),
        };

        let history = {
            let mut chats = self.chats.borrow_mut();
            let chat = chats.get_mut(&chat_id).ok_or_else(|| no_chat(&chat_id))?;
            if chat.running {
                return Err("the assistant is at work in this chat already".into());
            }

            chat.running = true;
            chat.stop = false;
            chat.record.allow_changes = allow_changes;
            let history = chat.record.messages.clone();
            // The question is kept even if the server stops before the answer.
            chat.record.messages.push(Message::User(text.to_string()));
            chat.record.updated = now();
            history
        };
        self.save(&chat_id);

        let provider = OpenAi::new(self.http.clone(), &llm.url, &llm.model, llm.api_key);
        let caps = Caps {
            allow_mutate: allow_changes,
        };
        let mut agent = Agent::new(
            provider,
            Tools::new(self.peek.clone(), fabric),
            caps,
            llm.think,
        );
        agent.restore(history);

        let listener = Listener {
            chats: self,
            chat_id: chat_id.clone(),
            out: out.clone(),
        };
        if let Err(message) = agent.ask(text, &listener).await {
            listener.send(ChatEvent::Error { message });
        }

        {
            let mut chats = self.chats.borrow_mut();
            if let Some(chat) = chats.get_mut(&chat_id) {
                chat.record.messages = agent.history().to_vec();
                chat.record.updated = now();
                chat.running = false;
                chat.stop = false;
                chat.pending = None;
            }
        }
        self.save(&chat_id);

        self.view(&chat_id)
    }

    /// A new chat, titled after its first question: its id.
    fn create(&self, question: &str) -> String {
        let id = self.next_id.get();
        self.next_id.set(id.wrapping_add(1));

        let now = now();
        let id = format!("{now:x}{id:02x}");
        let record = Record {
            id: id.clone(),
            title: title(question),
            created: now,
            updated: now,
            allow_changes: false,
            messages: Vec::new(),
        };

        self.chats.borrow_mut().insert(
            id.clone(),
            Chat {
                record,
                running: false,
                stop: false,
                always: BTreeSet::new(),
                pending: None,
            },
        );

        id
    }

    fn approve(&self, chat_id: &str, call_id: &str, answer: Approval) -> Result<Response, String> {
        let mut chats = self.chats.borrow_mut();
        let chat = chats.get_mut(chat_id).ok_or_else(|| no_chat(chat_id))?;

        match chat.pending.take() {
            Some((call, reply)) if call.id == call_id => {
                if answer == Approval::Always {
                    chat.always.insert(call.name);
                }
                let _ = reply.try_send(answer);
                Ok(Response::Ok)
            }
            pending => {
                chat.pending = pending;
                Err("the assistant asks nothing of that kind now".into())
            }
        }
    }

    fn stop(&self, chat_id: &str) -> Result<Response, String> {
        let mut chats = self.chats.borrow_mut();
        let chat = chats.get_mut(chat_id).ok_or_else(|| no_chat(chat_id))?;

        chat.stop = true;
        // A request to run a tool is a no.
        if let Some((_, reply)) = chat.pending.take() {
            let _ = reply.try_send(Approval::No);
        }

        Ok(Response::Ok)
    }

    async fn delete(&self, chat_id: &str) -> Result<Response, String> {
        {
            let mut chats = self.chats.borrow_mut();
            let chat = chats.get(chat_id).ok_or_else(|| no_chat(chat_id))?;
            if chat.running {
                return Err("the assistant is at work in this chat: stop it first".into());
            }
            chats.remove(chat_id);
        }

        if let Err(e) = std::fs::remove_file(self.path(chat_id)) {
            if e.kind() != std::io::ErrorKind::NotFound {
                warn!("chats: cannot delete chat {chat_id}: {e}");
            }
        }

        Ok(self.list().await)
    }

    fn save(&self, chat_id: &str) {
        let Some(record) = self
            .chats
            .borrow()
            .get(chat_id)
            .map(|chat| chat.record.clone())
        else {
            return;
        };

        let saved = std::fs::create_dir_all(&self.dir)
            .and_then(|()| serde_json::to_vec(&record).map_err(std::io::Error::other))
            .and_then(|json| std::fs::write(self.path(chat_id), json));
        if let Err(e) = saved {
            warn!("chats: cannot keep chat {chat_id}: {e}");
        }
    }

    fn path(&self, chat_id: &str) -> PathBuf {
        self.dir.join(format!("{chat_id}.json"))
    }
}

/// The user's side of a chat's agent: its events to the connection that asked,
/// its requests to run tools to whoever answers.
struct Listener<'a> {
    chats: &'a Chats,
    chat_id: String,
    out: async_channel::Sender<ServerMsg>,
}

impl Listener<'_> {
    fn send(&self, update: ChatEvent) {
        let _ = self.out.try_send(ServerMsg::Event {
            event: Event::Chat {
                chat_id: self.chat_id.clone(),
                update,
            },
        });
    }
}

impl AgentListener for Listener<'_> {
    fn on_model_call(&self) {
        self.send(ChatEvent::Thinking);
    }

    fn on_usage(&self, usage: Usage) {
        self.send(ChatEvent::Usage {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
        });
    }

    fn on_thinking(&self, thinking: &str) {
        self.send(ChatEvent::Thought {
            text: thinking.into(),
        });
    }

    fn on_text(&self, text: &str) {
        self.send(ChatEvent::Text { text: text.into() });
    }

    fn on_tool_call(&self, call: &ToolCall) {
        self.send(ChatEvent::ToolCall { call: call.clone() });
    }

    fn on_tool_result(&self, call: &ToolCall, result: &str, is_error: bool) {
        self.send(ChatEvent::ToolResult {
            call_id: call.id.clone(),
            result: result.into(),
            is_error,
        });
    }

    async fn approve(&self, call: &ToolCall) -> bool {
        let (reply, answer) = async_channel::bounded(1);
        {
            let mut chats = self.chats.chats.borrow_mut();
            let Some(chat) = chats.get_mut(&self.chat_id) else {
                return false;
            };
            if chat.always.contains(&call.name) {
                return true;
            }
            if chat.stop {
                return false;
            }
            chat.pending = Some((call.clone(), reply));
        }

        self.send(ChatEvent::Approve { call: call.clone() });

        let answer = async { answer.recv().await.ok() }
            .or(async {
                Timer::after(APPROVAL_TIMEOUT).await;
                None
            })
            .await;

        if let Some(chat) = self.chats.chats.borrow_mut().get_mut(&self.chat_id) {
            chat.pending = None;
        }

        matches!(answer, Some(Approval::Yes | Approval::Always))
    }

    fn stopped(&self) -> bool {
        self.chats
            .chats
            .borrow()
            .get(&self.chat_id)
            .is_none_or(|chat| chat.stop)
    }
}

/// The assistant's tools, run against the server's controller in process,
/// about `fabric` unless one names another.
struct Tools {
    session: WireSession,
    fabric: FabricRef,
}

impl Tools {
    fn new(peek: Arc<MatterPeek>, fabric: FabricRef) -> Self {
        // Nobody listens to the events of the tools' requests.
        let (out, _) = async_channel::unbounded();
        Self {
            session: WireSession::new(peek, out),
            fabric,
        }
    }
}

impl Dispatch for Tools {
    async fn dispatch(&self, mut request: Request) -> Result<Response, String> {
        request.default_fabric(&self.fabric);

        Ok(self.session.answer(request).await)
    }
}

/// A chat's title, from its first question.
fn title(question: &str) -> String {
    let question = question.split_whitespace().collect::<Vec<_>>().join(" ");
    if question.chars().count() <= TITLE_MAX {
        return question;
    }

    let cut: String = question.chars().take(TITLE_MAX).collect();
    let cut = cut.rsplit_once(' ').map_or(cut.as_str(), |(head, _)| head);
    format!("{cut}…")
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

fn no_chat(chat_id: &str) -> String {
    format!("there is no chat {chat_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_from_questions() {
        assert_eq!(
            title("  Which   devices\nare offline? "),
            "Which devices are offline?"
        );

        let long = title(&"word ".repeat(30));
        assert!(long.ends_with('…'), "{long}");
        assert!(long.chars().count() <= TITLE_MAX + 1, "{long}");
        assert!(!long.contains("wor…"), "cut inside a word: {long}");
    }
}
