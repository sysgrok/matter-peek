//! `matter-peek chat`: ask an AI model about the devices, in a chat. The model
//! looks into them with the API's tools, and changes them only with
//! `--allow-mutate` and once the user approves each change (see
//! [`matter_peek_core::api::agent`]). Any model behind an OpenAI-compatible API
//! will do: a local one (Ollama, LM Studio) or a provider's.

use core::cell::{Cell, RefCell};

use std::collections::BTreeSet;
use std::io::{self, BufRead as _, IsTerminal as _, Write as _};
use std::time::{Duration, Instant};

use termimad::MadSkin;

use matter_peek_core::api::agent::openai::OpenAi;
use matter_peek_core::api::agent::{Agent, AgentListener, Http, Provider, Think, ToolCall, Usage};
use matter_peek_core::api::mcp::{Caps, Dispatch};
use matter_peek_core::api::wire;

use crate::mcp;

/// How long a model may take to answer.
const MODEL_TIMEOUT: Duration = Duration::from_secs(600);

/// How much of a tool's result `--verbose` shows, in bytes.
const SHOWN_RESULT_MAX: usize = 800;

/// The chat's model and manners.
pub struct ChatArgs {
    /// Ask this, answer, and end.
    pub prompt: Option<String>,
    pub llm_url: String,
    pub model: String,
    pub api_key: Option<String>,
    pub think: Think,
    pub show_thinking: bool,
    pub verbose: bool,
    pub allow_mutate: bool,
}

/// Chat with the model through the server at `server`, else through a
/// controller started in this process.
pub async fn chat(server: Option<String>, args: ChatArgs) -> io::Result<()> {
    let provider = OpenAi::new(Ureq::new(), &args.llm_url, &args.model, args.api_key);
    let caps = Caps {
        allow_mutate: args.allow_mutate,
    };
    let embedded = server.is_none();
    let dispatch = mcp::dispatch(server)?;

    let terminal = Terminal::new(args.show_thinking, args.verbose);
    if embedded {
        // A controller started now lists only the remembered devices until a
        // sync finds the others.
        terminal.note("Looking for the devices on our fabric…");
        if let Err(e) = dispatch.dispatch(wire::Request::Sync).await {
            terminal.note(&format!("The sync failed: {e}"));
        }
    }

    let mut agent = Agent::new(provider, dispatch, caps, args.think);

    let changing = agent.tools().iter().filter(|tool| !tool.read_only).count();
    terminal.note(&format!(
        "{} at {}: {} tools{}",
        args.model,
        args.llm_url,
        agent.tools().len(),
        if changing > 0 {
            format!(", {changing} of which change things and are asked for first")
        } else {
            String::new()
        }
    ));

    if let Some(prompt) = args.prompt {
        ask(&mut agent, &prompt, &terminal).await;
        return Ok(());
    }

    terminal.note("Ask away. /reset forgets the conversation, /tools lists the tools, /quit ends.");

    loop {
        print!("\n{} ", terminal.bold("you>"));
        io::stdout().flush()?;

        let Some(line) = read_line().await? else {
            println!();
            return Ok(());
        };

        match line.trim() {
            "" => {}
            "/quit" | "/exit" => return Ok(()),
            "/reset" => {
                agent.reset();
                terminal.note("Forgotten.");
            }
            "/tools" => {
                for tool in agent.tools() {
                    let changes = if tool.read_only {
                        ""
                    } else {
                        "  (changes things)"
                    };
                    println!("  {}{changes}", tool.name);
                }
            }
            question => ask(&mut agent, question, &terminal).await,
        }
    }
}

async fn ask<P: Provider, D: Dispatch>(
    agent: &mut Agent<P, D>,
    question: &str,
    terminal: &Terminal,
) {
    if let Err(e) = agent.ask(question, terminal).await {
        terminal.done_waiting();
        println!("{}", terminal.error(&format!("  {e}")));
    }
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

/// The model's API over HTTP(S): blocking calls, on the thread pool.
struct Ureq(ureq::Agent);

impl Ureq {
    fn new() -> Self {
        let config = ureq::Agent::config_builder()
            // The body of an error status says why.
            .http_status_as_error(false)
            .timeout_global(Some(MODEL_TIMEOUT))
            .build();

        Self(config.into())
    }
}

impl Http for Ureq {
    async fn post(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        body: String,
    ) -> Result<String, String> {
        let agent = self.0.clone();
        let url = url.to_string();
        let headers: Vec<(String, String)> = headers
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();

        blocking::unblock(move || {
            let mut request = agent.post(&url);
            for (name, value) in &headers {
                request = request.header(name, value);
            }

            let mut response = request
                .send(body)
                .map_err(|e| format!("cannot reach the model at {url}: {e}"))?;
            let status = response.status();
            let text = response
                .body_mut()
                .read_to_string()
                .map_err(|e| format!("cannot read the model's answer: {e}"))?;

            if status.is_success() {
                Ok(text)
            } else {
                Err(format!(
                    "the model's API answered {status}: {}",
                    clip(&text, 300)
                ))
            }
        })
        .await
    }
}

/// The chat in a terminal: tool calls as one line each, answers as rendered
/// Markdown, and the approvals.
struct Terminal {
    tty: bool,
    show_thinking: bool,
    verbose: bool,
    skin: MadSkin,
    /// The tools approved for the rest of the chat.
    always: RefCell<BTreeSet<String>>,
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
            always: RefCell::new(BTreeSet::new()),
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

    fn bold(&self, text: &str) -> String {
        self.style("1", text)
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
}

impl AgentListener for Terminal {
    fn on_model_call(&self) {
        self.started.set(Some(Instant::now()));
        if self.tty {
            print!("{}", self.dim("  …"));
            let _ = io::stdout().flush();
            self.waiting.set(true);
        }
    }

    fn on_usage(&self, usage: Usage) {
        self.done_waiting();
        let took = self.took();
        if self.verbose {
            self.note(&format!(
                "  [model {took}: {} in / {} out tokens]",
                usage.input_tokens, usage.output_tokens
            ));
        }
    }

    fn on_thinking(&self, thinking: &str) {
        if self.show_thinking {
            self.note(thinking);
        }
    }

    fn on_text(&self, text: &str) {
        self.done_waiting();
        if self.tty {
            self.skin.print_text(text);
        } else {
            println!("{text}");
        }
    }

    fn on_tool_call(&self, call: &ToolCall) {
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

    fn on_tool_result(&self, _call: &ToolCall, result: &str, is_error: bool) {
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

    async fn approve(&self, call: &ToolCall) -> bool {
        if self.always.borrow().contains(&call.name) {
            return true;
        }

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
                _ => return false,
            };

            match answer.as_str() {
                "y" | "yes" => break,
                "a" | "always" => {
                    self.always.borrow_mut().insert(call.name.clone());
                    break;
                }
                "" | "n" | "no" => {
                    print!("{}", self.dim("  refused"));
                    return false;
                }
                _ => {}
            }
        }

        self.started.set(Some(Instant::now()));
        true
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
