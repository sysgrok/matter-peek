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
  MCP / CLI --server ─── WebSocket/JSON ───┤      matter-peek-server
                                           │ ──►  (WireSession per connection)
                                           │              │
  MCP / CLI (no --server) ── in process ───┼──► WireSession┤
  mobile apps ──────── UniFFI ─────────────┤              ▼
  Rust ─────────────── Rust ───────────────┴──►  ┌────────────────────────────────┐
                                                  │ matter-peek-core               │
                                                  │  api: MatterPeek (the API),    │
                                                  │       its types, wire messages │
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
| Health | `peek_device` (one-shot), `subscribe` (live; returns a `Subscription`, which stops when dropped) |
| Device | `retrieve_logs`, `identify` |
| OTA | `ota_images`, `ota_upload` (returns an `OtaUpload` to write chunks to), `ota_delete`, `ota_update` (a progress listener) |

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

The types are always compiled, also without the controller (`--no-default-features`),
which is what the web UI and the MCP protocol layer depend on.

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
| MCP server | `matter-peek-mcp` | `--server <url>`, or in process (read-only unless `--allow-mutate`) |
| Mobile apps | — | UniFFI, in process |

The MCP protocol layer (JSON-RPC, tool catalog, tool ⇄ request mapping) is
`no_std`, behind an injected `Dispatch`; the stdio shell is the `std` part.

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

### `core` and `mcp` are `no_std` + `alloc`

They build for bare-metal targets (`riscv32imac-unknown-none-elf`): `core` with
`--no-default-features` (the API types) and with `--features controller` (the
controller too), and `mcp`'s protocol layer. The rules that keep it so:
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
logs; identify; OTA Provider and updates; decommission; web UI, CLI and MCP on
one API; the API in `core` (`MatterPeek`), exported over UniFFI.

**Next**
- Network diagnostics (Ethernet, Wi-Fi, Thread; network interfaces) and network
  management (the networks a device knows; add / remove / update), then
  migrating devices to a new Wi-Fi.
- Device types (and card icons) and a data-model browser: endpoints, clusters,
  attributes, commands. Needs id → name tables from rs-matter's codegen, and
  subscriptions per set of paths rather than one fixed set.
- Sharing a device: opening a basic or enhanced commissioning window and showing
  its pairing code and QR code.
- Commissioning progress (percent and stage) from rs-matter's `Commissioner`,
  with questions back to the user (attestation failed or test device: continue?
  a retryable step failed: retry?).
- BLE commissioning onto Thread (in the API, not yet tried on a device).

**Platforms**: running operations concurrently (a long commission blocks the
others today); mobile apps (above); macOS desktop.

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
