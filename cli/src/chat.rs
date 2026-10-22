//! `matter-peek chat`: ask the assistant about the devices, in a chat. Its
//! model looks into them with the API's tools, and changes them only with
//! `--allow-mutate` and once the user approves each change (see
//! [`matter_peek_core::api::agent`]).
//!
//! The assistant runs where its model's key is kept: in the server with
//! `--server`, else in this process. Either way, the chat is one of its chats,
//! kept like the web UI's.

use core::cell::Cell;

use std::io::{self, BufRead as _, IsTerminal as _, Write as _};
use std::path::PathBuf;
use std::time::Instant;

use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;
use termimad::MadSkin;

use matter_peek_core::api::agent::{self, ToolCall, STOPPED};
use matter_peek_core::api::mcp::Caps;
use matter_peek_core::api::platform::data_dir;
use matter_peek_core::api::wire::{Approval, ChatEvent, Event, Request, Response, ServerMsg};
use matter_peek_server::client::Connection;

use crate::mcp;

/// The chat's history file, in the data directory.
const HISTORY_FILE: &str = "chat-history";

/// How many lines the history keeps.
const HISTORY_MAX: usize = 1000;

/// How much of a tool's result `--verbose` shows, in bytes.
const SHOWN_RESULT_MAX: usize = 800;

/// The chat's manners.
pub struct ChatArgs {
    /// Ask this, answer, and end.
    pub prompt: Option<String>,
    pub show_thinking: bool,
    pub verbose: bool,
    pub allow_mutate: bool,
    /// The fabric of ours the assistant's tools are about, unless asked for
    /// another.
    pub fabric: matter_peek_core::api::FabricRef,
}

/// Chat with the assistant of the server at `server`, else of a controller
/// started in this process; `false` if there is no assistant.
pub async fn chat(server: Option<String>, args: ChatArgs) -> io::Result<bool> {
    let terminal = Terminal::new(args.show_thinking, args.verbose);
    if server.is_none() {
        terminal.note("Looking for the devices on our fabric…");
    }
    let dispatch = mcp::dispatch(server, args.fabric.clone()).await?;
    let conn = dispatch.connection().await.map_err(io::Error::other)?;

    let assistant = match conn.call(Request::Assistant, |_| ()).await? {
        Response::Assistant {
            assistant: Some(assistant),
        } => assistant,
        Response::Assistant { assistant: None } => {
            eprintln!(
                "error: there is no assistant yet; give it a model, e.g.\n  \
                 matter-peek assistant set --url http://localhost:11434/v1 --model qwen3.5:9b"
            );
            return Ok(false);
        }
        Response::Error { message } => {
            eprintln!("error: {message}");
            return Ok(false);
        }
        _ => return Err(io::Error::other("an unexpected answer")),
    };

    let tools = agent::tools(Caps {
        allow_mutate: args.allow_mutate,
    });
    let changing = tools.iter().filter(|tool| !tool.read_only).count();
    terminal.note(&format!(
        "{} at {}: {} tools{}",
        assistant.model,
        assistant.url,
        tools.len(),
        if changing > 0 {
            format!(", {changing} of which are asked for first")
        } else {
            String::new()
        }
    ));

    let mut chat = Chat {
        conn: &conn,
        id: None,
        allow_changes: args.allow_mutate,
        fabric: args.fabric.clone(),
        terminal: &terminal,
    };

    if let Some(prompt) = args.prompt {
        chat.ask(&prompt).await?;
        return Ok(true);
    }

    terminal.note(
        "Ask away. /new starts a new chat, /tools lists the tools, /quit ends. \
         The chats are kept, as the web UI's.",
    );

    let mut prompt = Prompt::new();

    loop {
        println!();
        let Some(line) = prompt.read().await? else {
            return Ok(true);
        };

        match line.trim() {
            "" => {}
            "/quit" | "/exit" => return Ok(true),
            "/new" | "/reset" => {
                chat.id = None;
                terminal.note("A new chat.");
            }
            "/tools" => {
                for tool in &tools {
                    let changes = if tool.read_only {
                        ""
                    } else {
                        "  (asked first)"
                    };
                    println!("  {}{changes}", tool.name);
                }
            }
            question => chat.ask(question).await?,
        }
    }
}

/// A chat of the assistant's, in this terminal.
struct Chat<'a> {
    conn: &'a Connection,
    /// `None` until the assistant starts a new one.
    id: Option<String>,
    allow_changes: bool,
    /// The fabric of ours the tools are about, unless asked for another.
    fabric: matter_peek_core::api::FabricRef,
    terminal: &'a Terminal,
}

impl Chat<'_> {
    /// Ask `question`, showing what the assistant does until it answers.
    async fn ask(&mut self, question: &str) -> io::Result<()> {
        let req_id = self
            .conn
            .send(Request::ChatAsk {
                chat_id: self.id.clone(),
                text: question.into(),
                allow_changes: self.allow_changes,
                fabric: self.fabric.clone(),
            })
            .await?;

        loop {
            let Some(msg) = self.conn.recv().await else {
                return Err(io::Error::other("the connection closed"));
            };

            match msg {
                ServerMsg::Event {
                    event: Event::Chat { chat_id, update },
                } => {
                    // A new chat's first word names it.
                    let id = self.id.get_or_insert_with(|| chat_id.clone());
                    if *id == chat_id {
                        self.show(update).await?;
                    }
                }
                ServerMsg::Response {
                    req_id: id,
                    response,
                } if id == req_id => {
                    match response {
                        Response::Chat { chat } => self.id = Some(chat.id),
                        Response::Error { message } => {
                            self.terminal.done_waiting();
                            println!("{}", self.terminal.error(&format!("  {message}")));
                        }
                        _ => {}
                    }
                    return Ok(());
                }
                _ => {}
            }
        }
    }

    async fn show(&self, update: ChatEvent) -> io::Result<()> {
        let terminal = self.terminal;

        match update {
            ChatEvent::Thinking => terminal.model_call(),
            ChatEvent::Thought { text } => terminal.thinking(&text),
            ChatEvent::Usage {
                input_tokens,
                output_tokens,
            } => terminal.usage(input_tokens, output_tokens),
            ChatEvent::Text { text } => terminal.text(&text),
            ChatEvent::ToolCall { call } => terminal.tool_call(&call),
            ChatEvent::ToolResult {
                result, is_error, ..
            } => terminal.tool_result(&result, is_error),
            ChatEvent::Approve { call } => {
                let answer = terminal.approve(&call).await;
                if let Some(chat_id) = self.id.clone() {
                    self.conn
                        .send(Request::ChatApprove {
                            chat_id,
                            call_id: call.id,
                            answer,
                        })
                        .await?;
                }
            }
            ChatEvent::Error { message } => {
                terminal.done_waiting();
                let message = if message == STOPPED {
                    "Stopped."
                } else {
                    &message
                };
                println!("{}", terminal.error(&format!("  {message}")));
            }
        }

        Ok(())
    }
}

/// The "you>" prompt: a line editor with the chat's history, kept across
/// chats in the data directory.
struct Prompt {
    editor: Option<DefaultEditor>,
    history: PathBuf,
}

impl Prompt {
    fn new() -> Self {
        let editor = editor()
            .map_err(|e| log::warn!("chat: no line editing: {e}"))
            .ok();

        let history = data_dir().join(HISTORY_FILE);
        let mut prompt = Self { editor, history };
        if let Some(editor) = &mut prompt.editor {
            // None yet, the first time.
            let _ = editor.load_history(&prompt.history);
        }

        prompt
    }

    /// The user's next line; `None` when they end the chat (Ctrl-D).
    async fn read(&mut self) -> io::Result<Option<String>> {
        let Some(mut editor) = self.editor.take() else {
            print!("you> ");
            io::stdout().flush()?;
            return read_line().await;
        };

        loop {
            let (returned, line) = blocking::unblock(move || {
                let line = editor.readline("you> ");
                (editor, line)
            })
            .await;
            editor = returned;

            match line {
                Ok(line) => {
                    if !line.trim().is_empty() {
                        let _ = editor.add_history_entry(line.as_str());
                        if let Err(e) = editor.append_history(&self.history) {
                            log::warn!("chat: cannot keep the history: {e}");
                        }
                    }
                    self.editor = Some(editor);
                    return Ok(Some(line));
                }
                // Ctrl-C drops the line, as in a shell.
                Err(ReadlineError::Interrupted) => continue,
                Err(ReadlineError::Eof) => {
                    self.editor = Some(editor);
                    return Ok(None);
                }
                Err(e) => return Err(io::Error::other(e)),
            }
        }
    }
}

fn editor() -> rustyline::Result<DefaultEditor> {
    let config = rustyline::Config::builder()
        .max_history_size(HISTORY_MAX)?
        .history_ignore_dups(true)?
        .build();

    DefaultEditor::with_config(config)
}

/// A line of stdin, without blocking the executor; `None` at its end.
async fn read_line() -> io::Result<Option<String>> {
    blocking::unblock(|| {
        let mut line = String::new();
        let read = io::stdin().lock().read_line(&mut line)?;

        Ok((read > 0).then_some(line))
    })
    .await
}

/// The chat in a terminal: tool calls as one line each, answers as rendered
/// Markdown, and the approvals.
struct Terminal {
    tty: bool,
    show_thinking: bool,
    verbose: bool,
    skin: MadSkin,
    /// When the model call or tool run in progress started.
    started: Cell<Option<Instant>>,
    /// A "…" waits on the current line for the model.
    waiting: Cell<bool>,
}

impl Terminal {
    fn new(show_thinking: bool, verbose: bool) -> Self {
        Self {
            tty: io::stdout().is_terminal(),
            show_thinking,
            verbose,
            skin: MadSkin::default(),
            started: Cell::new(None),
            waiting: Cell::new(false),
        }
    }

    fn style(&self, code: &str, text: &str) -> String {
        if self.tty {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    fn dim(&self, text: &str) -> String {
        self.style("2", text)
    }

    fn error(&self, text: &str) -> String {
        self.style("31", text)
    }

    fn note(&self, text: &str) {
        println!("{}", self.dim(text));
    }

    /// Seconds since the current step started.
    fn took(&self) -> String {
        self.started
            .take()
            .map(|started| format!("({:.1} s)", started.elapsed().as_secs_f32()))
            .unwrap_or_default()
    }

    /// Take the model's "…" off the line.
    fn done_waiting(&self) {
        if self.waiting.replace(false) {
            print!("\r\x1b[2K");
        }
    }

    fn model_call(&self) {
        self.started.set(Some(Instant::now()));
        if self.tty {
            print!("{}", self.dim("  …"));
            let _ = io::stdout().flush();
            self.waiting.set(true);
        }
    }

    fn usage(&self, input_tokens: u32, output_tokens: u32) {
        self.done_waiting();
        let took = self.took();
        if self.verbose {
            self.note(&format!(
                "  [model {took}: {input_tokens} in / {output_tokens} out tokens]"
            ));
        }
    }

    fn thinking(&self, thinking: &str) {
        if self.show_thinking {
            self.note(thinking);
        }
    }

    fn text(&self, text: &str) {
        self.done_waiting();
        if self.tty {
            self.skin.print_text(text);
        } else {
            println!("{text}");
        }
    }

    fn tool_call(&self, call: &ToolCall) {
        self.done_waiting();
        let arguments = match call.arguments.trim() {
            "" | "{}" => String::new(),
            arguments => format!(" {arguments}"),
        };
        print!(
            "{}",
            self.style("36", &format!("→ {}{arguments}", call.name))
        );
        let _ = io::stdout().flush();
        self.started.set(Some(Instant::now()));
    }

    fn tool_result(&self, result: &str, is_error: bool) {
        let took = self.took();
        if is_error {
            println!(
                "  {} {}",
                self.error(&format!("✗ {result}")),
                self.dim(&took)
            );
        } else {
            println!("  {}", self.dim(&format!("✓ {took}")));
        }

        if self.verbose && !is_error {
            let shown = clip(result, SHOWN_RESULT_MAX);
            let cut = if shown.len() < result.len() {
                " …"
            } else {
                ""
            };
            self.note(&format!("    {shown}{cut}"));
        }
    }

    /// Ask the user whether `call` may run; the assistant remembers an
    /// "always".
    async fn approve(&self, call: &ToolCall) -> Approval {
        println!();
        loop {
            print!(
                "{}",
                self.style(
                    "33",
                    &format!("  Run {}? [y]es / [n]o / [a]lways: ", call.name)
                )
            );
            let _ = io::stdout().flush();

            let answer = match read_line().await {
                Ok(Some(line)) => line.trim().to_lowercase(),
                _ => return Approval::No,
            };

            let answer = match answer.as_str() {
                "y" | "yes" => Approval::Yes,
                "a" | "always" => Approval::Always,
                "" | "n" | "no" => {
                    print!("{}", self.dim("  refused"));
                    return Approval::No;
                }
                _ => continue,
            };

            self.started.set(Some(Instant::now()));
            return answer;
        }
    }
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
