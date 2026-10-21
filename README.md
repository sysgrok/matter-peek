# matter-peek

A vendor-agnostic **Matter mini commissioner / controller** for installers and
technicians: commission into one or more Matter devices, inspect their health
via the standard diagnostic clusters, and pull **Diagnostic Logs** (crash dumps
/ logs) over BDX.

Built as a native Rust core over [`rs-matter`](https://github.com/project-chip/rs-matter),
with a web UI, a CLI and an MCP server on one API, and mobile bindings via UniFFI.

See [`DESIGN.md`](DESIGN.md) for the full architecture and roadmap.

## Status

A local Matter controller for diagnostics: list the devices on our fabric (live
health, logs, OTA updates) and commission new ones (with mDNS discovery) — from
a web UI, a command line, or an AI agent over MCP, all through one API.

## Layout

```
core/    matter-peek-core — the API (`MatterPeek`, its types and wire messages)
         and the controller behind it (wraps rs-matter); also the UniFFI bindings
server/  matter-peek-server — the API over WebSocket/JSON, and its clients'
         connection (binary: matter-peek-server)
cli/     the command line (binary: matter-peek)
mcp/     the MCP server for AI agents (binary: matter-peek-mcp)
web/     matter-peek-web — Yew/Wasm web UI (WebSocket client; separate wasm crate)
```

Everything goes through the API. The web UI talks to a running
`matter-peek-server`; the CLI and the MCP server do too with `--server`, else
they run a controller in their own process. Only one process at a time can hold
the store.

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

# The MCP server (stdio), for an AI agent: through the server, or on its own;
# mutating tools need --allow-mutate
cargo run --bin matter-peek-mcp -- --server ws://127.0.0.1:8087 --allow-mutate
```

### Mobile bindings

`core` exports `MatterPeek` over [UniFFI](https://mozilla.github.io/uniffi-rs/)
with its `uniffi` feature. Build it as a library, then generate the bindings:

```sh
cargo rustc -p matter-peek-core --lib --features uniffi --crate-type cdylib
cargo run -p matter-peek-core --features uniffi-cli --bin uniffi-bindgen -- \
    generate --library target/debug/libmatter_peek_core.so --language kotlin --out-dir out
# --language swift for iOS / macOS
```

For local co-development against working clones, uncomment the `[patch.crates-io]`
line in the workspace `Cargo.toml` to point `rs-matter` at `../rs-matter/rs-matter`.

## License

Proprietary — all rights reserved. Not open source (at least not yet).
