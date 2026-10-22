//! The agent loop against a scripted model, a fake controller and a listener
//! that records what the user would see.

use alloc::collections::VecDeque;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use alloc::{format, vec};
use core::cell::{Cell, RefCell};
use core::future::Future;
use core::pin::pin;
use core::task::{Context, Poll, Waker};

use super::*;
use crate::api::wire;

/// A model answering from a script, recording how it was asked.
struct Script {
    replies: RefCell<VecDeque<Reply>>,
    asked: RefCell<Vec<(usize, Think)>>,
}

impl Script {
    fn new(replies: Vec<Reply>) -> Self {
        Self {
            replies: RefCell::new(replies.into()),
            asked: RefCell::new(Vec::new()),
        }
    }
}

impl Provider for &Script {
    async fn complete(
        &self,
        messages: &[Message],
        _tools: &[ToolSpec],
        think: Think,
    ) -> Result<Reply, String> {
        self.asked.borrow_mut().push((messages.len(), think));
        self.replies
            .borrow_mut()
            .pop_front()
            .ok_or_else(|| "the script ran out".to_string())
    }
}

/// A controller that answers every request with `Ok`, recording them.
#[derive(Default)]
struct Controller {
    requests: RefCell<Vec<wire::Request>>,
}

impl Dispatch for &Controller {
    async fn dispatch(&self, request: wire::Request) -> Result<wire::Response, String> {
        self.requests.borrow_mut().push(request);
        Ok(wire::Response::Ok)
    }
}

/// What the user saw, and how they answer approvals.
struct User {
    approves: bool,
    seen: RefCell<Vec<String>>,
    asked: Cell<usize>,
    /// Stop the agent at the first tool result.
    stops: bool,
    stopped: Cell<bool>,
}

impl User {
    fn new(approves: bool) -> Self {
        Self {
            approves,
            seen: RefCell::new(Vec::new()),
            asked: Cell::new(0),
            stops: false,
            stopped: Cell::new(false),
        }
    }

    fn seen(&self) -> Vec<String> {
        self.seen.borrow().clone()
    }
}

impl AgentListener for User {
    fn on_text(&self, text: &str) {
        self.seen.borrow_mut().push(format!("text: {text}"));
    }

    fn on_tool_call(&self, call: &ToolCall) {
        self.seen.borrow_mut().push(format!("call: {}", call.name));
    }

    fn on_tool_result(&self, call: &ToolCall, _result: &str, is_error: bool) {
        let outcome = if is_error { "failed" } else { "ok" };
        self.seen
            .borrow_mut()
            .push(format!("result: {} {outcome}", call.name));
        if self.stops {
            self.stopped.set(true);
        }
    }

    fn stopped(&self) -> bool {
        self.stopped.get()
    }

    async fn approve(&self, _call: &ToolCall) -> bool {
        self.asked.set(self.asked.get() + 1);
        self.approves
    }
}

fn calls(calls: &[(&str, &str)]) -> Reply {
    Reply {
        calls: calls
            .iter()
            .enumerate()
            .map(|(index, (name, arguments))| ToolCall {
                id: format!("c{index}"),
                name: name.to_string(),
                arguments: arguments.to_string(),
            })
            .collect(),
        ..Reply::default()
    }
}

fn text(text: &str) -> Reply {
    Reply {
        text: text.into(),
        ..Reply::default()
    }
}

/// Run `future`, which never waits on anything here, to its end.
fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut cx = Context::from_waker(Waker::noop());

    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
            return output;
        }
    }
}

const READ_ONLY: Caps = Caps {
    allow_mutate: false,
};
const MUTATE: Caps = Caps { allow_mutate: true };

#[test]
fn tool_call_then_answer() {
    let script = Script::new(vec![
        calls(&[("peek_device", r#"{"node_id":"0x1B67B"}"#)]),
        text("It is up."),
    ]);
    let controller = Controller::default();
    let user = User::new(true);
    let mut agent = Agent::new(&script, &controller, READ_ONLY, Think::Default);

    block_on(agent.ask("How is the 2PM?", &user)).unwrap();

    assert_eq!(
        user.seen(),
        [
            "call: peek_device",
            "result: peek_device ok",
            "text: It is up."
        ]
    );
    assert!(matches!(
        controller.requests.borrow()[..],
        [wire::Request::PeekDevice { node_id: 112251 }]
    ));
    // Read-only: not asked for.
    assert_eq!(user.asked.get(), 0);
    // System, question, the call, its result, the answer.
    assert_eq!(agent.messages().len(), 5);
    assert!(matches!(&agent.messages()[3], Message::Tool { call_id, .. } if call_id == "c0"));
}

#[test]
fn a_change_runs_only_once_approved() {
    for approves in [false, true] {
        let script = Script::new(vec![
            calls(&[("set_tags", r#"{"node_id":1,"tags":["kitchen"]}"#)]),
            text("Done."),
        ]);
        let controller = Controller::default();
        let user = User::new(approves);
        let mut agent = Agent::new(&script, &controller, MUTATE, Think::Default);

        block_on(agent.ask("Tag it kitchen", &user)).unwrap();

        assert_eq!(user.asked.get(), 1);
        assert_eq!(controller.requests.borrow().len(), usize::from(approves));
        let Message::Tool { result, .. } = &agent.messages()[3] else {
            panic!("no tool result");
        };
        assert_eq!(result.contains("said no"), !approves, "{result}");
    }
}

#[test]
fn an_empty_reply_is_asked_again_without_thinking() {
    let script = Script::new(vec![text(""), text("Here it is.")]);
    let controller = Controller::default();
    let user = User::new(true);
    let mut agent = Agent::new(&script, &controller, READ_ONLY, Think::Default);

    block_on(agent.ask("Anything?", &user)).unwrap();

    assert_eq!(user.seen(), ["text: Here it is."]);
    // Asked again in place of the empty reply, without thinking.
    assert_eq!(
        script.asked.borrow()[..],
        [(2, Think::Default), (3, Think::Off)]
    );
    // Neither the empty reply nor the request for the answer stays.
    assert_eq!(
        agent.messages().last(),
        Some(&Message::Assistant {
            text: "Here it is.".into(),
            calls: Vec::new(),
        })
    );
    assert_eq!(agent.messages().len(), 3);
}

#[test]
fn bad_calls_are_answered_not_run() {
    let script = Script::new(vec![
        calls(&[("teleport", "{}"), ("identify", "{not json")]),
        text("Sorry."),
    ]);
    let controller = Controller::default();
    let user = User::new(true);
    let mut agent = Agent::new(&script, &controller, READ_ONLY, Think::Default);

    block_on(agent.ask("Do something", &user)).unwrap();

    assert!(controller.requests.borrow().is_empty());
    assert_eq!(
        user.seen(),
        [
            "call: teleport",
            "result: teleport failed",
            "call: identify",
            "result: identify failed",
            "text: Sorry."
        ]
    );
}

#[test]
fn gives_up_after_too_many_steps() {
    let script = Script::new(
        (0..MAX_STEPS)
            .map(|_| calls(&[("identify", r#"{"node_id":1}"#)]))
            .collect(),
    );
    let controller = Controller::default();
    let user = User::new(true);
    let mut agent = Agent::new(&script, &controller, READ_ONLY, Think::Default);

    assert!(block_on(agent.ask("Loop", &user)).is_err());
    assert_eq!(controller.requests.borrow().len(), MAX_STEPS);
}

#[test]
fn tools_as_the_caps_allow() {
    let script = Script::new(Vec::new());
    let controller = Controller::default();

    let read_only = Agent::new(&script, &controller, READ_ONLY, Think::Default);
    let asked: Vec<_> = read_only
        .tools()
        .iter()
        .filter(|tool| !tool.read_only)
        .map(|tool| tool.name.as_str())
        .collect();
    // Offered without changing anything, but acting on a device: asked first.
    assert_eq!(asked, ["identify"]);
    assert!(read_only
        .tools()
        .iter()
        .any(|tool| tool.name == "list_devices"));

    let mutate = Agent::new(&script, &controller, MUTATE, Think::Default);
    let set_tags = mutate.tools().iter().find(|tool| tool.name == "set_tags");
    assert!(set_tags.is_some_and(|tool| !tool.read_only));
}

#[test]
fn long_results_are_cut_at_a_character() {
    let text = "é".repeat(10);

    assert_eq!(clip(&text, 5), "éé");
    assert_eq!(clip(&text, 100), text);
}

#[test]
fn a_stop_ends_the_question_with_every_call_answered() {
    let script = Script::new(vec![
        calls(&[
            ("peek_device", r#"{"node_id":1}"#),
            ("read_network", r#"{"node_id":1}"#),
        ]),
        text("Never asked."),
    ]);
    let controller = Controller::default();
    let mut user = User::new(true);
    user.stops = true;
    let mut agent = Agent::new(&script, &controller, READ_ONLY, Think::Default);

    assert_eq!(block_on(agent.ask("Look", &user)), Err(STOPPED.into()));

    assert_eq!(controller.requests.borrow().len(), 1);
    assert_eq!(script.asked.borrow().len(), 1);
    let Some(Message::Tool { call_id, result }) = agent.messages().last() else {
        panic!("the second call is not answered");
    };
    assert_eq!(call_id, "c1");
    assert!(result.contains(NOT_RUN), "{result}");
}

#[test]
fn a_kept_conversation_goes_on() {
    let script = Script::new(vec![text("Still here.")]);
    let controller = Controller::default();
    let user = User::new(true);
    let mut agent = Agent::new(&script, &controller, READ_ONLY, Think::Default);

    agent.restore(vec![
        Message::System("an old prompt".into()),
        Message::User("Hi".into()),
        Message::Assistant {
            text: "Hello.".into(),
            calls: Vec::new(),
        },
    ]);
    assert!(
        matches!(&agent.messages()[0], Message::System(prompt) if prompt.starts_with(SYSTEM_PROMPT))
    );
    assert_eq!(agent.history().len(), 2);

    block_on(agent.ask("And now?", &user)).unwrap();
    assert_eq!(agent.history().len(), 4);
    // The model got the whole conversation.
    assert_eq!(script.asked.borrow()[0].0, 4);
}

#[test]
fn messages_kept_as_json() {
    let messages = vec![
        Message::User("Hi".into()),
        Message::Assistant {
            text: String::new(),
            calls: vec![ToolCall {
                id: "c0".into(),
                name: "list_devices".into(),
                arguments: "{}".into(),
            }],
        },
    ];

    let json = serde_json::to_value(&messages).unwrap();
    assert_eq!(
        json[0],
        serde_json::json!({ "role": "user", "content": "Hi" })
    );
    assert_eq!(json[1]["role"], "assistant");
    assert_eq!(json[1]["content"]["calls"][0]["name"], "list_devices");

    let back: Vec<Message> = serde_json::from_value(json).unwrap();
    assert_eq!(back, messages);
}
