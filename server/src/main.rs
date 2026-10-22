//! matter-peek-server — the local matter-peek server: the web UI, and the CLI
//! and MCP server with `--server`, connect to it.
//!
//! ```text
//! matter-peek-server [BIND_ADDR]   # default 127.0.0.1:8087
//! ```
//!
//! The assistant's model is set on the web UI's Assistant page, or with
//! `matter-peek assistant set`, and kept in the store. `MATTER_PEEK_LLM_URL`
//! (an OpenAI-compatible API, e.g. `http://localhost:11434/v1`),
//! `MATTER_PEEK_LLM_MODEL`, and `MATTER_PEEK_LLM_KEY` if the API needs a key,
//! override it.

use std::process::ExitCode;

use log::{error, info};

const DEFAULT_ADDR: &str = "127.0.0.1:8087";

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| DEFAULT_ADDR.into());
    info!("matter-peek-server: starting on {addr}");

    let env = matter_peek_server::llm::model_from_env();

    match matter_peek_server::run(&addr, env) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("matter-peek-server: {e}");
            ExitCode::FAILURE
        }
    }
}
