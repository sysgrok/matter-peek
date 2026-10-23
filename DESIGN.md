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
  CLI (+ mcp) --server ─ WebSocket/JSON ───┤      matter-peek serve
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
| Commissioning | `commission` (IP or BLE, network credentials, `remember`, a progress listener), `cancel_commission`, `read_qr_image`, `discover` (mDNS + BLE), `advertised_devices` (the commissioned devices on the network, on our fabrics or others', grouped by device) |
| Device list | `list_devices`, `sync`, `remember`, `set_tags`, `decommission` |
| Health | `peek_device` (one-shot: also its heap, and the threads (tasks) of its software and their stacks, where it reports them), `subscribe` (live: General Diagnostics (uptime, reboots, operating hours) and the fabrics the device is on; returns a `Subscription`, which stops when dropped) |
| Thread | `read_thread_mesh`: networks, nodes, links and findings, from our Thread devices' tables and the LAN's MeshCoP / Matter adverts |
| Device | `read_network` (interfaces, known networks, Wi-Fi / Thread / Ethernet diagnostics), `change_network` (add or update, remove, connect, reorder a known network), `retrieve_logs`, `identify` |
| Data model | `read_model` (endpoints, device types, clusters, attribute values, accepted commands, with names and types), `read_events` (the events a device keeps), `subscribe_model` (live: the application endpoints, or everything, with their events), `write_attribute`, `invoke_command`: any cluster, values as JSON |
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
  so the key never travels: the server's chat hub (`matter-peek/src/server/chats.rs`),
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
- our own node (OTA Provider, time source, BDX, inbound subscription
  reports, CASE for devices that reach us),
- the operations (side by side, plus a periodic sync): those without device
  I/O are answered at once; the others queue, in arrival order, on read/write
  locks of the device, the fabric or the sync they use. One commission runs at
  a time; another is refused,
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
| Server | `matter-peek serve` | starts one; serves the API (`ws://127.0.0.1:8087/ws`) and the web UI (`http://127.0.0.1:8087/`) on one port |
| Web UI (Yew/Wasm, Bulma) | taken into `matter-peek` at build time (`trunk serve` while working on it) | WebSocket, to the server that served it |
| CLI | `matter-peek` | `--server <url>`, or a `WireSession` in process |
| MCP server | `matter-peek mcp` | as the CLI (read-only unless `--allow-mutate`) |
| Assistant (AI agent) | the server, or `matter-peek` without `--server` | runs where the store is, with the stored model (or `MATTER_PEEK_LLM_*`'s); the web UI and `matter-peek chat` ask it over the wire's `Chat…` requests |
| Mobile apps | — | UniFFI, in process |


---

## 4. Behavior worth knowing


### One executable
- `matter-peek` is the server (`serve`), the CLI, the MCP server and the
  terminal chat. `serve` answers, on one port, a request to upgrade to
  WebSocket with the API, and any other with a file of the web UI: it peeks
  at the request's head, so the WebSocket handshake reads it whole.
- `build.rs` takes the web UI's files (`MATTER_PEEK_WEB_DIST`, else
  `../matter-peek-web/dist`) into the executable, each gzipped; they go out
  as they are, with `Content-Encoding: gzip` (unpacked only for a client that
  takes no gzip). Without them, `/` says how to add them. An executable to
  hand out should take `trunk build --release`'s: the web crate's release
  profile builds for size and goes through `wasm-opt -Oz` (2.9 MB of wasm
  down to 1.3 MB, 0.5 MB gzipped).
- The web UI reaches the API on its own origin, at `/ws`; `trunk serve`
  passes that on to the server (`Trunk.toml`).
- It links no C library but ring's bit of C (rustls, for the assistant's
  model over HTTPS, with its own root certificates), so it builds static
  with musl, for x86_64 and aarch64 (`.cargo/config.toml` names the C
  compilers). The `dist` profile (LTO, one codegen unit, `opt-level = "s"`,
  stripped) makes it about 11 MB instead of 17 MB; `"z"` would save 0.6 MB
  more, at the crypto's expense (every CASE handshake).
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
- The data model works on any cluster with no code per cluster: `build.rs`
  generates a schema from the Matter IDL rs-matter's clusters come from
  (`rs_matter_codegen::parse_idl`): every cluster's attributes, commands and
  events, their names and types, and the enums, bitmaps and structs they use
  (a cluster's `FeatureMap` as the bits of its `Feature` bitmap; where the
  IDL leaves a field a plain `enum8`/`enum16` that holds a cluster enum's
  values, by that enum: the operational states and errors of the RVC and oven
  clusters, the mode tags). One
  converter reads TLV into JSON and writes JSON as TLV by that schema (enums by
  name, bitmaps as bit names, structs as objects by field name); rs-matter's
  untyped `read_with` / `write_with` / `invoke_with` carry it. The web UI
  builds its editors and command forms from the same types, and adds quick
  controls (switches, sliders, choices, readings): curated for the clusters
  people act on most (lights and their colours, plugs, thermostats, fans,
  covers, locks, closures, valves, pumps, EV chargers, media), one per shape
  for the clusters sharing one (modes, operational states, measurements,
  filters), derived from the schema for the rest. A device card turns over
  to its controls and its latest events; one switch turns them all.
- Events are read with rs-matter's event paths, their fields by the schema;
  delta timestamps count from the event before in the same report.
- `advertised_devices` browses `_matter._tcp` across all fabrics, resolves each
  node and groups the nodes sharing an address into one device. A foreign
  fabric is named (its controller's vendor, its label) when a device of ours is
  on it too: the compressed fabric ID follows from the root key and fabric ID in
  that device's Fabrics list. Nothing more can be read from a device not on our
  fabrics.

### Health subscriptions
- One subscription per device, shared by all its listeners; a late one gets
  the state and merged health so far. It stops with its last listener.
- It takes three paths, as many as a device must accept: General Diagnostics
  (a cluster wildcard, so a device serving part of it still accepts it), and
  Operational Credentials' `Fabrics` and `SupportedFabrics` (mandatory), read
  unfiltered so every fabric shows, not only ours. Ours is the one with our
  fabric ID on which the device has the node ID we gave it. A fabric list sent
  in pieces is merged piece by piece.
- We ask for at most 20 s between reports; devices may choose longer (sleepy
  devices pick their idle duration).
- The same subscription carries the data model while a listener wants it
  (`subscribe_model`): a device with one application endpoint takes that
  endpoint (a wildcard) in place of `SupportedFabrics`, any other everything
  (`*/*/*`, one path). It is re-established when the wanted scope
  widens, and narrows 3 minutes after the last listener of the wider one
  leaves. Its priming comes to the listeners at once, then what changes; no
  report comes sooner than 2 s after the last. Over Thread, 2 devices at a
  time take a data model priming. It follows the events of the endpoints it
  covers too (one event path), from the number after the last one seen, so a
  re-established subscription brings only new ones; the latest 500 are kept
  per device.
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
the device queries and downloads over BDX, reported to an `OtaListener`. The
service also keeps the latest update of each device, for an hour after its
last progress (`ota_updates`), so that any client can follow it.

### Fabrics of ours
- Each fabric of ours has a root CA of its own (RCAC signing directly), a
  random 64-bit fabric ID, our node ID 112233 and vendor 0xFFF1. rs-matter
  persists the fabric (`FabricPersist`); we persist, in one blob, each one's
  name, color and CA signing key, and the default. A store of the
  single-fabric versions migrates on start: its fabric becomes the default,
  "Home", and its lone CA key moves into that blob.
- rs-matter is built with `max-fabrics-16` and `max-sessions-64` on the host:
  a controller holds a CASE session per device it follows, across fabrics.
- Node IDs come from one counter for all fabrics, so a node ID names a device
  across them, and only the operations about a fabric take one (`FabricRef`:
  default, all, index or name): list, sync, commission, the Thread mesh. The
  visible fabric is the client's: the web filters the device list itself;
  `mcp --fabric` and the assistant's chat replace "default" in their requests
  (`Request::default_fabric`).
- Sync browses every fabric; a device without a record leaves the list when
  its own fabric no longer advertises it.
- A device's fabric is ours when its root public key and fabric ID are those
  of one of our fabrics (our first fabric's ID, 1, is also chip-tool's).
- A fabric created or deleted at run time must be (re)advertised, but
  rs-matter signals its mDNS responder only on its own changes. Setting the
  ICD advertisement to another value and back, with no await in between,
  signals it; it then finds the advertisement as it was.

### Sharing (commissioning windows)
`open_commissioning_window` opens an enhanced window (Administrator
Commissioning, a timed invoke over CASE): a random passcode and discriminator,
a 32-byte salt and 1000 PBKDF2 iterations; the device gets only the SPAKE2+
verifier (`w0` and `L`, derived as rs-matter's PASE does, checked against
CHIP's test vector). The manual code, the QR text (discovery: on the network)
and its modules are kept in the service while the window is open, so a reload
or another client shows them again. The device's `WindowStatus` decides what is
shown: the window is ours when its `AdminFabricIndex` is the device's index of
our fabric. The basic window is not offered: it needs the device's own setup
code, which we do not keep.

### Time
- Our node serves the Time Synchronization cluster on endpoint 0, without
  features: `UTCTime`, `Granularity`, `TimeSource`, from the stack's clock,
  which a task keeps on the host's (set again when they drift 50 ms apart, or
  the host's sync state changes). Synchronized (on Linux, the kernel's NTP
  state), it reports milliseconds and SNTP from outside Matter, as CHIP's
  hosts do; else minutes and unknown.
- Each fabric of ours grants any CASE peer View on it (and Operate on the OTA
  Provider). A time sync client opens CASE to us when it has no session, so
  our node answers CASE (Secure Channel), and our operational records carry
  the UDP port we bound.
- Setting a device's clock follows the spec's steps after
  CommissioningComplete: `SetUTCTime`; `SetTimeZone` (the standard offset and
  the IANA name) with the TimeZone feature, then `SetDSTOffset` if the device
  asks for it, with as many of the next DST periods as it keeps; with the
  TimeSyncClient feature, `SetTrustedTimeSource` to our node, endpoint 0. CHIP
  sends these over PASE, under the fail-safe; we send them over CASE once
  commissioned, which never fails a commission, and the same code serves
  devices commissioned before. After a commission it leaves another
  controller's time source in place; on demand it replaces it.
- The host's zone comes from the C library (`localtime_r`): the DST periods
  are found by stepping a day at a time over the next two years; the name from
  `TZ` or the `/etc/localtime` link.

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
| Time source | `time_sync` cluster handler over the stack's clock (`rtc`), `SecureChannel` for CASE |
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
and changes to its networks; the Thread mesh; several fabrics of our own; a
device's fabrics, and removing another controller's; the devices on other
controllers' fabrics; the data model (device types, any attribute read,
written or followed live, any command, the events), with controls on the
back of each card; sharing a device through
an enhanced commissioning window; a time source for our devices, and their
clocks read and set; web UI, CLI and MCP on one API, in one executable; the API in `core` (`MatterPeek`), exported over UniFFI.

**Next**
- Migrating devices to a new Wi-Fi, and other changes to the networks of many
  devices at once.
- Filtering devices by what we know of them (Wi-Fi / Thread / Ethernet, device
  types, clusters, fabrics), next to the user's tags; the same filters select
  devices for bulk changes.
- Devices on other fabrics only (to have their commissioning windows opened
  for us).
- Adding a device to, or moving it to, another fabric of ours (through our
  own commissioning window); exporting and importing a fabric.
- Joint Fabric, as anchor and as administrator: see below.
- Keeping devices' clocks: renewing DST periods before they run out, and
  setting the time again on devices that lost it (no time source of their
  own), without blocking the operations.
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

### Joint Fabric (researched 2026-10, not started)

Joint Fabric (Matter Core, provisional in 1.5.1, final in 1.6) lets several
ecosystems administer one fabric:

- The joint fabric is the anchor's fabric: its root and its fabric ID. Every
  other ecosystem's administrator gets an ICAC cross-signed by the anchor's
  root, with the public key of the ICAC it already has, so it goes on
  issuing NOCs with its own key. Every participant issues NOCs through an
  ICAC.
- Two CATs carry the rights: the Administrator CAT (`0xFFFF`, version 1),
  in every administrator's NOC, to which every device's ACL grants
  Administer; and the Anchor CAT (`0xFFFE`), only in the anchor's NOC (issued
  by an ICAC with `OU=jf-anchor-icac`), to which the administrators' ACLs
  grant Administer.
- Only the anchor hosts the Joint Fabric Datastore (cluster 0x0752): the
  nodes, groups, key sets (the IPK among them), ACLs and bindings of the
  fabric, and its administrators. The others register what they commission
  there, and learn of the others' devices by subscribing to its node list;
  the anchor pushes keys, groups and ACLs to the nodes.
- Joining (the Joint Commissioning Method): the joining administrator opens
  a joint commissioning window (Joint Fabric Administrator cluster, 0x0753;
  advertised with `CM=3`), and the anchor commissions it like a device, with
  extra checks (its Administrator CAT, Vendor ID verification both ways, the
  user's consent) and, over CASE, `ICACCSRRequest` / `AddICAC` for the
  cross-signed ICAC. Commissionable nodes advertise their role in the `JF`
  TXT key (1 available, 2 administrator, 4 anchor, 8 datastore).

What others have (October 2026):
- Home Assistant's Matter Server (matterjs-server, Matter 1.6.1 through
  matter.js 0.18): no Joint Fabric in any role; matter.js has the clusters as
  empty generated stubs. A node manager modeled on the Datastore is a draft
  (matter.js PR #3948, Open Home Foundation roadmap #182). Apple, Google,
  Amazon and SmartThings have announced nothing.
- connectedhomeip: the Administrator cluster is provisional up to tag
  v1.6.1.0 (no longer on `master`), and the work is in two example apps
  (`jf-admin-app`, `jf-control-app`). An anchor sets itself up and keeps a
  Datastore (in memory only) that it pushes to nodes; a joining
  administrator is commissioned through the joint window with the checks
  above, gets an Administrator-CAT NOC on the anchor's fabric, and answers
  `ICACCSRRequest`. There it stops: the anchor never signs that CSR and never
  sends `AddICAC`, so the joined ecosystem cannot commission devices into the
  joint fabric. PR #73591 adds the cross-signing, but has had changes
  requested since August 2026 (its anchor signs a CSR of its own key rather
  than the joiner's). Anchor transfer is not implemented.

What matter-peek would need:
- Fabrics issuing NOCs through an ICAC, keeping their root key (the anchor
  signs ICACs), with the Administrator CAT in our NOC, and the Anchor CAT on
  the anchor's; device NOCs with the Administrator CAT as the admin subject.
  Ours issue NOCs from the root today; "Home" can move, as we hold its root
  key: its devices' NOCs stay valid, each gets an ACL entry for the
  Administrator CAT.
- To join, a commissionable node: a PASE responder (we answer CASE already),
  the root endpoint's clusters, and an endpoint of device type 0x0130 with
  both Joint Fabric clusters. Our node serves the OTA Provider, the time and
  subscription reports today.
- The Datastore (anchor) or its client (administrator); random node IDs,
  unique in its node list; an orphan sweep that leaves other administrators'
  devices alone.
- Vendor ID verification, which anchors trust in the DCL (a root registered
  under the vendor ID): out of reach for test vendors and self-made roots,
  which connectedhomeip stubs too.
- In rs-matter (which already generates both clusters): ICACs signed from
  another's CSR and with an `OU`; the two CATs; the Commissioner taking CATs,
  an admin subject and the ICAC that issued the NOC (it sends the fabric
  table's), with hooks for the extra steps; joint windows (`CM=3`, `JF`); the
  verifier side of Vendor ID verification; the clusters' handlers.

Interoperable peers so far: only connectedhomeip's example apps, and only as
far as `ICACCSRRequest`.

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
- **UniFFI**: the bindings generate, but no app has used them yet; listener
  calls run on the controller's thread and must return quickly.
