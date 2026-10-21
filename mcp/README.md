# matter-peek-mcp

An **MCP (Model Context Protocol)** server that gives AI agents (Claude Desktop,
agent frameworks, …) the matter-peek API as tools: discover and commission
devices, list them, read their health and logs, identify, update and
decommission them.

Like the CLI, it runs a controller in its own process, or with `--server` uses
a running `matter-peek-server`: each tool call is one request of the API
(`matter_peek_core::api::wire`). It speaks MCP over **stdio**.

## Tools

| Tool | Access | Description |
|------|--------|-------------|
| `discover` | read-only | The commissionable devices: on the network (mDNS) and over Bluetooth LE. |
| `list_devices` | read-only | The devices on our fabric: identity, fingerprint, tags, online, remembered. |
| `peek_device` | read-only | Read a listed device's health (uptime, reboots, heap). |
| `retrieve_logs` | read-only | A diagnostic log of a device (end-user support, network, crash). |
| `identify` | read-only | Make a device blink / beep, to find the unit. |
| `read_qr_image` | read-only | The setup code in a picture of a QR code. |
| `ota_images` | read-only | The stored OTA images. |
| `commission` | **mutating** | Commission a device over IP or Bluetooth (Wi-Fi / Thread credentials); optionally remember it. |
| `decommission` | **mutating** | Remove our fabric from a device and drop it from the list. |
| `remember` | **mutating** | Keep a record of a listed device. |
| `set_tags` | **mutating** | Replace a remembered device's local tags. |
| `sync` | **mutating** | Sync the device list with the devices on our fabric. |
| `ota_upload` / `ota_delete` | **mutating** | Store / delete an OTA image. |
| `ota_update` | **mutating** | Update a device to a stored image. |

## Safety: read-only by default

The mutating tools touch real devices on a real fabric, so they are **off by
default**: only with `--allow-mutate` are they listed and accepted. Without it,
they are hidden from `tools/list` **and** rejected at `tools/call` (JSON-RPC
error `-32602`). The human operator decides; the agent cannot enable them.

## Running it

```sh
cargo build -p matter-peek-mcp --release    # → target/release/matter-peek-mcp

matter-peek-mcp [--server ws://127.0.0.1:8087] [--allow-mutate]
```

Without `--server`, it holds the controller's store for as long as it runs (it
syncs the device list periodically, as the server does), so no server, CLI or
other MCP server can use that store meanwhile; to share it with the web UI, run
`matter-peek-server` and pass `--server`.

It speaks newline-delimited **JSON-RPC 2.0** on stdin/stdout (the MCP stdio
transport), advertising protocol version `2024-11-05`. Logs go to **stderr**, so
stdout stays a clean protocol channel. It exits when stdin closes.

### Claude Desktop

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

Add `"--allow-mutate"` to `args` only when the agent may change devices.

## Quick manual check

```sh
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' \
  '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_devices","arguments":{}}}' \
  | matter-peek-mcp 2>/dev/null
```

A `tools/call` result is one text content block with the response as JSON
(`isError: true` for failures).

## Design notes

- **`no_std` + `alloc` protocol layer.** The library (JSON-RPC envelope, tool
  catalog, tool ⇄ request mapping, the `--allow-mutate` gate) depends only on the
  API's types (`matter-peek-core` without the controller) and cross-compiles for
  bare-metal targets. It runs a tool through an injected `Dispatch`; the desktop
  binary (`std` feature) implements it over a connection to the API, in process
  or to a server.
- **No tokio MCP SDK.** It is `Send`-bound; the shells run on a single-threaded
  smol executor. MCP over stdio is just JSON-RPC 2.0 over lines, so the framing
  is hand-rolled.
- **Transport:** stdio only for now; HTTP/SSE would be another `Dispatch` user.
