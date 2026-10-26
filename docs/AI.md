# AI: the MCP server and the assistant

matter-peek offers its API to AI in two ways: as MCP tools, for an agent you
already use (Claude Code, Claude Desktop, Cursor, …), and as a built-in
assistant on a model of your choice.

## The MCP server

`matter-peek mcp` offers the API as MCP tools over stdio (newline-delimited
JSON-RPC 2.0, protocol version `2025-06-18`, or `2024-11-05` for older
clients); logs go to stderr. Each tool carries the MCP annotations
`readOnlyHint` / `destructiveHint`; devices are named by node id, as a number
or as its hex form (`"0x1B67B"`); a failed tool says why in plain words. Without
`--server` it holds the store for as long as it runs (syncing the device list
periodically, like the server), so to share the store with the web UI, run
`matter-peek serve` and pass `--server`.

| Tools | Access |
|-------|--------|
| `discover`, `advertised_devices`, `list_devices`, `list_fabrics`, `peek_device`, `read_model`, `read_events`, `read_network`, `thread_mesh`, `thread_findings`, `retrieve_logs`, `read_qr_image`, `ota_images`, `ota_updates`, `commissioning_window`, `device_time` | read-only |
| `identify` | acts on the device (it blinks); not read-only, so agents ask first |
| `commission`, `decommission`, `remember`, `set_tags`, `sync`, `add_network`, `remove_network`, `connect_network`, `reorder_network`, `ota_upload`, `ota_delete`, `ota_update`, `open_commissioning_window`, `close_commissioning_window`, `set_device_time`, `remove_device_fabric`, `create_fabric`, `rename_fabric`, `set_default_fabric`, `delete_fabric`, `write_attribute`, `invoke_command` | **mutating** |

The mutating tools touch real devices, so only `--allow-mutate` offers them:
without it they are hidden from `tools/list` and rejected at `tools/call`. The
operator decides; the agent cannot enable them.

Claude Code:

```sh
claude mcp add matter-peek -- matter-peek --server ws://127.0.0.1:8087 mcp
```

Claude Desktop (`claude_desktop_config.json`):

```json
{
  "mcpServers": {
    "matter-peek": {
      "command": "/absolute/path/to/matter-peek",
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

## The assistant

The assistant answers questions about the devices: its model looks into them
with the same tools as MCP and answers in Markdown. Any model behind an
OpenAI-compatible API will do: a local one (Ollama, LM Studio, vLLM) or a
provider's (OpenAI, OpenRouter, …). It runs where the store is: in
`matter-peek serve` (and so in the app), or in the CLI when no server runs.

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

### Chats

The assistant's chats are kept as JSON files in the data directory's `chats/`,
listed by their first question, whichever front-end asked: the web UI's
Assistant page lists, reopens, continues and deletes them. A chat goes on while
its page is closed. "Allow changes" (`--allow-mutate` in the terminal) offers
the model the tools that change things; each tool not marked read-only is asked
for first: `identify` always. The web UI shows an approval card (Allow, Always
allow for the chat, Deny), each tool call opening onto its arguments and
result, and a stop button.

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
