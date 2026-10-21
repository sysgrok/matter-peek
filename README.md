# matter-peek

A vendor-agnostic **Matter mini commissioner / controller** for installers and
technicians: commission into one or more Matter devices, inspect their health
via the standard diagnostic clusters, and pull **Diagnostic Logs** (crash dumps
/ logs) over BDX.

Built as a native Rust core over [`rs-matter`](https://github.com/project-chip/rs-matter),
with a web UI for desktop validation and (later) native mobile UIs via UniFFI.

See [`DESIGN.md`](DESIGN.md) for the full architecture and roadmap.

## Status

A local Matter controller for diagnostics: list the devices on our fabric (live
health, logs, OTA updates) and commission new ones (with mDNS discovery) — from
a web UI, a command line, or an AI agent over MCP, all through one API.

## Layout

```
core/   matter-peek-core — the reusable controller library (wraps rs-matter)
proto/  matter-peek-proto — the API: requests, responses, events (serde)
rpc/    matter-peek-rpc — the API over the core: the actor, the WS server
        (binary: matter-peek-server) and its client connections
cli/    the command line, over the API (binary: matter-peek)
mcp/    the MCP server for AI agents, over the API (binary: matter-peek-mcp)
web/    matter-peek-web — Yew/Wasm web UI (WS/JSON client; separate wasm crate)
uniffi/ mobile bindings over the core           (Phase 3)
```

Everything goes through the API. The web UI and the MCP server talk to a
running `matter-peek-server`; the CLI does too with `--server`, else it runs a
controller in its own process. Only one process at a time can hold the store.

## Run

```sh
# An accessory to try it on (here, rs-matter's system_tests with a crash log)
( cd ../rs-matter && cargo run -p rs-matter-tests --bin system_tests -- --crash_log /tmp/crash.log )

# The server (default ws://127.0.0.1:8087)
cargo run --bin matter-peek-server

# The web UI (needs `trunk` + the wasm32 target on nightly)
( cd web && trunk serve )   # then open the printed http://… URL

# The CLI: through the server, or on its own while no server runs
cargo run --bin matter-peek -- --server ws://127.0.0.1:8087 discover
cargo run --bin matter-peek -- --help

# The MCP server (stdio), for an AI agent; mutating tools need --allow-mutate
cargo run --bin matter-peek-mcp -- --allow-mutate
```

For local co-development against working clones, uncomment the `[patch.crates-io]`
line in the workspace `Cargo.toml` to point `rs-matter` at `../rs-matter/rs-matter`.

## License

Proprietary — all rights reserved. Not open source (at least not yet).
