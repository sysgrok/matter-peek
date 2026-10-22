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
health, logs, OTA updates, the fabrics they are on), commission new ones (with
mDNS discovery) and share them with other ecosystems (Google Home, Home
Assistant, ...) — from
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
| `discover`, `list_devices`, `list_fabrics`, `peek_device`, `read_network`, `thread_mesh`, `thread_findings`, `retrieve_logs`, `read_qr_image`, `ota_images`, `ota_updates`, `commissioning_window` | read-only |
| `identify` | acts on the device (it blinks); not read-only, so agents ask first |
| `commission`, `decommission`, `remember`, `set_tags`, `sync`, `add_network`, `remove_network`, `connect_network`, `reorder_network`, `ota_upload`, `ota_delete`, `ota_update`, `open_commissioning_window`, `close_commissioning_window`, `remove_device_fabric`, `create_fabric`, `rename_fabric`, `set_default_fabric`, `delete_fabric` | **mutating** |

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

### Fabrics: several homes

matter-peek keeps devices on fabrics of its own, each with a root CA of its
own and a name: homes, customers, or test and production apart. One is the
default, and can't be deleted. The web UI shows one fabric at a time (picked
at the top, remembered by the browser): its devices, the Thread mesh of its
devices, and the fabric a commission puts a device into. The CLI works in
`--fabric <name|#index|all>` (or `MATTER_PEEK_FABRIC`), else the default, and
manages them with `matter-peek fabric list|create|rename|default|delete`
(`delete --decommission` or `--forget` for a fabric with devices); MCP has
`list_fabrics` and `create/rename/set_default/delete_fabric`, and a `fabric`
argument on the tools that list or commission (`mcp --fabric` sets the
default). Devices keep their node IDs unique across fabrics, so every tool
about one device names it by node ID alone.

### Sharing a device with another ecosystem

A device on our fabric can join another controller's too (Google Home, Home
Assistant, Apple Home, ...): the card's Share button, `matter-peek share
<node> [--minutes 3..15]` (`--status`, `--close`) and the MCP tools
`open_commissioning_window` / `close_commissioning_window` /
`commissioning_window` open an enhanced commissioning window on it, with a
passcode made for that window, and show the code to commission it with: the
11-digit manual pairing code and the QR code (drawn in the web UI and the
terminal). The window closes after 15 minutes (3 to 15), or when closed; the
code works only while it is open. A device with no free fabric is refused.

The fabrics a device is on (the controllers it belongs to: ours, Google,
Apple, Home Assistant, ... by their vendor, or the label their controller set)
come with its health: live on the device card (a click shows each one's
vendor, fabric ID and the device's node ID there), in `matter-peek peek
<node>` and in the MCP tool `peek_device`. Another controller's fabric can be
removed from a device (the popup, `matter-peek remove-fabric <node> <index>`,
MCP `remove_device_fabric`); ours is removed by decommissioning.

### Thread mesh

The Thread page of the web UI, `matter-peek thread` (`thread findings` for the
findings alone) and the MCP tools `thread_mesh` / `thread_findings` show the
Thread mesh as our Thread devices see it, from Matter data only (no Thread
credentials): each device's Thread Network Diagnostics (its neighbour and
route tables), merged into one graph, labelled with the border routers' MeshCoP
adverts (`_meshcop._udp`). The nodes are counted by role, and the links show
the link quality (0–3) and signal each way. How a node sleeps comes from its
role, our devices' ICD Management cluster and the `_matter._tcp` adverts
(`SII`/`SAI`/`ICD`); whether a sleepy node uses CSL is not visible. Other
fabrics' Matter nodes whose address is in a border router's prefix are listed
as seen on the LAN, with their fabrics and how they sleep: no table we read
places them in the mesh. The findings, in plain words: several
networks on the LAN, a split network, weak or one-way links, routers some of
our devices depend on, a single border router, devices that keep changing
their parent or losing the network, a Thread channel within a Wi-Fi channel
our Wi-Fi devices use, OpenThread's default credentials, and no route from
this machine to a network's prefix (Linux). It reads the devices one after
another, so it takes a few seconds; only the links our devices report are
known.

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
