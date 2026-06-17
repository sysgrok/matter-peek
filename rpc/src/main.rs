//! matter-peek-rpc binary — the local RPC server the web UI connects to.
//!
//! ```text
//! matter-peek-rpc [BIND_ADDR]   # default 127.0.0.1:8087
//! ```

use std::process::ExitCode;

use log::{error, info};

const DEFAULT_ADDR: &str = "127.0.0.1:8087";

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| DEFAULT_ADDR.into());
    info!("matter-peek-rpc: starting on {addr}");

    match matter_peek_rpc::run(&addr) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("matter-peek-rpc: server error: {e}");
            ExitCode::FAILURE
        }
    }
}
