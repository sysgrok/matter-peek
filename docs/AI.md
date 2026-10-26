# AI: the MCP server and the assistant

matter-peek makes its API available to AI in two ways. It offers MCP tools,
which an agent you already use can call (Claude Code, Claude Desktop, Cursor
and others). It also has a built-in assistant, which works with a model of your
choice.

## The MCP server

`matter-peek mcp` offers the API as MCP tools over stdio. It uses
newline-delimited JSON-RPC 2.0, with protocol version `2025-06-18`, or
`2024-11-05` for older clients. Its logs go to stderr.

- Each tool carries the MCP annotations `readOnlyHint` and `destructiveHint`.
- Devices are identified by their node ID, either as a number or in its hex
  form (`"0x1B67B"`).
- When a tool fails, it says why in plain words.

Without `--server`, `matter-peek mcp` holds the store for as long as it runs,
and syncs the device list periodically, as the server does. To share the store
with the web UI, run `matter-peek serve` and pass `--server` to
`matter-peek mcp`.

| Tools | Access |
|-------|--------|
| `discover`, `advertised_devices`, `list_devices`, `list_fabrics`, `peek_device`, `read_model`, `read_events`, `read_network`, `thread_mesh`, `thread_findings`, `retrieve_logs`, `read_qr_image`, `ota_images`, `ota_updates`, `commissioning_window`, `device_time` | read-only |
| `identify`, `wake_device`, `stay_awake` | act on the device without changing anything stored: `identify` makes it blink, for example, and the other two keep a sleeping device awake so that it answers; they are not read-only, so agents ask before using them |
| `commission`, `decommission`, `register_check_ins`, `unregister_check_ins`, `remember`, `forget`, `set_tags`, `sync`, `add_network`, `remove_network`, `connect_network`, `reorder_network`, `ota_upload`, `ota_delete`, `ota_update`, `open_commissioning_window`, `close_commissioning_window`, `set_device_time`, `remove_device_fabric`, `create_fabric`, `rename_fabric`, `set_default_fabric`, `delete_fabric`, `write_attribute`, `invoke_command` | **mutating** |

The mutating tools change real devices, so they are only offered with
`--allow-mutate`. Without it, they are hidden from `tools/list` and rejected
at `tools/call`. This is decided by the operator; the agent cannot enable them
itself.

To add matter-peek to Claude Code:

```sh
claude mcp add matter-peek -- matter-peek --server ws://127.0.0.1:8087 mcp
```

To add it to Claude Desktop, edit `claude_desktop_config.json`:

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

Add `"--allow-mutate"` to `args` only if the agent is allowed to change
devices. To use a running server, add `"--server", "ws://127.0.0.1:8087"`
before `"mcp"`.

`scripts/claude-eval.sh` shows how a strong model works with these tools alone.
It runs Claude Code with only the matter-peek tools, without its auto-memory,
CLAUDE.md, user settings or its own built-in tools.

## The assistant

The assistant answers questions about the devices. Its model looks into the
devices with the same tools that the MCP server offers, and answers in
Markdown. Any model with an OpenAI-compatible API can be used: a local one
(Ollama, LM Studio, vLLM) or one from a provider (OpenAI, OpenRouter and
others).

The assistant runs where the store is: in `matter-peek serve`, and therefore
also in the app, or in the CLI when no server is running.

The model is kept in the store, together with the API key. The key never leaves
the side that runs the assistant: the API only shows its last four characters.
The key is kept only as long as the API's address stays the same.

The model can be set on the **Assistant** page of the web UI (click the
model's name in the header of a chat), or in the CLI:

```sh
matter-peek assistant                                   # show the current model
matter-peek assistant models --url http://localhost:11434/v1   # list the models an API offers
matter-peek assistant set --url http://localhost:11434/v1 --model qwen3.5:9b
matter-peek assistant set --api-key                     # asks for the key, without showing it
echo "$KEY" | matter-peek assistant set --api-key-stdin
matter-peek assistant set --think off                   # faster, but less careful
matter-peek assistant test                              # the model answers with a greeting
matter-peek assistant clear                             # forget the model and its key
```

`MATTER_PEEK_LLM_URL`, `MATTER_PEEK_LLM_MODEL` and `MATTER_PEEK_LLM_KEY`
override the stored model wherever the assistant runs, for example for a
server in a container. While they are set, the model cannot be changed through
the API. The model is deliberately not available as an MCP tool, so that an
agent cannot change its own model or see the keys.

### Chats

The assistant's chats are kept as JSON files in the `chats/` folder of the data
directory. They are listed by their first question, no matter which front-end
asked it. The **Assistant** page of the web UI lists the chats, and can reopen,
continue and delete them. A chat keeps running when its page is closed.

"Allow changes" in the web UI, or `--allow-mutate` in the terminal, offers the
model the tools that change things. Each tool that is not marked read-only is
asked for before it runs. This always includes `identify`, `wake_device` and
`stay_awake`. The web UI shows an
approval card with Allow, Always allow (for this chat) and Deny. Each tool call
can be opened to show its arguments and its result, and a stop button ends the
answer.

```sh
matter-peek chat                                       # start a chat; /new, /tools, /quit
matter-peek chat "Which of my devices are offline?"    # ask one question
matter-peek chat --allow-mutate                        # the model may change things
```

In the terminal, approvals are answered with `[y]es / [n]o / [a]lways`. The
prompt can be edited like in a shell (arrows, Ctrl-A/E/W and so on). Its
history (↑/↓, Ctrl-R) is kept across chats in the data directory. Ctrl-C drops
the current line, and Ctrl-D ends the chat. `--show-thinking` shows the
model's thinking, and `--verbose` shows the tools' results and the model's
token use. Without `--server`, the chat holds the store, and syncs the device
list before the first question.
