# Phase 1 — Task (3): Persistent fabric, device store, tags, Identify, re-sync

Scoping note for the last Phase 1 chunk. Goal: move from the stateless
"commission → peek → decommission" vertical to a **persistent** model where a
device is commissioned **once** and re-addressed cheaply thereafter, with local
metadata (tags) and the operational hygiene that requires.

## Building blocks (all verified present in rs-matter)

- `FabricPolicy::Persistent` already exists in the model (only `Ephemeral` is
  wired today).
- **Identify** cluster (`0x0003`): `IdentifyRequest { identify_time }` →
  client `identify().identify(ep, |r| r.identify_time(secs))`. Self-contained.
- **BasicInformation** (`0x0028`) stable identity: `unique_id`, `serial_number`,
  `node_label` — readable via the generated client (`unique_id_read(0)` …).
- **OperationalCredentials** (`0x003e`): `Fabrics` list read + `RemoveFabric`
  (already used by `decommission`) — the basis for orphan cleanup.

## The one real structural challenge: a long-lived Matter stack

Today the actor builds a **fresh `Matter` per command** (see `actor.rs` module
doc) — correct for ephemeral (each run installs then removes a controller-local
fabric). Persistent mode needs the **same** controller fabric (RCAC/NOC/IPK) to
survive across commands, so a CASE re-address works without re-commissioning.

Two sub-problems:

1. **Stack lifetime.** A persistent controller stack must outlive individual
   commands. The self-referential `Matter<'a>` can't be stored in a struct field
   easily; the cleanest fit is a dedicated **long-lived actor task** that owns
   the stack for the whole session and services commands over a channel (the
   transport pump runs continuously alongside). This is a variant of the actor
   we have — one persistent task instead of per-command scopes.

2. **Controller identity persistence — mostly free.** Research finding: a
   controller fabric is just a `Fabric`, and rs-matter already persists fabrics
   through `KvBlobStore` via `Matter::load_persist` / save (`fabrics.load_persist`,
   TLV serialization already implemented). So persisting our RCAC/NOC/IPK is not
   bespoke work — we install our fabric once, then `load_persist`/save handles it.
   `rs_matter::persist::DirKvBlobStore` is a ready filesystem-backed KV (no impl
   to write). Remaining work: generate the CA *once* (not per-`commission`) and
   reload it on startup via `load_persist`.

## Storage — two worlds, deliberately separated

### World (a): fabric/creds + device identity + tags → KV store (NO SQL)

Small, bounded (~50–200 devices), low write rate, read in full at startup,
filtered in memory. **No SQL engine, no async-to-sqlite bridge, no new heavy
dependency.** Built on rs-matter's `KvBlobStore` (`DirKvBlobStore` on desktop):

- **Controller fabric / creds**: persisted for free by `Matter::load_persist`.
- **Device identity + tags**: our own small blobs under our own `u16` key range
  in the *same* KV — `node_id → { unique_id, serial_number, node_label,
  device_fabric_index, tags: Vec<String> }`, serialized via serde/TLV. Loaded
  into memory; tag filtering/grouping done in RAM (trivial at this scale).

This layer is `no_std`-friendly and a clean candidate to **graduate into
rs-matter** later as a generic controller-side "commissioned-peer store" — which
is a reason *not* to couple it to SQLite/std.

### World (b): device telemetry / stats over time → DEFERRED, and NOT in core

Time-series data (uptime, reboot counts, heap watermarks, Thread/Wifi/netif
stats sampled over time) is unbounded and growing — the only data that might
justify a real engine, and it is **time-series-shaped, not SQL-shaped**. It is
an application/cloud concern, **out of the `no_std` core**, and explicitly
future (cf. the consent-gated upload phase). We design it when we build it —
possibly "stream to cloud, persist nothing locally" — and the engine choice (and
any graphing/visualization, which is a UI-layer concern, never a core
dependency) is deferred until then. Not in scope for Phase 1.

## Re-sync / orphaned-fabric hygiene (the bug you flagged)

Two out-of-sync cases (from the project notes):

- **Device forgot us** (factory reset): CASE fails → flag offline → prompt
  re-share → on re-commission, match by `unique_id`/`serial_number` and
  *overwrite* the stale device row (no duplicate).
- **We forgot device** (store wiped / reinstall): before `AddNOC` during a new
  commissioning, **read the `Fabrics` list over the PASE session** and
  `RemoveFabric` any entry whose root public key matches ours, so we never leak
  a device fabric slot. (This is also a standalone robustness win, independent
  of persistence.)

## Proposed sequencing

1. **Identify blink** *(small, independent, high installer-UX value)* —
   `Controller::identify(handle, secs)`; proto `Identify` op; a "Locate" button
   per device in the UI. No store/persistence dependency. **Ship first.**
2. **Persisted controller identity** — generate-once CA, persist/reload; the
   prerequisite for everything persistent. Add the long-lived actor task.
3. **Device store + tags** — KV blobs (NOT SQL); capture `unique_id`/serial/label
   at commission; `Persistent` policy keeps the device; tag CRUD + filter in
   proto + UI (filtering in memory).
4. **Re-address (CASE-only) flow** — `read_health`/`retrieve_logs`/`identify`
   against a *stored* device without re-commissioning.
5. **Re-sync + orphan cleanup** — the two out-of-sync cases above.

Rationale: (1) is a quick, self-contained win that improves the discovery UI
immediately. (2) unblocks the rest. (3)–(5) build the persistent model in
dependency order, each independently testable against the `system_tests`
accessory.

## Decisions (resolved)
- **Store engine**: none — KV (`KvBlobStore`/`DirKvBlobStore`) + in-memory
  filtering for world (a). No rusqlite/sqlx (avoids an async-to-sync bridge and
  keeps the core `no_std`-friendly / mobile-ready / graduatable upstream).
- **Controller CA persistence**: reuse rs-matter's fabric persistence
  (`Matter::load_persist` / `fabrics.load_persist`) — no bespoke serialization.
- **Telemetry/stats + graphing**: world (b), deferred and out of the core.
- **This iteration**: (1) Identify first, then (2) the persistence foundation
  (long-lived actor + `DirKvBlobStore` + generate-once CA + `load_persist`).
