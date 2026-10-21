//! matter-peek-server — the local matter-peek server: the web UI, and the CLI
//! and MCP server with `--server`, connect to it.
//!
//! ```text
//! matter-peek-server [BIND_ADDR]   # default 127.0.0.1:8087
//! ```

use std::process::ExitCode;

use log::{error, info};

const DEFAULT_ADDR: &str = "127.0.0.1:8087";

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| DEFAULT_ADDR.into());
    info!("matter-peek-server: starting on {addr}");

    match matter_peek_rpc::run(&addr) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("matter-peek-server: {e}");
            ExitCode::FAILURE
        }
    }
}
