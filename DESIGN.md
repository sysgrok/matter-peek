# matter-peek — Design

> **matter-peek** is a vendor-agnostic, local Matter controller for installers,
> technicians and developers: commission devices (over IP or Bluetooth LE), keep
> an inventory of them, watch their health live, pull their diagnostic logs,
> update their firmware and decommission them — from a web UI, a command line or
> an AI agent, all through one API.

---

## 1. Goals & non-goals

### Goals
- **A local Matter controller** on our own fabric:
  - commission over IP or BLE (handing Wi-Fi or Thread credentials to the
    device), from a manual code or a QR code (also from a picture);
  - an **inventory** of the devices on our fabric, with local tags;
  - **live health** via subscriptions to the diagnostic clusters, and one-shot
    reads;
  - **Diagnostic Logs** (end-user support, network, crash) inline or over BDX;
  - Identify, **OTA updates** (we are the devices' OTA Provider), decommission.
- **Vendor-agnostic**: only standard clusters, so it works with any certified
  device. Proprietary log payloads stay opaque, but are still transported.
- **One API, many front-ends**: whatever the web UI can do is in the API, and
  the CLI and the MCP server expose the same. The CLI grows into a
  `chip-tool`-like tool for humans and scripts; the MCP server is the same for
  AI agents (which know `chip-tool`, not matter-peek).
- **Portable**: the Matter-facing core is `no_std` + `alloc`. Desktop first
  (Linux), then mobile (iOS, Android), possibly embedded (an esp32 hosting the
  controller and its web UI).

### Non-goals (for now)
- A vendor cloud: consent-gated upload of logs and crash dumps comes later.
- Home Assistant integration.
- Re-implementing the Matter stack: this is a thin controller over `rs-matter`;
  what it lacks is added upstream.
- Decoding proprietary crash dumps (later, as per-vendor plugins).

---

## 2. Why a native core, not Wasm-in-browser

Matter needs raw sockets: UDP (and TCP) for PASE/CASE/IM/BDX, multicast for
mDNS, and Bluetooth LE for commissioning. A browser sandbox has none of them, so
the controller is native and the web UI is a client of it. The same native
controller serves the CLI, the MCP server and mobile apps.

---

## 3. Architecture

```
  Web UI (Yew/Wasm) ──── WebSocket/JSON ───┐
  CLI (+ mcp) --server ─ WebSocket/JSON ───┤      matter-peek-server
                                           │ ──►  (WireSession per connection)
                                           │              │
  CLI (+ mcp), no --server ─ in process ──┼──► WireSession┤
  mobile apps ──────── UniFFI ─────────────┤              ▼
  Rust ─────────────── Rust ───────────────┴──►  ┌────────────────────────────────┐
                                                  │ matter-peek-core               │
                                                  │  api: MatterPeek (the API),    │
                                                  │       its types, wire messages,│
                                                  │       MCP tools                │
                                                  │  controller (private):         │
                                                  │   service: the operations      │
                                                  │   matter, ble, discovery, …    │
                                                  │   host (std): thread, socket,  │
                                                  │     store dir, OTA files       │
                                                  └───────────────┬────────────────┘
                                                                  │ rs-matter
                                                                  ▼
                                          Matter devices (UDP / mDNS / BTP / BDX)
```

### The API: `MatterPeek`

`core::api::MatterPeek` is the controller, and its methods are the API:

| Area | Methods |
|---|---|
| Commissioning | `commission` (IP or BLE, network credentials, `remember`, a progress listener), `cancel_commission`, `read_qr_image`, `discover` (mDNS + BLE) |
| Device list | `list_devices`, `sync`, `remember`, `set_tags`, `decommission` |
| Health | `peek_device` (one-shot: also its heap, and the threads (tasks) of its software and their stacks, where it reports them), `subscribe` (live, General Diagnostics only: uptime, reboots, operating hours; returns a `Subscription`, which stops when dropped) |
| Thread | `read_thread_mesh`: networks, nodes, links and findings, from our Thread devices' tables and the LAN's MeshCoP / Matter adverts |
| Device | `read_network` (interfaces, known networks, Wi-Fi / Thread / Ethernet diagnostics), `change_network` (add or update, remove, connect, reorder a known network), `retrieve_logs`, `identify` |
| OTA | `ota_images`, `ota_upload` (returns an `OtaUpload` to write chunks to), `ota_delete`, `ota_update` (a progress listener) |
| Assistant | `assistant`, `set_assistant` (a key stays only for the same API address), `clear_assistant`: its model, kept in the store; never its key, which only `assistant_model` (Rust, for an agent in the same process) returns |

The same methods serve every binding:
- **Rust**: the server, the CLI and tests call them directly.
- **UniFFI** (`uniffi` feature): they are exported as they are, so the
  constraints UniFFI puts on them hold for all: `MatterPeek` is `Send + Sync`
  behind an `Arc`, methods take `&self` and owned arguments, futures are `Send`,
  errors are one `PeekError` enum, and events come through listener traits
  (`HealthListener`, `CommissionListener`, `OtaListener`) the caller implements,
  in Swift / Kotlin too. UniFFI does not cancel futures, hence
  `cancel_commission`.
- **The wire** (`api::wire`): each method is also a `Request` / `Response`, with
  the listeners' calls as `Event`s. `WireSession` runs one client's messages
  against a `MatterPeek`, and owns what outlives a request (its subscriptions
  and uploads, which end with the session).
- **MCP** (`api::mcp`): the methods as tools for AI agents (JSON-RPC envelope,
  tool catalog, tool → `Request` mapping, the `--allow-mutate` gate), run
  through an injected `Dispatch`; `matter-peek mcp` is the stdio shell.
- **Agent** (`api::agent`): a conversation with a model behind an LLM
  `Provider` (OpenAI-compatible today, over a platform `Http`), which runs the
  MCP tools in process. A tool not annotated read-only runs only once the
  user approves it (`AgentListener::approve`); a model that ends with an empty
  reply is asked once more for its answer. Its model (`ModelConfig`: API
  address, model, key, think) is kept in the store; the API shows it as
  `AssistantInfo`, without the key. The assistant runs where the store is,
  so the key never travels: the server's chat hub (`server/src/chat.rs`),
  which `client::connect_in_process` also runs for the CLI on its own. The
  wire's `Chat…` requests list, open, ask, approve, stop and delete its chats,
  kept as JSON in the data directory, with what the model does as
  `Event::Chat`; `AssistantModels` and `AssistantTest` try a model before it
  is kept. `MATTER_PEEK_LLM_*` override the stored model. A chat outlives the
  page that asks: another page sees it at work, and its approvals wait for up
  to 10 minutes. `WireSession` answers the chat requests with an error: the
  chats live with the hub, not in a session.

How values are shown to people is shared too (`api::display`, also exported over
UniFFI): IDs as `0x` + uppercase hex, vendors and products by name with the ID
in brackets (`Shelly (0x1490)`: the device's own name, else the CSA's DCL
registry, embedded and regenerated by `scripts/update-dcl-names.py`), durations
as `2mo 4d 0h 6m 1s`. The web UI, the CLI and the MCP tools all use it.

The types, the messages and the MCP tools are always compiled, also without the
controller (`--no-default-features`), which is what the web UI depends on; an
unused MCP layer is left out by the linker.

### The controller

`MatterPeek` is a handle: each call sends an operation over a channel to the
**service**, which owns the Matter stack (not `Send`) and runs, side by side:
- the transport pump (UDP and, with `ble`, BTP chained on one stack),
- the mDNS backend (permanently, for operational resolves),
- our own node (OTA Provider, BDX, inbound subscription reports),
- the operations (one at a time, plus a periodic sync),
- the subscriptions (detached, so they never block operations).

The service is `no_std` and generic over what the host provides (`api::platform`):
the stack, the crypto, the KV store (`KvBlobStore`), the OTA image store
(`OtaImageStore`), the UDP network (`NetworkSend + NetworkReceive`) and the mDNS
backend (`Mdns`). A host gets a handle and the service with `MatterPeek::new()`
and runs it with `Service::run(Platform { … })`. `MatterPeek::start` (`std`) does
that for this host: it locks the store directory (one controller per store),
binds the socket, picks the mDNS backend by cargo feature (Avahi,
systemd-resolved, Bonjour or rs-matter's own) and runs the service on its own
thread, which ends once every handle is dropped.

Below the service, `matter` (`Controller`), `ble`, `discovery`, `subscribe`,
`logs`, `ota`, `node`, `ca` and `persist` wrap rs-matter's controller side. All
of it is private to `core`.

### Front-ends

| Front-end | Binary | Reaches the controller |
|---|---|---|
| Server | `matter-peek-server` | starts one; serves `ws://127.0.0.1:8087` |
| Web UI (Yew/Wasm, Bulma) | `trunk serve` | WebSocket |
| CLI | `matter-peek` | `--server <url>`, or a `WireSession` in process |
| MCP server | `matter-peek mcp` | as the CLI (read-only unless `--allow-mutate`) |
| Assistant (AI agent) | the server, or `matter-peek` without `--server` | runs where the store is, with the stored model (or `MATTER_PEEK_LLM_*`'s); the web UI and `matter-peek chat` ask it over the wire's `Chat…` requests |
| Mobile apps | — | UniFFI, in process |


---

## 4. Behavior worth knowing

### Devices and the fabric
- One persistent controller fabric. Node IDs come from a persisted counter that
  every successful commissioning bumps; they are never reused.
- A device is **remembered** (a record is kept: taggable, listed while offline)
  or not (listed only while it is on our fabric). `remember` promotes one.
- **Sync** browses our fabric over mDNS: it adds the devices on it that are
  missing from the list, refreshes the others' identity and online state, and
  drops unremembered devices that are gone. The server syncs at start and every
  5 minutes.
- **Decommission** sends `RemoveFabric` over CASE, retried for up to 30 s, and
  drops the device either way (with a warning if it could not be reached);
  `force` skips contacting it.
- Before AddNOC, commissioning removes any fabric on the device that has our
  root key (left over from a lost store), so fabric slots don't leak.

### Commissioning
- Over IP: PASE, AddNOC, then CASE to finish. Over BLE: PASE over BTP, the
  network credentials (Wi-Fi or Thread), ConnectNetwork, then CASE over IP.
- The PASE session stays open until commissioning completes, and on failure the
  fail-safe is disarmed over it (the device then accepts PASE again).
- Concurrent vs. non-concurrent connection is read from General Commissioning
  `SupportsConcurrentConnection`. Devices may get this wrong, so a missing
  ConnectNetwork answer falls through to CASE, which decides.
- Discovery lists only devices in commissioning mode (mDNS `CM` ≠ 0), plus
  commissionable BLE advertisements.

### Health subscriptions
- One subscription per device (General + Software Diagnostics), shared by all
  its listeners; a late one gets the state and merged health so far. It stops
  with its last listener.
- We ask for at most 20 s between reports; devices may choose longer (sleepy
  devices pick their idle duration).
- A device is **offline** when no report arrived within the max interval plus
  two MRP round trips of a report. Re-establishing is retried with a 2–60 s
  backoff; a successful `peek_device` retries at once.

### Network changes
- One `change_network` call is one complete change: arm the fail-safe, make the
  change, commit it (`CommissioningComplete`). What fails is disarmed at once,
  or reverted by the device when its fail-safe expires.
- Connecting to another network, or new credentials for the network the device
  is on, is committed only once the device answers on that network again;
  otherwise it returns to its previous one.
- Removing the network the device is on is refused.
- A new network goes last, as the spec says; where a device puts it elsewhere
  (the Shelly Gen4 uses a free slot), it is moved there in the same change.
- Which network the device is on comes from what it shows of it: for Wi-Fi, its
  access point (BSSID) among a directed scan's results for that SSID; for
  Thread, its extended PAN ID. A network's Connected flag is the fallback only:
  some devices get it wrong (the Shelly Gen4 keeps it on a position in the list,
  not on the network).
- A device names its Thread networks only by extended PAN ID (their datasets
  are write-only). The one it is on is named as its Thread Network Diagnostics
  say; the others, and that one where the diagnostics are empty, as the border routers on the LAN advertise them (MeshCoP's
  `nn` and `xp`, browsed through Avahi and kept for the controller's lifetime);
  else by the extended PAN ID in hex.

### Thread mesh
- `read_thread_mesh` reads each of our Thread devices' networking in turn
  (devices known to be offline are not tried; ops are serial), the Wi-Fi
  channels of our Wi-Fi devices, the border routers' MeshCoP adverts (kept 10
  minutes, as a browse may miss one) and the `_matter._tcp` hosts. A pure
  builder (`controller/mesh.rs`) merges them: nodes by extended address (a
  device's own from its diagnostics, else its Thread interface's hardware
  address), links from the neighbour tables and the established routes of the
  route tables, each side's hearing kept apart. A router only a route table
  knows, with no link seen, is placed by its route: through which router our
  router reaches it, and at what path cost (not drawn as a link). Link quality is 0–3, as
  OpenThread devices report it (the spec scales it to 255).
- A `_matter._tcp` host name is not a Thread extended address, and node IDs
  repeat across fabrics. Our devices' adverts are matched by compressed fabric
  ID and node ID, and add their `SII`/`SAI`/`ICD` to the node's sleep, with
  its ICD Management attributes. Other fabrics' hosts are on Thread when
  their address is in a border router's OMR prefix; they are kept apart as
  LAN nodes, one per host with all its fabrics, not placed in the graph.
- Neighbour and route tables, like ThreadMetrics, are read item by item where
  a device splits a list over chunks.
- The findings are drawn from that partial view: a router is critical only
  for our devices, not its own children, that lose every seen path to a border
  router without it; one-way needs both sides' tables, or a route's quality 0
  one way.

### OTA
We are the devices' OTA Provider: images are stored as files (uploaded in
chunks, checked as they arrive), `ota_update` announces us to the device, and
the device queries and downloads over BDX, reported to an `OtaListener`.

---

## 5. What rs-matter provides

| Need | rs-matter |
|---|---|
| Commissioning | `onboard::Commissioner` (`commission`, `complete_via_case`, `disarm_fail_safe`, `end_pase`) |
| Discovery | `Transport::browse` over the mDNS backends (Avahi, systemd-resolved, Bonjour, builtin) |
| BLE | `Btp`, BlueZ `scan` / `run_central` |
| IM client | `im::client` (read, subscribe, invoke) + the generated cluster clients |
| Reports to a controller | `ReportDataHandler` |
| Diagnostic logs | `diag_logs` client + `bdx` |
| OTA Provider | `ota_prov` cluster + BDX handler, `ota` image header |
| QR / manual codes | `pairing` (incl. `qr-scan` from pixels) |
| Persistence | `KvBlobStore`, `DirKvBlobStore` |

matter-peek follows rs-matter `main` and patches it in the workspace until the
pieces it uses are released.

---

## 6. Portability

### `core` is `no_std` + `alloc`

It builds for bare-metal targets (`riscv32imac-unknown-none-elf`), with
`--no-default-features` (the API types, messages and MCP tools) and with
`--features controller` (the controller too). The rules that keep it so:
- `#![no_std]` + `extern crate alloc`; `core::net` addresses, `alloc`
  collections.
- `core` depends on rs-matter without default features; its `std` feature (pulled
  in by the hosted mDNS backends, `ble` and `uniffi`) re-enables rs-matter's `os`.
- Only `host` (`std`) creates sockets, threads and files; the service takes
  them, and the mDNS backend, from the host (`api::platform`).

### Desktop

Linux today: the server enables the Avahi backend and BlueZ (`ble`). macOS needs
the Bonjour backend (`mdns-astro`) selectable from the server, and has no BLE
central yet; Windows would use the builtin mDNS backend.

### Mobile

`core` builds the bindings itself: with the `uniffi` feature, as a `cdylib` /
`staticlib` (`cargo rustc --crate-type …`, so the no_std builds are unaffected),
and its `uniffi-bindgen` binary (`uniffi-cli` feature) generates the Swift and
Kotlin code. An app calls `MatterPeek.start(config)` with its own store
directory. Still to do for a real app:
- BLE: a GATT central in Swift (CoreBluetooth) / Kotlin, feeding BTP through
  `Btp::process_incoming` / `process_outgoing` (public in rs-matter), behind a
  listener-style trait like the event ones.
- mDNS: Bonjour (`mdns-astro`) on iOS; the builtin backend with a
  `MulticastLock` on Android, or an `NsdManager` backend.
- iOS: the local network permission and `NSBonjourServices`; subscriptions only
  while in the foreground.
- UI: SwiftUI / Jetpack Compose, or first the existing web UI in a WebView
  talking to an in-process server.
- Later: the Apple MatterSupport / Google Home Mobile SDK hand-off.

### Embedded (later)

An esp32 could run the controller: `MatterPeek::new()` and `Service::run` with
its own platform pieces (e.g. `Builtin` mDNS over its multicast socket), plus an
`edge-http`/`edge-ws` shell for the wire. Not tried on hardware yet.

---

## 7. Roadmap

**Done**: commissioning over IP and BLE (Wi-Fi), discovery, QR from a picture;
inventory with remember, tags and sync; one-shot and live health; diagnostic
logs; identify; OTA Provider and updates; decommission; a device's networking
and changes to its networks; web UI, CLI and MCP on one API; the API in `core`
(`MatterPeek`), exported over UniFFI.

**Next**
- Migrating devices to a new Wi-Fi, and other changes to the networks of many
  devices at once.
- Device types (and card icons) and a data-model browser: endpoints, clusters,
  attributes, commands; reading and writing any attribute, invoking any
  command. Needs id → name tables from rs-matter's codegen, and subscriptions
  per set of paths rather than one fixed set.
- Filtering devices by what we know of them (Wi-Fi / Thread / Ethernet, device
  types, clusters, fabrics), next to the user's tags; the same filters select
  devices for bulk changes.
- A device's fabrics (where else it is commissioned), and devices on other
  fabrics only (to have their commissioning windows opened for us).
- Several fabrics of our own ("homes"), and joining another controller's
  fabric with Matter 1.6 Joint Fabric (e.g. Home Assistant's).
- Sharing a device: opening a basic or enhanced commissioning window and showing
  its pairing code and QR code.
- Commissioning progress (percent and stage) from rs-matter's `Commissioner`,
  with questions back to the user (attestation failed or test device: continue?
  a retryable step failed: retry?).
- BLE commissioning onto Thread (in the API, not yet tried on a device).

**Platforms**: running operations concurrently (a long commission blocks the
others today); macOS desktop; Android and iOS apps (above), commissioning
through the platform's own commissioner (Google Play services, Apple) rather
than over BLE, with the chat of `api::agent` built in.

**AI agent**: the assistant (`api::agent`) works with any OpenAI-compatible
API, in the terminal and in the web UI; next are the Anthropic API and the
GitHub Copilot SDK (a Copilot subscription, on the desktop), as more kinds of
`ModelConfig`, and several kept models to pick from per chat. Users of
Claude or ChatGPT apps get the tools through a remote MCP connector: a relay
to a controller that is always on.

**Thread**: moving our Thread devices onto one network (the Thread twin of the
Wi-Fi migration: add and connect under a fail-safe, find the device again,
commit); then, if the mesh from Matter data proves too thin, diagnostics
through the Border Agent (a MeshCoP commissioner: DTLS EC-JPAKE with the PSKc
or ePSKc, `DIAG_GET` traversal), which sees every node.

**Later**: consent-gated upload of logs and crash dumps; per-vendor crash
parsers.

---

## 8. Risks

- **rs-matter `main`**: matter-peek depends on unreleased rs-matter, patched in
  the workspace.
- **mDNS** behaves differently per host daemon; operational resolves need the
  backend running all the time.
- **Devices that misreport** their commissioning flow or reporting behavior;
  the flows fall back rather than trust them.
- **Fabric slots**: the orphan sweep reclaims ours; clearing the store without
  decommissioning still strands them on devices that are offline at the time.
- **Serial operations**: the service runs one operation at a time, so a
  commission (up to minutes over BLE) holds up the others.
- **UniFFI**: the bindings generate, but no app has used them yet; listener
  calls run on the controller's thread and must return quickly.
