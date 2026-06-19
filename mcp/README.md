# matter-peek-mcp

An **MCP (Model Context Protocol)** server that exposes the matter-peek Matter
diagnostic controller to AI agents (Claude Desktop, agent frameworks, …) as a
set of tools — discover devices, read health, identify a unit, and (opt-in)
commission.

It is a sibling transport to the WebSocket server (`matter-peek-rpc`): both drive
the **same** controller actor and the same [`matter-peek-proto`] contract; this
one just speaks MCP over **stdio** instead of WebSocket/JSON.

## What it does

| Tool | Access | Description |
|------|--------|-------------|
| `discover` | read-only | Browse the LAN for commissionable Matter devices via mDNS. |
| `list_devices` | read-only | List persistently-commissioned devices (identity, fingerprint, tags). |
| `peek_device` | read-only | Re-address a stored device over a fresh CASE session and read its health (uptime / reboot count / heap), optionally its crash-log size. |
| `identify` | read-only | Blink/beep a device via the Identify cluster, to locate a unit. |
| `commission` | **mutating** | Commission a device by address (persistent = keep on our fabric + store; ephemeral = throwaway fabric, diagnose, then remove). |
| `set_tags` | **mutating** | Replace a stored device's local tags (controller-local labels; never touches the device). |

## Safety: read-only by default

Mutating tools (`commission`, `set_tags`) touch real devices on a real fabric, so
they are **off by default**. They are only registered — and only accepted — when
the server is started with `--allow-mutate`:

```
matter-peek-mcp                 # read-only tools only
matter-peek-mcp --allow-mutate  # also exposes commission + set_tags
```

This is defense in depth: without the flag, the mutating tools are both hidden
from `tools/list` **and** hard-rejected at `tools/call` time (JSON-RPC error
`-32602`). The human operator decides whether the agent may commission — the
agent cannot self-enable it.

## Running it

Build the binary (desktop / `std`):

```sh
cargo build -p matter-peek-mcp --release
# → target/release/matter-peek-mcp
```

It speaks newline-delimited **JSON-RPC 2.0** on stdin/stdout (the MCP stdio
transport). Logs go to **stderr**, so stdout stays a clean protocol channel. It
exits cleanly when stdin closes.

### Claude Desktop

Add it to your MCP config (`claude_desktop_config.json`):

```json
{
  "mcpServers": {
    "matter-peek": {
      "command": "/absolute/path/to/target/release/matter-peek-mcp",
      "args": []
    }
  }
}
```

To let the agent commission devices, add the flag (only when you want that):

```json
      "args": ["--allow-mutate"]
```

### Any MCP client

Point the client at the `matter-peek-mcp` executable as a stdio MCP server. The
server advertises protocol version `2024-11-05`.

## Quick manual check

Pipe a couple of JSON-RPC lines in and read the responses out:

```sh
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' \
  '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_devices","arguments":{}}}' \
  | matter-peek-mcp 2>/dev/null
```

`tools/call` results come back as a single text content block containing the
structured JSON payload (and `isError: true` for failures) — e.g.
`{"content":[{"type":"text","text":"{ \"devices\": [] ... }"}],"isError":false}`.

## State & networking

The server stands up a full Matter controller on startup: it restores any
persisted fabric + device store (a directory under the system temp dir,
`<tmp>/rs-matter`) and binds a controller UDP socket. So the same persistent
device list, tags, and CASE re-addressing the web UI uses are available here too
— `list_devices` / `peek_device` operate on devices commissioned by *any*
matter-peek front end sharing that store.

## Design notes

- **`no_std` + `alloc` protocol layer.** The library (JSON-RPC envelope, tool
  catalog, tool ⇄ proto mapping, the `--allow-mutate` gate) carries no `std` and
  no controller dependency — it cross-compiles for bare-metal targets
  (`riscv32imac-unknown-none-elf`). It runs a tool through an injected `Dispatch`
  trait. The desktop **binary** (behind the `std` feature) supplies a `Dispatch`
  over the `matter-peek-rpc` actor and the stdio loop. A future on-device (esp)
  shell would implement `Dispatch` over its own in-process controller, reusing
  the protocol layer unchanged.
- **No tokio MCP SDK.** The Rust MCP SDK is `Send`-bound; the Matter stack is
  `!Send` and runs on a single-threaded executor. MCP-over-stdio is just JSON-RPC
  2.0 over newline-delimited stdin/stdout, so the framing is hand-rolled to fit
  the existing smol/async-io runtime.
- **Transport:** stdio only for now. The `Dispatch` seam makes an HTTP/SSE
  transport additive (deferred).

[`matter-peek-proto`]: ../proto
