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
server/  matter-peek-server — the API over WebSocket/JSON, its clients'
         connection, and the web UI's assistant chats (binary: matter-peek-server)
cli/     the command line, the MCP server for AI agents and the terminal chat
         (binary: matter-peek)
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

# The assistant: give it a model (any OpenAI-compatible API), then chat
cargo run --bin matter-peek -- assistant set --url http://localhost:11434/v1 --model qwen3.5:9b
cargo run --bin matter-peek -- chat
```

### MCP (AI agents)

`matter-peek mcp` offers the API as MCP tools over stdio (newline-delimited
JSON-RPC 2.0, protocol version `2025-06-18`, or `2024-11-05` for older
clients); logs go to stderr. Each tool carries the MCP annotations
`readOnlyHint` / `destructiveHint`; devices are named by node id, as a number
or as its hex form (`"0x1B67B"`); a failed tool says why in plain words. Without
`--server` it holds the store for as long as it runs (syncing the device list
periodically, like the server), so to share the store with the web UI, run
`matter-peek-server` and pass `--server`.

| Tools | Access |
|-------|--------|
| `discover`, `list_devices`, `peek_device`, `read_network`, `retrieve_logs`, `read_qr_image`, `ota_images` | read-only |
| `identify` | acts on the device (it blinks); not read-only, so agents ask first |
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

To see what a strong model makes of the tools alone, `scripts/claude-eval.sh`
runs Claude Code with only these tools: no auto-memory, CLAUDE.md, user
settings or Claude Code tools of its own.

### The assistant (an AI agent)

The assistant answers questions about the devices: its model looks into them
with the same tools as MCP and answers in Markdown. Any model behind an
OpenAI-compatible API will do: a local one (Ollama, LM Studio, vLLM) or a
provider's (OpenAI, OpenRouter, …). It runs where the store is: in
`matter-peek-server`, or in the CLI when no server runs (`core`'s
`api::agent`, so the mobile apps can have it too).

Its model is kept in the store, with the API's key, which never leaves the side
that runs the assistant: the API shows only its last four characters. A key
stays only while the API stays at the same address. Set the model on the web
UI's Assistant page (the model's name in a chat's header), or:

```sh
matter-peek assistant                                   # show it
matter-peek assistant models --url http://localhost:11434/v1   # what an API offers
matter-peek assistant set --url http://localhost:11434/v1 --model qwen3.5:9b
matter-peek assistant set --api-key                     # asks for the key, hidden
echo "$KEY" | matter-peek assistant set --api-key-stdin
matter-peek assistant set --think off                   # faster, less careful
matter-peek assistant test                              # the model says hello
matter-peek assistant clear                             # forget it, and its key
```

`MATTER_PEEK_LLM_URL` and `MATTER_PEEK_LLM_MODEL` (and `MATTER_PEEK_LLM_KEY`)
override the stored model wherever the assistant runs, e.g. for a server in a
container; it cannot be changed through the API then. The model is not an MCP
tool: an agent does not get to change its own model, or see keys.

**Chats.** The assistant's chats are kept as JSON files in the data
directory's `chats/`, listed by their first question, whichever front-end
asked: the web UI's Assistant page lists, reopens, continues and deletes them.
A chat goes on while its page is closed. "Allow changes" (`--allow-mutate` in
the terminal) offers the model the tools that change things; each tool not
marked read-only is asked for first: `identify` always. The web UI shows an
approval card (Allow, Always allow for the chat, Deny), each tool call opening
onto its arguments and result, and a stop button.

```sh
matter-peek chat                                       # a chat; /new, /tools, /quit
matter-peek chat "Which of my devices are offline?"    # one question
matter-peek chat --allow-mutate                        # may change things
```

In the terminal, approvals are `[y]es / [n]o / [a]lways`. The prompt edits
like a shell's (arrows, Ctrl-A/E/W, …), with its history (↑/↓, Ctrl-R) kept
across chats in the data directory; Ctrl-C drops the line, Ctrl-D ends the
chat. `--show-thinking` shows the model's thinking, `--verbose` the tools'
results and the model's tokens. Without `--server`, it holds the store and
syncs the device list before the first question.

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
