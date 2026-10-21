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
core/    matter-peek-core — the API (`MatterPeek`, its types, wire messages and
         MCP tools) and the controller behind it (wraps rs-matter); also the
         UniFFI bindings
server/  matter-peek-server — the API over WebSocket/JSON, and its clients'
         connection (binary: matter-peek-server)
cli/     the command line, and the MCP server for AI agents (binary: matter-peek)
web/     matter-peek-web — Yew/Wasm web UI (WebSocket client; separate wasm crate)
```

Everything goes through the API. The web UI talks to a running
`matter-peek-server`; the CLI (also as the MCP server) does too with `--server`,
else it runs a controller in its own process. Only one process at a time can
hold the store.

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
cargo run --bin matter-peek -- --server ws://127.0.0.1:8087 mcp --allow-mutate
```

### MCP (AI agents)

`matter-peek mcp` offers the API as MCP tools over stdio (newline-delimited
JSON-RPC 2.0, protocol version `2024-11-05`); logs go to stderr. Without
`--server` it holds the store for as long as it runs (syncing the device list
periodically, like the server), so to share the store with the web UI, run
`matter-peek-server` and pass `--server`.

| Tools | Access |
|-------|--------|
| `discover`, `list_devices`, `peek_device`, `read_network`, `retrieve_logs`, `identify`, `read_qr_image`, `ota_images` | read-only |
| `commission`, `decommission`, `remember`, `set_tags`, `sync`, `add_network`, `remove_network`, `connect_network`, `reorder_network`, `ota_upload`, `ota_delete`, `ota_update` | **mutating** |

The mutating tools touch real devices, so only `--allow-mutate` offers them:
without it they are hidden from `tools/list` and rejected at `tools/call`. The
operator decides; the agent cannot enable them.

Claude Desktop (`claude_desktop_config.json`):

```json
{
  "mcpServers": {
    "matter-peek": {
      "command": "/absolute/path/to/target/release/matter-peek",
      "args": ["mcp"]
    }
  }
}
```

Add `"--allow-mutate"` to `args` only when the agent may change devices, and
`"--server", "ws://127.0.0.1:8087"` (before `mcp`) to go through a running server.

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
