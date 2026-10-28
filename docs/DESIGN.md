# matter-peek — Design

> **matter-peek** is a vendor-agnostic, local Matter controller for installers,
> technicians and developers. It commissions devices (over IP or Bluetooth LE),
> keeps an inventory of them, follows their health live, retrieves their
> diagnostic logs, updates their firmware and decommissions them. All of this is
> available from a web UI, a command line or an AI agent, through one API.

---

## 1. Goals & non-goals

### Goals
- **A local Matter controller** with fabrics of its own. It should:
  - commission devices over IP or BLE (handing Wi-Fi or Thread credentials to
    the device), from a manual code or a QR code (also from a picture);
  - keep an **inventory** of the devices on our fabrics, with local tags;
  - follow their **live health** through subscriptions to the diagnostic
    clusters, and also read it once on demand;
  - retrieve their **Diagnostic Logs** (end-user support, network, crash),
    inline or over BDX;
  - identify devices, **update their firmware** (we are the devices' OTA
    Provider) and decommission them.
- **Vendor-agnostic.** matter-peek uses only standard clusters, so it works
  with any certified device. Proprietary log payloads are not decoded, but they
  are still transported.
- **One API, many front-ends.** Everything the web UI can do is in the API, and
  the CLI and the MCP server expose the same operations. The CLI is meant to
  grow into a `chip-tool`-like tool for people and scripts. The MCP server plays
  the same role for AI agents, which know `chip-tool`, but not matter-peek.
- **Portable.** The Matter-facing core is `no_std` + `alloc`. The desktop
  (Linux) comes first, then mobile (iOS, Android), and possibly embedded (an
  esp32 hosting the controller and its web UI).

### Non-goals (for now)
- A vendor cloud. Consent-gated upload of logs and crash dumps is planned for
  later.
- Home Assistant integration.
- Re-implementing the Matter stack. matter-peek is a thin controller over
  `rs-matter`, and what rs-matter lacks is added upstream.
- Decoding proprietary crash dumps. This is planned for later, as per-vendor
  plugins.

---

## 2. Why a native core, not Wasm-in-browser

Matter needs raw sockets: UDP (and TCP) for PASE, CASE, IM and BDX, multicast
for mDNS, and Bluetooth LE for commissioning. A browser sandbox provides none of
them. The controller is therefore native, and the web UI is a client of it. The
same native controller also serves the CLI, the MCP server and the apps.

---

## 3. Architecture

```
  Web UI (Yew/Wasm) ──── WebSocket/JSON ───┐
  CLI (+ mcp) --server ─ WebSocket/JSON ───┤      matter-peek serve
  app, on a server ───── WebSocket/JSON ───┤ ──►  (a Hub's Session per connection)
                                           │              │
  CLI (+ mcp), no --server ─ in process ──┼──► Hub Session┤
  app, on this device ─── IPC ─────────────┤              │
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

`core::api::MatterPeek` is the controller, and its methods form the API:

| Area | Methods |
|---|---|
| Commissioning | `commission` (over IP or BLE, with network credentials, `remember` and a progress listener), `cancel_commission`, `read_qr_image`, `discover` (mDNS and BLE), `advertised_devices` (the commissioned devices on the network, on our fabrics or on others', grouped by device) |
| Device list | `list_devices` (with a filter, see [Device filters](#device-filters)), `subscribe_devices` (live: the current list, then the list again each time it changes; returns a `DevicesSubscription`), `sync`, `remember`, `forget`, `set_tags`, `decommission` |
| Health | `peek_device` (a one-shot read, which also includes the heap, and the threads (tasks) of the device's software and their stacks, where the device reports them), `subscribe` (live: General Diagnostics (uptime, reboots, operating hours) and the fabrics the device is on; returns a `Subscription`, which stops when it is dropped) |
| Sleepy devices (ICD) | `register_check_ins`, `unregister_check_ins` (Check-In registration for a device that can idle long), `wake_device`, `stay_awake`, `let_sleep` (keeping such a device awake, so that it answers) |
| Thread | `read_thread_mesh`: the networks, nodes, links and findings, built from our Thread devices' tables and from the MeshCoP and Matter adverts on the LAN |
| Device | `read_network` (interfaces, known networks, and Wi-Fi, Thread or Ethernet diagnostics), `change_network` (add or update, remove, connect or reorder a known network), `retrieve_logs`, `identify` |
| Data model | `read_model` (endpoints, device types, clusters, attribute values and accepted commands, with names and types), `read_events` (the events a device keeps), `subscribe_model` (live: the application endpoints, or everything, with their events), `write_attribute`, `invoke_command`. They work on any cluster, with values as JSON. |
| Fabrics | `fabrics`, `create_fabric`, `rename_fabric`, `set_default_fabric`, `delete_fabric` (our fabrics); `remove_device_fabric` (removes another controller's fabric from a device) |
| Sharing | `open_commissioning_window`, `close_commissioning_window`, `commissioning_window` |
| Time | `device_time`, `set_device_time` |
| OTA | `ota_images`, `ota_upload` (returns an `OtaUpload` to write chunks to), `ota_delete`, `ota_update` (with a progress listener), `ota_updates` |
| Assistant | `assistant`, `set_assistant` (an API key is kept only while the API address stays the same), `clear_assistant`. They manage the assistant's model, which is kept in the store. They never return its key; only `assistant_model` (Rust only, for an agent in the same process) does. |
| Controller | `capabilities` (what this controller can do, fixed by how it was built) |

The same methods serve every binding:
- **Rust.** The server, the CLI and the tests call them directly.
- **UniFFI** (the `uniffi` feature). The methods are exported as they are, so
  the constraints UniFFI puts on them apply to all of them. `MatterPeek` is
  `Send + Sync` behind an `Arc`, methods take `&self` and owned arguments,
  futures are `Send`, and errors are one `PeekError` enum. Events come through
  listener traits (`HealthListener`, `CommissionListener`, `OtaListener`) that
  the caller implements, in Swift or Kotlin too. UniFFI does not cancel
  futures, which is why `cancel_commission` exists.
- **The wire** (`api::wire`). Each method is also a `Request` / `Response`, and
  the listeners' calls are `Event`s. A `WireSession` runs one client's messages
  against a `MatterPeek`. It owns what outlives a single request (the client's
  subscriptions and uploads), and these end with the session.
- **MCP** (`api::mcp`). The methods are offered as tools for AI agents: the
  JSON-RPC envelope, the tool catalog, the mapping from a tool to a `Request`,
  and the `--allow-mutate` gate. The tools run through an injected `Dispatch`.
  `matter-peek mcp` is the stdio shell around them.
- **Agent** (`api::agent`). This is a conversation with a model behind an LLM
  `Provider` (OpenAI-compatible today, over a platform `Http`), which runs the
  MCP tools in process. A tool that is not annotated as read-only runs only
  after the user approves it (`AgentListener::approve`). If a model ends with an
  empty reply, it is asked once more for its answer. The model's settings
  (`ModelConfig`: API address, model, key, think) are kept in the store, and the
  API shows them as `AssistantInfo`, without the key.

  The assistant runs where the store is, so the key never travels. It runs in
  the chats of `matter-peek-server`'s `Hub` (a controller and its assistant).
  The server, the CLI on its own (`client::connect_in_process`) and the app on
  its own controller all run such a hub. The wire's `Chat…` requests list,
  open, ask, approve, stop and delete the chats. The chats are kept as JSON in
  the data directory, and what the model does arrives as `Event::Chat`.
  `AssistantModels` and `AssistantTest` try a model before it is kept. The
  `MATTER_PEEK_LLM_*` environment variables override the stored model.

  A chat outlives the page that asked: another page sees it at work, and its
  approvals wait for up to 10 minutes. `WireSession` answers the chat requests
  with an error, because the chats live in the hub, not in a session.

The way values are shown to people is shared as well (`api::display`, also
exported over UniFFI). IDs are shown as `0x` followed by uppercase hex. Vendors
and products are shown by name, with the ID in brackets (`Shelly (0x1490)`).
The name is the device's own, or else the one in the CSA's DCL registry, which
is embedded and regenerated by `scripts/update-dcl-names.py`. Durations are
shown as `2mo 4d 0h 6m 1s`. The web UI, the CLI and the MCP tools all use this.

The types, the messages and the MCP tools are always compiled, also without the
controller (`--no-default-features`). The web UI depends on this. When the MCP
layer is not used, the linker leaves it out.

### The controller

`MatterPeek` is a handle. Each call sends an operation over a channel to the
**service**, which owns the Matter stack (the stack is not `Send`). The service
runs the following side by side:
- the transport pump (UDP and, with `ble`, BTP, chained on one stack);
- the mDNS backend, all the time, for operational resolves;
- our own node: the OTA Provider, the time source, BDX, inbound subscription
  reports, the Check-Ins of sleepy devices, and CASE for devices that reach us;
- the operations, plus a periodic sync. Operations without device I/O are
  answered at once. The others queue in arrival order, on read/write locks of
  the device, the fabric or the sync they use. One commission runs at a time,
  and a second one is refused;
- the subscriptions, which are detached, so that they never block operations.

The service is `no_std` and generic over what the host provides
(`api::platform`): the stack, the crypto, the KV store (`KvBlobStore`), the OTA
image store (`OtaImageStore`), the UDP network (`NetworkSend + NetworkReceive`)
and the mDNS backend (`Mdns`). A host gets a handle and the service with
`MatterPeek::new()`, and runs the service with `Service::run(Platform { … })`.
`MatterPeek::start` (`std`) does this for the current host. It locks the store
directory (one controller per store), binds the socket, picks the mDNS backend
by cargo feature (Avahi, systemd-resolved, Bonjour or rs-matter's own), and
runs the service on its own thread. The thread ends once every handle has been
dropped.

Below the service, these modules wrap rs-matter's controller side:
- `matter` (`Controller`): the operations the service builds on;
- `discovery` and `mdns`: browsing, and the mDNS backends;
- `ble`: commissioning over Bluetooth LE;
- `basic_info`, `diagnostics` and `network`: a device's identity, its health
  and its networks; `netconfig` changes the networks;
- `subscribe`: the live subscriptions;
- `datamodel`, with `schema` and `values`: any cluster, by name, as JSON;
- `logs`, `ota`, `icd`, `mesh`, `time` and `window`: diagnostic logs, the OTA
  Provider, sleepy devices, the Thread mesh, time and commissioning windows;
- `node`: our own node, which devices reach;
- `fabrics`, `ca` and `persist`: our fabrics, their CAs, and the store.

Next to them, `model` holds the controller's records of devices, `setup_code`
reads a setup code in any of its forms, and `explain` puts errors in plain
words. All of this is private to `core`.

### Front-ends

| Front-end | Binary | How it reaches the controller |
|---|---|---|
| Server | `matter-peek serve` | It starts a controller, and serves the API (`ws://127.0.0.1:17335/ws`, with the access token) and the web UI (`http://127.0.0.1:17335/`) on one port |
| Web UI (Yew/Wasm, Bulma) | Built into `matter-peek-server` at build time (`trunk serve` while working on it) | WebSocket, to the server that served it |
| CLI | `matter-peek` | `--server <url>`, or a `Hub` in its own process |
| MCP server | `matter-peek mcp` | The same way as the CLI (read-only unless `--allow-mutate`) |
| Assistant (AI agent) | The server, or `matter-peek` without `--server` | It runs where the store is, with the stored model (or the one set by `MATTER_PEEK_LLM_*`). The web UI and `matter-peek chat` talk to it with the wire's `Chat…` requests |
| App (Tauri: desktop, Android) | `matter-peek-app`: the web UI in a window | IPC, to a `Hub` in the app; or WebSocket, to a server |
| Native mobile apps (none yet) | — | UniFFI, in process. The bindings build, but no app uses them yet |


---

## 4. Behavior worth knowing


### One executable
- `matter-peek` is the server (`serve`), the CLI, the MCP server and the
  terminal chat. `serve` answers two kinds of requests on one port: a request
  to upgrade to WebSocket gets the API, and any other request gets a file of
  the web UI. The server peeks at the request's head, so that the WebSocket
  handshake can still read it whole.
- `matter-peek-server`'s `build.rs` takes the web UI's files into the crate,
  each one gzipped. It takes them from `MATTER_PEEK_WEB_DIST` if that is set,
  else from `../matter-peek-web/dist`, else from `web-dist`, the copy that is
  kept in git and in the package. The files are sent as they are, with
  `Content-Encoding: gzip`, and are unpacked only for a client that does not
  accept gzip. Without the files, `/` explains how to add them. An executable
  meant for others should take the output of `trunk build --release`: the web
  crate's release profile builds for size and runs `wasm-opt -Oz`, which brings
  the wasm to about 1.8 MB (0.7 MB gzipped).
- The web UI reaches the API on its own origin, at `/ws`. `trunk serve`
  forwards that to the server (`Trunk.toml`).
- The web UI is installable (a web app manifest, `manifest.webmanifest`, and
  icons rendered from `matter-peek-web/icons/*.svg`). Where the browser offers
  it (`beforeinstallprompt`, Chrome and Edge, on localhost or over HTTPS), the
  top bar shows an Install button.
- The API asks for an access token: 16 random bytes, in hex, made at the
  server's first start and kept in the store's directory (`access-token`,
  readable by its owner alone). A client gives it on the WebSocket handshake,
  as `?token=` (browsers cannot set headers on a WebSocket; the web UI keeps it
  in local storage and takes it out of the page's URL) or as
  `Authorization: Bearer` (the CLI). Without it, the server completes the
  handshake and closes the connection with code 4401, which a browser can see,
  unlike an HTTP status. The web UI's files are open: they are the same for
  everyone, and hold nothing of the devices. There is no TLS yet.
- The only C code the executable links is ring's (used by rustls, for the
  assistant's model over HTTPS, with its own root certificates). It therefore
  builds as a static executable with musl, for x86_64 and aarch64
  (`.cargo/config.toml` names the C compilers). The `dist` profile (LTO, one
  codegen unit, `opt-level = "s"`, stripped) makes it about 13 MB instead of
  20 MB. `"z"` would save another 0.6 MB, but at the expense of the crypto's
  speed, which every CASE handshake depends on.

### Devices and fabrics
- Node IDs come from a persisted counter that every successful commissioning
  increments. They are never reused.
- A device is either **remembered** or not. For a remembered device a record is
  kept: it can be tagged, and it is listed while offline. A device that is not
  remembered is listed only while it is on one of our fabrics. `remember`
  promotes a device, and `forget` drops its record and tags.
- **Sync** browses our fabrics over mDNS. It adds the devices that are missing
  from the list, refreshes the identity and online state of the others, and
  drops the devices that are not remembered and are gone. The server syncs at
  start and every 5 minutes. These syncs read again only the devices whose
  details it lacks.
- A device's identity read (one IM read of eight paths, within the nine paths a
  device must accept) also gets its **details**: the Matter version, the
  clusters it serves, its application endpoints, its power sources, how it
  idles (ICD), and its network (SSID or Thread network name, and Thread role).
- **Decommission** sends `RemoveFabric` over CASE, retrying for up to 30 s, and
  drops the device either way (with a warning if the device could not be
  reached). `force` skips contacting the device.
- Before AddNOC, commissioning removes any fabric on the device that has our
  root key (left over from a lost store), so that fabric slots do not leak.

### Device filters
`core::api::query` is the single filter language used by the web UI, the CLI
(`list <filter>`, `filters`), MCP (`list_devices` with `filter`) and the API
(`list_devices`). The rules are:
- Words match the start of a word of anything known about a device (tags,
  names, the facets' values).
- `key:value` matches one facet, and `key:a,b` matches either value.
- `-term` negates a term, and quotes keep spaces.
- Facets with numbers take `<`, `<=`, `>`, `>=` and `=`, in their units
  (`uptime:<1h`, `heap:<16k`, `spec:>=1.4`).

This way the tag `offline` and the state `is:offline` never collide: a plain
word matches both, while `tag:` and `is:` match one each.

The facets come from the device record (`net`, `type`, `vendor`, `product`,
`fabric`, `sw`, `hw`, `node`, `label`, `tag`, and the details: `spec`,
`cluster`, `can`, `power`, `icd`, `endpoints`, `ssid`, `thread`, `role`), from
the health subscription (`eco`, `fabrics`, `uptime`, `reboots`, `hours`,
`heap`) and from the updates (`ota`). `is:` takes states (online, offline,
remembered, shared, fabrics-full, updatable, updating, sleepy, bridge,
test-vendor). The web UI knows the live facts of every card. The server knows
them only for the devices it follows at the moment, so on the server a term on
a live fact matches no other device. The web UI's filter box completes the last
term from the devices shown (facets, and values with the number of devices that
have them). A card's network icon, type, vendor and tags add their term to the
filter.

### Commissioning
- Over IP, commissioning does PASE, AddNOC, and then CASE to finish. Over BLE,
  it does PASE over BTP, sends the network credentials (Wi-Fi or Thread) and
  ConnectNetwork, and then does CASE over IP.
- The PASE session stays open until commissioning completes. On failure, the
  fail-safe is disarmed over it, after which the device accepts PASE again.
- Whether the device supports a concurrent connection is read from General
  Commissioning `SupportsConcurrentConnection`. Devices may get this wrong, so a
  missing ConnectNetwork answer falls through to CASE, which decides.
- Discovery lists only devices in commissioning mode (mDNS `CM` ≠ 0), plus
  commissionable BLE advertisements.
- `advertised_devices` browses `_matter._tcp` across all fabrics, resolves each
  node, and groups the nodes that share an address into one device. A foreign
  fabric is named (by its controller's vendor and its label) when one of our
  devices is on it too: the compressed fabric ID follows from the root key and
  fabric ID in that device's Fabrics list. Nothing more can be read from a
  device that is not on our fabrics.

### Data model
- The data model works on any cluster, with no code per cluster. `build.rs`
  generates a schema from the Matter IDL that rs-matter's clusters come from
  (`rs_matter_codegen::parse_idl`). The schema holds every cluster's attributes,
  commands and events, their names and types, and the enums, bitmaps and
  structs they use. A cluster's `FeatureMap` is described as the bits of its
  `Feature` bitmap. Where the IDL leaves a field as a plain `enum8`/`enum16`
  that holds the values of a cluster enum, the field is described by that enum.
  Examples are the operational states and errors of the RVC and oven clusters,
  and the mode tags.
- One converter reads TLV into JSON and writes JSON as TLV, following that
  schema (enums by name, bitmaps as bit names, structs as objects by field
  name). rs-matter's untyped `read_with` / `write_with` / `invoke_with` carry
  the data.
- The web UI builds its editors and command forms from the same types. It also
  adds quick controls (switches, sliders, choices, readings). The controls are
  curated for the clusters people act on most (lights and their colours, plugs,
  thermostats, fans, covers, locks, closures, valves, pumps, EV chargers,
  media). Clusters that share a shape (modes, operational states, measurements,
  filters) get one control per shape. For the rest, the controls are derived
  from the schema. A device card can be turned over to show its controls and
  its latest events, and one switch turns all the cards.
- Events are read with rs-matter's event paths, and their fields are decoded by
  the schema. Delta timestamps count from the previous event in the same
  report.

### Health subscriptions
- There is one subscription per device, shared by all its listeners. A listener
  that arrives later gets the state and the merged health so far. The
  subscription stops when its last listener goes away.
- It uses three paths, which is as many as a device must accept. The first is
  General Diagnostics, as a cluster wildcard, so that a device that serves only
  part of the cluster still accepts it. The other two are Operational
  Credentials' `Fabrics` and `SupportedFabrics`, which are mandatory. The
  fabrics are read unfiltered, so that every fabric shows, not only ours. Our
  fabric is the one with our fabric ID on which the device has the node ID we
  gave it. A fabric list that is sent in pieces is merged piece by piece.
- We ask for at most 20 s between reports. Devices may choose a longer interval
  (sleepy devices pick their idle duration).
- The same subscription carries the data model while a listener wants it
  (`subscribe_model`). A device with one application endpoint gets that
  endpoint (as a wildcard) in place of `SupportedFabrics`. Any other device gets
  everything (`*/*/*`, one path). The subscription is re-established when the
  wanted scope widens, and it narrows again 3 minutes after the last listener of
  the wider scope leaves. Its priming report goes to the listeners at once, and
  after that only what changes. No report comes sooner than 2 s after the
  previous one. Over Thread, at most 2 devices at a time get a data model
  priming. The subscription also follows the events of the endpoints it covers
  (one event path), starting from the number after the last event seen, so a
  re-established subscription brings only new events. The latest 500 events
  are kept per device.
- A device is **offline** when no report arrived within the maximum interval
  plus two MRP round trips of a report. Re-establishing the subscription is
  retried with a backoff of 2 to 60 s. A successful `peek_device` retries at
  once. A device that sleeps long is the exception, see below.

### Sleepy devices (ICD)
- A device that can idle long (an ICD that supports Check-Ins) sleeps long
  only while a client is registered for its Check-Ins. Otherwise it idles
  short, and answers within seconds. matter-peek registers for the Check-Ins
  of such a device when it commissions it. `register_check_ins` and
  `unregister_check_ins` change this later. The registration (the key and
  the last counter) is kept in the store. When half of the key's counter
  values are used, the next Check-In registers again, with a new key.
- Check-Ins arrive at our node. Each one is matched to a registration by its
  key, and one whose counter is not newer than the last is dropped.
- A device that sleeps long does not answer while it sleeps. It counts as
  awake for its active threshold (5 s if it does not say) after each message
  from it: a report, a Check-In, or a report for a subscription that we lost.
  The operations that need the device wait for it to wake up, for at most its
  idle duration (one hour if it does not say) plus 30 s. The sync does not
  read such a device again while it sleeps, and the Thread mesh skips it.
- Its subscription is counted offline by the same rule as any other. It is
  then not retried with a backoff. Instead it waits for the device to wake up:
  for a Check-In or a stray report, or for 30 minutes at most, in case the
  Check-Ins go astray. A Check-In marks the device online and re-establishes
  the subscription at once. Its subscription is also never narrowed, because a
  narrowed one would be lost until the device wakes up again.
- `wake_device` waits for the device to wake up, then asks it to stay awake for
  30 s (`StayActiveRequest`). `stay_awake` keeps it awake: it asks again 10 s
  before the device would fall asleep, while someone holds it, for 10 minutes
  at most. `let_sleep` ends this.

### Network changes
- One `change_network` call is one complete change: arm the fail-safe, make the
  change, and commit it (`CommissioningComplete`). If something fails, the
  change is disarmed at once, or the device reverts it when its fail-safe
  expires.
- Connecting to another network, or giving new credentials for the network the
  device is on, is committed only once the device answers on that network
  again. Otherwise the device returns to its previous network.
- Removing the network the device is on is refused.
- A new network goes last in the list, as the spec says. Where a device puts it
  elsewhere (the Shelly Gen4 uses a free slot), it is moved to the end in the
  same change.
- Which network the device is on is determined from what the device shows of
  it. For Wi-Fi, this is its access point (BSSID) among the results of a
  directed scan for that SSID. For Thread, it is its extended PAN ID. A
  network's Connected flag is used only as a fallback, because some devices get
  it wrong (the Shelly Gen4 keeps it on a position in the list, not on the
  network).
- A device names its Thread networks only by extended PAN ID, because their
  datasets are write-only. The network the device is on is named as its Thread
  Network Diagnostics report it. The other networks, and also that one when the
  diagnostics are empty, are named as the border routers on the LAN advertise
  them (MeshCoP's `nn` and `xp`, browsed through Avahi and kept for the
  controller's lifetime). Otherwise a network is named by its extended PAN ID
  in hex.

### Thread mesh
- `read_thread_mesh` reads the networking of each of our Thread devices in
  turn. Devices that are known to be offline are not tried. It also reads the
  Wi-Fi channels of our Wi-Fi devices, the border routers' MeshCoP adverts (kept
  for 10 minutes, as a browse may miss one) and the `_matter._tcp` hosts.
- A pure builder (`controller/mesh.rs`) merges all of this. Nodes are
  identified by extended address: a device's own address from its diagnostics,
  or else its Thread interface's hardware address. Links come from the
  neighbour tables and from the established routes of the route tables, and
  each side's view of a link is kept separately. A router that only a route
  table knows about, with no link seen, is placed by its route: through which
  router our router reaches it, and at what path cost. Such a route is not
  drawn as a link. Link quality is 0–3, as OpenThread devices report it (the
  spec scales it to 255).
- A `_matter._tcp` host name is not a Thread extended address, and node IDs
  repeat across fabrics. Our devices' adverts are therefore matched by
  compressed fabric ID and node ID. They add their `SII`/`SAI`/`ICD` keys to the
  node's sleep information, together with its ICD Management attributes. Other
  fabrics' hosts are considered to be on Thread when their address is in a
  border router's OMR prefix. They are kept apart as LAN nodes, one per host
  with all its fabrics, and are not placed in the graph.
- Neighbour and route tables, like ThreadMetrics, are read item by item where a
  device splits a list over several chunks.
- The findings are drawn from this partial view. A router is critical only for
  those of our devices (not its own children) that would lose every seen path
  to a border router without it. A one-way link can only be detected with both
  sides' tables, or from a route with quality 0 in one direction.

### OTA
We are the devices' OTA Provider. Images are stored as files; they are uploaded
in chunks and checked as they arrive. `ota_update` announces us to the device,
and the device then queries for the image and downloads it over BDX. The
progress is reported to an `OtaListener`. The service also keeps the latest
update of each device for an hour after its last progress (`ota_updates`), so
that any client can follow it.

### Fabrics of ours
- Each of our fabrics has its own root CA (the RCAC signs directly), a random
  64-bit fabric ID, our node ID 112233 and vendor 0xFFF1. rs-matter persists
  the fabric (`FabricPersist`). We persist, in one blob, each fabric's name,
  color and CA signing key, and which fabric is the default. A store from the
  single-fabric versions is migrated on start: its fabric becomes the default
  one, named "Home", and its single CA key moves into that blob.
- rs-matter is built with `max-fabrics-16` and `max-sessions-64` on the host,
  because a controller holds a CASE session for every device it follows, across
  fabrics.
- Node IDs come from one counter for all fabrics, so a node ID identifies a
  device across fabrics. Only the operations that are about a fabric take one
  (`FabricRef`: default, all, an index or a name): list, sync, commission, and
  the Thread mesh. The visible fabric is chosen by the client. The web UI
  filters the device list itself, while `mcp --fabric` and the assistant's chat
  replace "default" in their requests (`Request::default_fabric`).
- Sync browses every fabric. A device without a record leaves the list when its
  own fabric no longer advertises it.
- A device's fabric is ours when its root public key and fabric ID match one of
  our fabrics. Our first fabric's ID, 1, is also the one chip-tool uses.
- When a fabric is created or deleted at run time, the advertisements must be
  updated, but rs-matter signals its mDNS responder only on its own changes.
  Setting the ICD advertisement to another value and back, with no await in
  between, signals the responder, which then finds the advertisement as it was.

### Sharing (commissioning windows)
`open_commissioning_window` opens an enhanced commissioning window
(Administrator Commissioning, with a timed invoke over CASE). It uses a random
passcode and discriminator, a 32-byte salt and 1000 PBKDF2 iterations. The
device receives only the SPAKE2+ verifier (`w0` and `L`), which is derived the
same way rs-matter's PASE derives it, and checked against CHIP's test vector.
The manual code, the QR text (discovery: on the network) and its modules are
kept in the service while the window is open, so that a reload or another
client can show them again. The device's `WindowStatus` decides what is shown:
the window is ours when its `AdminFabricIndex` is the device's index of our
fabric. The basic commissioning window is not offered, because it needs the
device's own setup code, which we do not keep.

### Time
- Our node serves the Time Synchronization cluster on endpoint 0, without
  features: `UTCTime`, `Granularity` and `TimeSource`. These come from the
  stack's clock, which a task keeps in line with the host's clock. The task sets
  it again when the two drift 50 ms apart, or when the host's sync state
  changes. When the host is synchronized (on Linux, according to the kernel's
  NTP state), the node reports millisecond granularity and SNTP from outside
  Matter as the source, as CHIP's hosts do. Otherwise it reports minutes and an
  unknown source.
- Each of our fabrics grants any CASE peer View access to it (and Operate on the
  OTA Provider). A time sync client opens CASE to us when it has no session, so
  our node answers CASE (Secure Channel), and our operational records carry the
  UDP port we bound.
- Setting a device's clock follows the spec's steps after
  CommissioningComplete. The first step is `SetUTCTime`. With the TimeZone
  feature, `SetTimeZone` follows (the standard offset and the IANA name), and
  then `SetDSTOffset` if the device asks for it, with as many of the next DST
  periods as the device keeps. With the TimeSyncClient feature,
  `SetTrustedTimeSource` points the device to our node, endpoint 0. CHIP sends
  these commands over PASE, under the fail-safe. We send them over CASE once the
  device is commissioned, so they never make a commission fail, and the same
  code serves devices that were commissioned earlier. After a commission,
  another controller's time source is left in place. On demand, it is replaced.
- The host's time zone comes from the C library (`localtime_r`). The DST
  periods are found by stepping one day at a time over the next two years. The
  zone's name comes from `TZ` or from the `/etc/localtime` link.

---

## 5. What rs-matter provides

| Need | rs-matter |
|---|---|
| Commissioning | `onboard::Commissioner` (`commission`, `complete_via_case`, `disarm_fail_safe`, `end_pase`) |
| Discovery | `Transport::browse` over the mDNS backends (Avahi, systemd-resolved, Bonjour, builtin) |
| BLE | `Btp`, BlueZ `scan` / `run_central` |
| IM client | `im::client` (read, subscribe, invoke) and the generated cluster clients |
| Reports to a controller | `ReportDataHandler` |
| Diagnostic logs | `diag_logs` client and `bdx` |
| OTA Provider | `ota_prov` cluster and BDX handler, `ota` image header |
| Time source | `time_sync` cluster handler over the stack's clock (`rtc`), `SecureChannel` for CASE |
| QR / manual codes | `pairing` (including `qr-scan` from pixels) |
| Persistence | `KvBlobStore`, `DirKvBlobStore` |

matter-peek follows rs-matter's `main` branch and patches it into the
workspace until the pieces it uses are released.

---

## 6. Portability

### `core` is `no_std` + `alloc`

`core` builds for bare-metal targets (`riscv32imac-unknown-none-elf`), both
with `--no-default-features` (the API types, messages and MCP tools) and with
`--features controller` (the controller too). These rules keep it that way:
- `#![no_std]` + `extern crate alloc`; addresses from `core::net`, and
  collections from `alloc`.
- `core` depends on rs-matter without default features. Its `std` feature
  (pulled in by the hosted mDNS backends, `ble` and `uniffi`) enables rs-matter's
  `os` feature again.
- Only `host` (`std`) creates sockets, threads and files. The service receives
  them, and the mDNS backend, from the host (`api::platform`).

### Desktop

Linux is supported today. There, the `matter-peek` executable and the app use
the Avahi backend and BlueZ (`ble`). On Windows, both use the builtin mDNS
backend, which finds its network interface with `if-addrs` there (`nix` is
Unix-only), and they have no BLE. CI builds them for Windows, but they have not
been tried on Windows yet. macOS needs the executables to use the Bonjour
backend (`mdns-astro`) there, and it has no BLE central yet.

### Mobile

The Android app is `matter-peek-app` (Tauri): the web UI in a WebView, on a
controller in the app (a `Hub`) or on a server. In the app, `core` uses
rs-matter's builtin mDNS backend (`mdns-builtin`), and the app holds a Wi-Fi
`MulticastLock` while it is in the foreground. An iOS app is not started;
building one needs a Mac.

The app commissions with Google Play services' Matter API
(`play-services-home`), through a small Tauri plugin in Kotlin (`PlayPlugin`,
in `gen/android`) whose commands the page calls directly:
- Google Play services scans the QR code, does the BLE part and puts the
  device on Wi-Fi or Thread. It commissions the device onto its own local
  fabric (the "Android fabric"), opens a commissioning window, and hands the
  device over to the app's `CommissioningService` (`HandoffService`): an IP
  address, a port and a passcode.
- The service passes these to the page on a channel. The page commissions the
  device over IP like any other device, so this works with the controller in
  the app and with a server. It then tells Google Play services how it went,
  with the device's node ID, and Google Play services closes its screens.
- Google Play services uses the Android fabric only for this, and Android does
  not list the device as its own afterwards. So once its screens close, the
  page removes that fabric from the device. The commission finds it while it
  reads the device's fabrics over PASE, before AddNOC: `AdminFabricIndex`
  names the fabric whose administrator opened the window, and the result
  reports it as `window_opener`.
- The app's activity handles `ACTION_COMMISSION_DEVICE`, which lists the app in
  Google Home's *Linked Matter apps & services*. A device shared from there
  comes with the pairing code of the window Google Home opened. The page
  passes this code back to Google Play services' commissioning, as Google's
  sample app does, and the device is handed over in the same way.
- The other way round, the Share popup hands a device to another Matter app on
  the phone: it opens our enhanced window, then gives its discriminator and
  passcode (now part of `CommissioningWindow`), with the device's name and IDs,
  to `CommissioningClient.shareDevice`. Android then lists the apps that handle
  `ACTION_COMMISSION_DEVICE`, and the one picked commissions the device.

`core` can also build native bindings itself. With the `uniffi` feature, it
builds as a `cdylib` / `staticlib` (`cargo rustc --crate-type …`, so the no_std
builds are unaffected), and its `uniffi-bindgen` binary (the `uniffi-cli`
feature) generates the Swift and Kotlin code. A native app would call
`MatterPeek.start(config)` with its own store directory. No app uses the
bindings yet.

Still to do on mobile:
- BLE: a GATT central in Swift (CoreBluetooth) or Kotlin, feeding BTP through
  `Btp::process_incoming` / `process_outgoing` (public in rs-matter), behind a
  listener-style trait like the event ones.
- mDNS: Bonjour (`mdns-astro`) on iOS. On Android the builtin backend with a
  `MulticastLock` is in use; an `NsdManager` backend would be an alternative.
- iOS: the local network permission and `NSBonjourServices`; subscriptions only
  while the app is in the foreground.
- UI: the existing web UI in a WebView is in use (through Tauri). SwiftUI or
  Jetpack Compose would be the native alternatives.
- Later: the hand-off to and from Apple's MatterSupport.

### Embedded (later)

An esp32 could run the controller with `MatterPeek::new()` and `Service::run`,
using its own platform pieces (for example the `Builtin` mDNS backend over its
multicast socket), plus an `edge-http`/`edge-ws` shell for the wire. This has
not been tried on hardware yet.

---

## 7. Roadmap

**Done**:
- commissioning over IP and BLE (onto Wi-Fi and Thread), discovery, and QR
  codes read from a picture;
- the inventory, with remember, tags and sync;
- one-shot and live health; diagnostic logs; identify;
- the OTA Provider and updates; decommissioning;
- a device's networking, and changes to its networks;
- the Thread mesh;
- several fabrics of our own; a device's fabrics, and removing another
  controller's fabric from a device;
- the devices on other controllers' fabrics;
- device filters over tags and everything known about a device;
- the data model (device types; any attribute read, written or followed live;
  any command; the events), with controls on the back of each card;
- sharing a device through an enhanced commissioning window;
- a time source for our devices, and reading and setting their clocks;
- sleepy (ICD) devices: Check-In registration, and keeping a device awake so
  that it answers;
- operations that run concurrently;
- the web UI, the CLI and MCP on one API, in one executable; the API in `core`
  (`MatterPeek`), exported over UniFFI;
- the app (Tauri), on the desktop and on Android; on Android, commissioning
  with Google Play services, and devices shared from Google Home.

**Next**
- Migrating devices to a new Wi-Fi network, and other changes to the networks
  of many devices at once, selected with a filter.
- Adding a device to another fabric of ours, or moving it there (through our
  own commissioning window); exporting and importing a fabric.
- Joint Fabric, as the anchor and as an administrator (see below).
- Keeping devices' clocks: renewing DST periods before they run out, and
  setting the time again on devices that lost it (those without a time source
  of their own), without blocking the operations.
- Commissioning progress (percentage and stage) from rs-matter's
  `Commissioner`, with questions back to the user (attestation failed, or a
  test device: continue? A retryable step failed: retry?).

**Platforms**: the macOS and Windows desktops; an iOS app, commissioning
through Apple's own commissioner (MatterSupport) rather than over BLE.

**AI agent**: The assistant (`api::agent`) works with any OpenAI-compatible
API, in the terminal and in the web UI. Next are the Anthropic API and the
GitHub Copilot SDK (with a Copilot subscription, on the desktop) as more kinds
of `ModelConfig`, and several kept models to choose from per chat. Users of the
Claude or ChatGPT apps could get the tools through a remote MCP connector,
which needs a relay to a controller that is always on.

**Thread**: Moving our Thread devices onto one network. This is the Thread
counterpart of the Wi-Fi migration: add and connect under a fail-safe, find the
device again, and commit. After that, if the mesh built from Matter data proves
too thin, diagnostics through the Border Agent (a MeshCoP commissioner: DTLS
EC-JPAKE with the PSKc or ePSKc, and `DIAG_GET` traversal), which sees every
node.

**Later**: consent-gated upload of logs and crash dumps; per-vendor crash
parsers.

### Joint Fabric (researched 2026-10, not started)

Joint Fabric (Matter Core; provisional in 1.5.1, final in 1.6) lets several
ecosystems administer one fabric:

- The joint fabric is the anchor's fabric, with its root and its fabric ID.
  Every other ecosystem's administrator gets an ICAC cross-signed by the
  anchor's root, carrying the public key of the ICAC it already has, so it can
  go on issuing NOCs with its own key. Every participant issues NOCs through an
  ICAC.
- Two CATs carry the rights. The Administrator CAT (`0xFFFF`, version 1) is in
  every administrator's NOC, and every device's ACL grants it Administer. The
  Anchor CAT (`0xFFFE`) is only in the anchor's NOC (issued by an ICAC with
  `OU=jf-anchor-icac`), and the administrators' ACLs grant it Administer.
- Only the anchor hosts the Joint Fabric Datastore (cluster 0x0752). It holds
  the nodes, groups, key sets (the IPK among them), ACLs and bindings of the
  fabric, and its administrators. The other administrators register what they
  commission there, and learn about each other's devices by subscribing to its
  node list. The anchor pushes keys, groups and ACLs to the nodes.
- Joining works with the Joint Commissioning Method. The joining administrator
  opens a joint commissioning window (Joint Fabric Administrator cluster,
  0x0753; advertised with `CM=3`), and the anchor commissions it like a device,
  with extra checks (its Administrator CAT, Vendor ID verification in both
  directions, and the user's consent). Over CASE, `ICACCSRRequest` and
  `AddICAC` then provide the cross-signed ICAC. Commissionable nodes advertise
  their role in the `JF` TXT key (1 available, 2 administrator, 4 anchor,
  8 datastore).

What others have (October 2026):
- Home Assistant's Matter Server (matterjs-server, Matter 1.6.1 through
  matter.js 0.18) has no Joint Fabric support in any role, and matter.js has
  the clusters only as empty generated stubs. A node manager modeled on the
  Datastore is a draft (matter.js PR #3948, Open Home Foundation roadmap #182).
  Apple, Google, Amazon and SmartThings have announced nothing.
- In connectedhomeip, the Administrator cluster is provisional up to tag
  v1.6.1.0 (it is no longer on `master`), and the work is in two example apps
  (`jf-admin-app`, `jf-control-app`). An anchor sets itself up and keeps a
  Datastore (in memory only), which it pushes to nodes. A joining administrator
  is commissioned through the joint window with the checks above, gets an
  Administrator-CAT NOC on the anchor's fabric, and answers `ICACCSRRequest`.
  The work stops there: the anchor never signs that CSR and never sends
  `AddICAC`, so the joined ecosystem cannot commission devices into the joint
  fabric. PR #73591 adds the cross-signing, but has had changes requested since
  August 2026 (its anchor signs a CSR of its own key rather than the joiner's).
  Anchor transfer is not implemented.

What matter-peek would need:
- Fabrics that issue NOCs through an ICAC while keeping their root key (the
  anchor signs ICACs), with the Administrator CAT in our NOC and the Anchor CAT
  in the anchor's; and device NOCs with the Administrator CAT as the admin
  subject. Our fabrics issue NOCs from the root today. "Home" can be moved,
  because we hold its root key: its devices' NOCs stay valid, and each device
  gets an ACL entry for the Administrator CAT.
- To join, a commissionable node: a PASE responder (we already answer CASE),
  the root endpoint's clusters, and an endpoint of device type 0x0130 with both
  Joint Fabric clusters. Today our node serves the OTA Provider, the time, and
  subscription reports.
- The Datastore (as the anchor) or its client (as an administrator); random
  node IDs, unique in its node list; and an orphan sweep that leaves other
  administrators' devices alone.
- Vendor ID verification, for which anchors trust the DCL (a root registered
  under the vendor ID). This is out of reach for test vendors and self-made
  roots, which connectedhomeip stubs too.
- In rs-matter (which already generates both clusters): ICACs signed from
  another party's CSR and with an `OU`; the two CATs; the Commissioner taking
  CATs, an admin subject and the ICAC that issued the NOC (today it sends the
  fabric table's), with hooks for the extra steps; joint windows (`CM=3`,
  `JF`); the verifier side of Vendor ID verification; and the clusters'
  handlers.

So far, the only interoperable peers are connectedhomeip's example apps, and
only as far as `ICACCSRRequest`.

---

## 8. Risks

- **rs-matter `main`.** matter-peek depends on unreleased rs-matter, patched
  into the workspace.
- **mDNS** behaves differently with each host daemon, and operational resolves
  need the backend running all the time.
- **Devices that misreport** their commissioning flow or their reporting
  behavior. The flows fall back rather than trust them.
- **Fabric slots.** The orphan sweep reclaims ours, but clearing the store
  without decommissioning still strands them on devices that are offline at the
  time.
- **UniFFI.** The bindings generate, but no app has used them yet. Listener
  calls run on the controller's thread and must return quickly.
