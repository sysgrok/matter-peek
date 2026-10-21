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
controller serves the CLI, the MCP server and, later, mobile apps.

---

## 3. Architecture

```
  Web UI (Yew/Wasm) ──── WS/JSON ────┐
  MCP server (stdio) ─── WS/JSON ────┤
  CLI ── WS/JSON (--server) ─────────┤
      └─ in process (channels) ──────┤
  mobile (later) ── UniFFI, in process ─┤
                                        ▼
                ┌──────────────────────────────────────────┐
                │ the API: matter-peek-proto               │
                │   Request / Response / Event (serde)     │
                ├──────────────────────────────────────────┤
                │ the service: rpc::actor                  │
                │   device inventory + sync, commissioning,│
                │   subscription hub, OTA images, logs, …  │
                ├──────────────────────────────────────────┤
                │ matter-peek-core (no_std + alloc)        │
                │   Controller: commission (IP/BLE),       │
                │   discover, read/subscribe health, logs, │
                │   identify, decommission, OTA Provider   │
                │   node; CA; KV-backed store              │
                └──────────────────┬───────────────────────┘
                                   │ rs-matter
                                   ▼
                     Matter devices (UDP / mDNS / BTP / BDX)
```

### The API is the message protocol

The API is `proto`: requests answered by one response each, plus events streamed
in between (commissioning steps, health reports, subscription states, OTA
progress). It is `no_std` + `alloc` and depends only on `serde`, so the native
server, the Wasm UI and any future shell share it.

| Area | Requests |
|---|---|
| Commissioning | `Commission` (IP or BLE, network credentials, `remember`), `Cancel`, `ReadQrImage`, `Discover` (mDNS + BLE) |
| Inventory | `ListDevices`, `Sync`, `Remember`, `SetTags`, `Decommission` |
| Health | `PeekDevice` (one-shot), `Subscribe` / `Unsubscribe` (live) |
| Device | `RetrieveLogs`, `Identify` |
| OTA | `OtaImages`, `OtaUploadBegin` / `Chunk` / `End`, `OtaDelete`, `OtaUpdate` |

There is no Rust-trait facade over the core: every front-end speaks `proto`, so
every operation has exactly one implementation (the service) and the front-ends
cannot drift apart.

### The service

`rpc::actor` implements the API. One actor task owns the Matter stack
(single-threaded: `rs_matter::Matter` is not `Send`) and runs, side by side:
- the transport pump (UDP and BTP chained on one stack),
- the mDNS backend (permanently, for operational resolves),
- our own node (OTA Provider, BDX, inbound subscription reports),
- the command loop (one command at a time, plus a periodic sync),
- the subscriptions (detached, so they never block commands).

A client reaches it through a `rpc::client::Connection`: over a WebSocket to a
running `matter-peek-server`, or in process (`connect_in_process`), which spawns
the actor and the same dispatch without a network hop. The store
(`<data dir>/matter-peek`) is locked, so only one process holds it at a time.

### The core

`matter-peek-core` wraps rs-matter's controller-side primitives behind
`Controller`: commission (with an orphan-fabric sweep before AddNOC), BLE
commissioning (`ble` feature), discovery, health reads and subscriptions,
diagnostic logs, identify, decommission, OTA announcements, and our node. Plus
the controller CA and `ControllerStore`, generic over rs-matter's `KvBlobStore`.
Its types (`core::model`) carry no serde; `rpc::convert` maps them to `proto`.

### Front-ends

| Front-end | Binary | Reaches the service |
|---|---|---|
| Web UI (Yew/Wasm, Bulma) | `trunk serve` | WebSocket |
| CLI | `matter-peek` | `--server <url>`, or in process |
| MCP server | `matter-peek-mcp` | WebSocket (read-only unless `--allow-mutate`) |
| Server | `matter-peek-server` | hosts the service, `ws://127.0.0.1:8087` |

The MCP protocol layer (JSON-RPC, tool catalog, tool ⇄ `proto` mapping) is
`no_std`, behind an injected `Dispatch`; the stdio shell is the `std` part.

---

## 4. Behavior worth knowing

### Devices and the fabric
- One persistent controller fabric. Node IDs come from a persisted counter that
  every successful commissioning bumps; they are never reused.
- A device is **remembered** (a record is kept: taggable, listed while offline)
  or not (listed only while it is on our fabric). `Remember` promotes one.
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
- One subscription per device (General + Software Diagnostics), shared by every
  watching connection; a late joiner gets the state and merged health so far.
  It stops with its last watcher.
- We ask for at most 20 s between reports; devices may choose longer (sleepy
  devices pick their idle duration).
- A device is **offline** when no report arrived within the max interval plus
  two MRP round trips of a report. Re-establishing is retried with a 2–60 s
  backoff; a successful `PeekDevice` retries at once.

### OTA
We are the devices' OTA Provider: images are stored as files (uploaded in
chunks over the API), `OtaUpdate` announces us to the device, and the device
queries and downloads over BDX, reported as `OtaProgress` events.

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

### `core`, `proto` and `mcp` are `no_std` + `alloc`

They build for bare-metal targets (`riscv32imac-unknown-none-elf`; `core` with
`--no-default-features`). The rules that keep it so:
- `#![no_std]` + `extern crate alloc`; `core::net` addresses, `alloc`
  collections.
- `core` depends on rs-matter without default features; its `std` feature (pulled
  in by the hosted mDNS backends and `ble`) re-enables rs-matter's `os`.
- `core` never creates sockets or opens files: the binary binds the socket and
  injects the KV store.

### Desktop

Linux today: the server enables the Avahi backend and BlueZ (`ble`). macOS needs
the Bonjour backend (`mdns-astro`) selectable from the server, and has no BLE
central yet; Windows would use the builtin mDNS backend.

### Mobile (proposed)

The API is already a message protocol with an in-process transport, so mobile
binds to the same service rather than to the core:
1. Move the service (actor, conversions, in-process client) out of `rpc` into
   its own crate, with the store directory, socket and backends injected; `rpc`
   keeps the WebSocket server.
2. A `uniffi` crate over it: a session with an async `call(Request) -> Response`
   and an event callback; UniFFI derives on the `proto` types behind a feature.
3. Platform pieces behind it:
   - BLE: a GATT central in Swift (CoreBluetooth) / Kotlin, feeding BTP through
     `Btp::process_incoming` / `process_outgoing` (public in rs-matter).
   - mDNS: Bonjour (`mdns-astro`) on iOS; the builtin backend with a
     `MulticastLock` on Android, or an `NsdManager` backend.
   - iOS: the local network permission and `NSBonjourServices`; subscriptions
     only while in the foreground.
4. UI: SwiftUI / Jetpack Compose, or first the existing web UI in a WebView
   talking to the in-process service.
5. Later: the Apple MatterSupport / Google Home Mobile SDK hand-off.

### Embedded (later)

An esp32 could host the service with an `edge-http`/`edge-ws` shell instead of
the desktop one. That needs the service itself `no_std`: today it uses `std`
collections, files (store lock, OTA images), `UdpSocket` and the `image` crate,
so it is a real port, not a re-shelling. Deferred until there is hardware to
validate against.

---

## 7. Roadmap

**Done**: commissioning over IP and BLE (Wi-Fi), discovery, QR from a picture;
inventory with remember, tags and sync; one-shot and live health; diagnostic
logs; identify; OTA Provider and updates; decommission; web UI, CLI and MCP on
one API.

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

**Platforms**: the service crate split, then mobile (above); macOS desktop.

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
- **The service** keeps growing in one file (`actor.rs`); split it when it moves
  to its own crate.
- **UniFFI** async calls and callback interfaces for long flows: spike before
  committing the mobile shape.
