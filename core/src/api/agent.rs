//! An AI agent on the API: a model, behind an LLM [`Provider`], answers the
//! user, calling the [`mcp`] tools to look things up and to change them. A tool
//! the tool layer does not mark read-only runs only once the user approves it.
//!
//! The loop and the messages are `no_std`. The platform gives a provider its
//! [`Http`] (see [`openai::OpenAi`]); the shell is the [`AgentListener`]: what
//! the user sees, and answers.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::future::Future;

use serde_json::{json, Value};

use super::mcp::{self, Caps, Dispatch};

pub mod openai;

/// How many model calls one question may take.
const MAX_STEPS: usize = 16;

/// How much of a tool's result the model reads, in bytes.
const RESULT_MAX: usize = 12_000;

/// What the agent is, and how it works; the tools' own instructions follow.
pub const SYSTEM_PROMPT: &str = "\
You are the assistant of matter-peek, a local Matter smart-home controller. \
You inspect, diagnose and repair the user's Matter devices only through the \
tools you are given.

- Look things up with the tools; never guess the state of a device.
- Call only the tools the current question needs; do not repeat earlier \
actions unless the user asks for them again.
- Name devices by the node ids the tools give; never convert numbers between \
hex and decimal yourself.
- Before a tool that changes something, say in one sentence what it will do.
- When a tool fails, tell the user in plain words what failed and why (its \
error says), and what they can do about it.
- Answer briefly and plainly, in Markdown for a human: short paragraphs, \
**bold** for the key facts, a table when you show several devices or values. \
Name devices by their product name. Never paste raw JSON.";

/// What the agent asks a model that ended with an empty reply (some put their
/// answer into their thinking).
const ANSWER_NOW: &str = "Write your answer to me now, in Markdown, without calling more tools.";

/// A tool call of the model: the tool, and its arguments as JSON text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// A message of the conversation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    System(String),
    User(String),
    Assistant { text: String, calls: Vec<ToolCall> },
    Tool { call_id: String, result: String },
}

/// A tool, as offered to the model.
#[derive(Clone, Debug)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// Its arguments, as JSON Schema.
    pub parameters: Value,
    /// Whether it only reads, and so runs without asking.
    pub read_only: bool,
}

/// What a model answered.
#[derive(Clone, Debug, Default)]
pub struct Reply {
    /// Its text for the user, Markdown.
    pub text: String,
    /// Its thinking, where the provider returns it.
    pub thinking: String,
    pub calls: Vec<ToolCall>,
    pub usage: Usage,
}

/// The tokens a model call took, where the provider tells.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

/// Whether a model is to think before it answers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Think {
    /// As the model does by default.
    #[default]
    Default,
    /// Not at all: faster, less careful.
    Off,
}

/// A model behind an LLM API.
pub trait Provider {
    /// The model's reply to `messages`, offered `tools`.
    fn complete(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        think: Think,
    ) -> impl Future<Output = Result<Reply, String>>;
}

/// The HTTP a provider needs, from the platform.
pub trait Http {
    /// POST the JSON `body` to `url` with `headers`: the response body, or why
    /// not (an error status comes with its body).
    fn post(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        body: String,
    ) -> impl Future<Output = Result<String, String>>;
}

/// The user's side of the agent: what it is told as the agent goes, and what
/// it is asked.
pub trait AgentListener {
    /// The model is being asked.
    fn on_model_call(&self) {}

    /// What the model call took.
    fn on_usage(&self, _usage: Usage) {}

    /// The model's thinking, where the provider returns it.
    fn on_thinking(&self, _thinking: &str) {}

    /// The model's text for the user, Markdown.
    fn on_text(&self, text: &str);

    /// A tool is about to run (or be asked for).
    fn on_tool_call(&self, call: &ToolCall);

    /// A tool ran: its result (the error, if it failed).
    fn on_tool_result(&self, call: &ToolCall, result: &str, is_error: bool);

    /// Whether to run `call`, a tool that may change something.
    fn approve(&self, call: &ToolCall) -> impl Future<Output = bool>;
}

/// A conversation of the user with a model that uses the tools.
pub struct Agent<P, D> {
    provider: P,
    dispatch: D,
    caps: Caps,
    think: Think,
    tools: Vec<ToolSpec>,
    system: String,
    messages: Vec<Message>,
}

impl<P: Provider, D: Dispatch> Agent<P, D> {
    /// An agent asking `provider`, running the tools `caps` allows through
    /// `dispatch`.
    pub fn new(provider: P, dispatch: D, caps: Caps, think: Think) -> Self {
        let tools = mcp::tool_list(caps).iter().map(tool_spec).collect();

        let mut agent = Self {
            provider,
            dispatch,
            caps,
            think,
            tools,
            system: String::new(),
            messages: Vec::new(),
        };
        agent.set_system_prompt(SYSTEM_PROMPT);

        agent
    }

    /// Replace [`SYSTEM_PROMPT`] with `prompt` (the tools' instructions stay),
    /// starting over.
    pub fn set_system_prompt(&mut self, prompt: &str) {
        self.system = format!("{prompt}\n\nAbout the tools:\n{}", mcp::INSTRUCTIONS);
        self.reset();
    }

    /// Forget the conversation.
    pub fn reset(&mut self) {
        self.messages = alloc::vec![Message::System(self.system.clone())];
    }

    /// The tools the model is offered.
    pub fn tools(&self) -> &[ToolSpec] {
        &self.tools
    }

    /// The conversation so far.
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Ask `question`, telling `listener` how the agent goes until the model
    /// answers.
    pub async fn ask<L: AgentListener>(
        &mut self,
        question: &str,
        listener: &L,
    ) -> Result<(), String> {
        self.messages.push(Message::User(question.to_string()));

        for _ in 0..MAX_STEPS {
            listener.on_model_call();
            let reply = self
                .provider
                .complete(&self.messages, &self.tools, self.think)
                .await?;
            listener.on_usage(reply.usage);

            if !reply.thinking.is_empty() {
                listener.on_thinking(&reply.thinking);
            }
            if !reply.text.is_empty() {
                listener.on_text(&reply.text);
            }

            self.messages.push(Message::Assistant {
                text: reply.text.clone(),
                calls: reply.calls.clone(),
            });

            if reply.calls.is_empty() {
                return if reply.text.is_empty() {
                    self.ask_for_answer(listener).await
                } else {
                    Ok(())
                };
            }

            for call in reply.calls {
                let result = self.run(&call, listener).await;
                self.messages.push(Message::Tool {
                    call_id: call.id,
                    result,
                });
            }
        }

        Err(format!("the model gave no answer in {MAX_STEPS} steps"))
    }

    /// After an empty reply, ask once more, without thinking, for the answer
    /// itself.
    async fn ask_for_answer<L: AgentListener>(&mut self, listener: &L) -> Result<(), String> {
        // The empty reply goes; so does the request, once answered.
        self.messages.pop();
        self.messages.push(Message::User(ANSWER_NOW.to_string()));

        listener.on_model_call();
        let reply = self
            .provider
            .complete(&self.messages, &self.tools, Think::Off)
            .await;
        self.messages.pop();
        let reply = reply?;
        listener.on_usage(reply.usage);

        if reply.text.is_empty() {
            return Err("the model ended without an answer".into());
        }

        listener.on_text(&reply.text);
        self.messages.push(Message::Assistant {
            text: reply.text,
            calls: Vec::new(),
        });

        Ok(())
    }

    /// Run `call`, if it may: its result for the model.
    async fn run<L: AgentListener>(&self, call: &ToolCall, listener: &L) -> String {
        listener.on_tool_call(call);

        let (result, is_error) = self.result(call, listener).await;
        listener.on_tool_result(call, &result, is_error);

        let clipped = clip(&result, RESULT_MAX);
        let cut = if clipped.len() < result.len() {
            " … (cut: too long)"
        } else {
            ""
        };

        if is_error {
            format!("ERROR: {clipped}{cut}")
        } else {
            format!("{clipped}{cut}")
        }
    }

    async fn result<L: AgentListener>(&self, call: &ToolCall, listener: &L) -> (String, bool) {
        let Some(tool) = self.tools.iter().find(|tool| tool.name == call.name) else {
            return (format!("there is no tool named {}", call.name), true);
        };

        let arguments = if call.arguments.trim().is_empty() {
            json!({})
        } else {
            match serde_json::from_str::<Value>(&call.arguments) {
                Ok(arguments) => arguments,
                Err(e) => return (format!("the arguments are not valid JSON: {e}"), true),
            }
        };

        if !tool.read_only && !listener.approve(call).await {
            return (
                "the user said no to running this tool now; it is allowed, but ask the user \
                 before trying it again"
                    .into(),
                true,
            );
        }

        let params = json!({ "name": call.name, "arguments": arguments });
        match mcp::tools_call(params, &self.dispatch, self.caps).await {
            Ok(result) => (result_text(&result), result["isError"] == json!(true)),
            Err((_, message)) => (message, true),
        }
    }
}

/// A tool of `tools/list`, for a model.
fn tool_spec(tool: &Value) -> ToolSpec {
    let text = |key: &str| tool[key].as_str().unwrap_or_default().to_string();

    ToolSpec {
        name: text("name"),
        description: text("description"),
        parameters: tool["inputSchema"].clone(),
        read_only: tool["annotations"]["readOnlyHint"] == json!(true),
    }
}

/// The text of a `tools/call` result.
fn result_text(result: &Value) -> String {
    result["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|content| content["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// At most `max` bytes of `text`, cut at a character.
fn clip(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }

    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests;
