# Matter Mini Commissioner / Controller — Design & Plan

> Working name: **matter-peek** (rename freely). A vendor-agnostic Matter
> diagnostic tool for installers/technicians: commission into one or more
> devices, read their health via the standard diagnostic clusters, and pull
> **DiagnosticLogs** (crash dumps / logs) over BDX. Future: consent-gated
> upload of logs & core dumps to a vendor cloud.

---

## 1. Goals & non-goals

### Goals
- **Peek into device health** via standard Matter diagnostic clusters:
  General (`gen_diag`), Software (`sw_diag`), Thread (`thread_diag`),
  Wi-Fi (`wifi_diag`), Ethernet (`eth_diag`), plus the **Diagnostic Logs**
  cluster (`diag_logs`) — `RetrieveLogsRequest` for `EndUserSupport`,
  `NetworkDiag`, and `CrashLogs` intents, streamed via **BDX**.
- **Vendor-agnostic**: works against any certified Matter device by relying
  only on standard clusters. (Proprietary, encrypted crash-dump *payloads*
  remain opaque for other vendors' devices — but the generic
  retrieve/transport mechanism still works everywhere.)
- **Both fabric models**, selectable per session (see §5):
  - *Persistent*: commission once, keep the device on our fabric, manage it
    locally with tags + filtering + Identify-blink.
  - *Ephemeral*: throwaway fabric per fetch, `RemoveFabric` self-destruct.
- **Lock-step core + web UI**: the web UI is the first validation surface,
  runnable on the dev desktop, so UI requirements drive the core's API shape.
- **Mobile-ready core**: the same core compiles into a native lib behind
  UniFFI for eventual SwiftUI (iOS) and Jetpack Compose (Android) UIs.

### Non-goals (initially)
- No persistent vendor cloud / log-upload backend yet (consent-gated upload
  is a later phase; the core will expose a clean seam for it).
- No Home Assistant integration (explicitly out of scope).
- No re-implementation of the Matter stack — we build a thin controller
  wrapper over `rs-matter`.
- No proprietary crash-dump *decoding* (we transport the bytes; per-vendor
  parsers are a later, pluggable concern).

---

## 2. Why Option A (native core), not Wasm-in-browser

Matter is fundamentally raw-socket: mDNS/DNS-SD needs UDP multicast for
discovery; PASE/CASE/BDX need raw UDP (and optionally TCP). A browser/Wasm
sandbox cannot open those sockets. So the **core must be native** with direct
OS socket access. This is also what `rs-matter` already targets.

The "web" requirement is about the **UI**, not the core's compilation target.
We therefore keep the core native and put a thin boundary between it and the
web UI (next section). This preserves the exact same core for the mobile
UniFFI path.

---

## 3. Architecture overview

```
            ┌──────────────────────────────────────────────────────┐
            │                    UI layer (per platform)            │
            │                                                       │
  desktop → │  Web UI (Yew/Wasm OR plain TS) ── WS/HTTP+JSON ─┐     │
            │                                                 │     │
  iOS     → │  SwiftUI ───────────────── UniFFI ──────────┐   │     │
  Android → │  Jetpack Compose ───────── UniFFI ──────────┤   │     │
            └─────────────────────────────────────────────┼───┼─────┘
                                                           │   │
                                          ┌────────────────▼───▼──────────┐
                                          │   matter-peek-core (Rust)     │
                                          │   - Session/Controller mgr    │
                                          │   - Commissioning (PASE→CASE) │
                                          │   - Diagnostics readers       │
                                          │   - DiagnosticLogs + BDX recv │
                                          │   - Local store (tags/devices)│
                                          │   - Upload seam (future)      │
                                          └───────────────┬───────────────┘
                                                          │ rs-matter
                                                          ▼
                                              Matter devices (UDP/mDNS/BDX)
```

### Core ↔ UI boundary: my recommendation

**Recommendation: a local RPC server (WebSocket + JSON) hosted by the native
core, with the web UI as a plain browser client.** Rationale:

1. **The web UI runs unmodified in a normal browser on your desktop** — no
   Tauri shell, no Wasm socket gymnastics. Fastest iteration loop.
2. **The RPC contract becomes the canonical core API spec.** The UniFFI
   surface for mobile is then a near-mechanical mirror of the same
   request/response/event vocabulary (commands + a streamed event channel).
   We design the API *once*, in terms the UI actually needs.
3. **Events/streaming fall out naturally.** Commissioning progress, BDX
   download progress, subscription reports, and device-state changes are all
   server→UI push events over the same WS — which maps cleanly to UniFFI
   callback interfaces / async streams on mobile.
4. **Language-agnostic UI choice.** Yew (Rust→Wasm) *or* a plain TS/React
   frontend can consume the same WS API. We can start with whichever is
   fastest and not be locked in. (Yew is fine here precisely because it only
   speaks WS/JSON — it never needs raw sockets.)

Trade-off vs. Tauri: Tauri would let the Yew UI call Rust via `invoke` with no
network hop, but the boundary would be Tauri-specific and would *not* mirror
the mobile UniFFI surface — defeating the lock-step goal. The WS/JSON contract
is the more reusable artifact. (If we later want a single-binary desktop app,
we can still wrap the same core + a bundled web UI in Tauri/Wry without
changing the contract.)

> Net: **one core, two transports** — WS/JSON for web/desktop, UniFFI for
> mobile — both projections of the same internal `Controller` API.

---

## 4. What `rs-matter` already gives us (verified in-tree)

The controller side is **already present** — we are wrapping, not building:

| Need | rs-matter API | Location |
|---|---|---|
| Commission (PASE → ArmFailSafe → CSR → AddNOC) | `Commissioner::commission()` | `onboard.rs` |
| Finalize over CASE | `Commissioner::complete_via_case()` | `onboard.rs` |
| Decommission / fabric cleanup | `RemoveFabric` (OperationalCredentials) | `dm/clusters` |
| IM client (Read/Write/Invoke/Subscribe) | `im::client` (`ImClient` ext trait on `Exchange`) | `im/client.rs` |
| Per-cluster typed clients | codegen `*Client` traits | `dm/clusters/*` |
| Diagnostic clusters | `gen_diag`, `sw_diag`, `thread_diag`, `wifi_diag`, `eth_diag` | `dm/clusters/*` |
| Diagnostic logs receive over BDX | `diag_logs::client::{DiagLogsBdxHandler, DiagLogsReceiver}` + `bdx::BdxReader` | `dm/clusters/diag_logs/`, `bdx.rs` |
| Desktop crypto | `rustcrypto` / `openssl` backend | `crypto/backend/*` |

Reference flows live in `tests/commissioning.rs`, `tests/case.rs`,
`tests/diag_logs.rs`, `tests/bdx.rs` — these are our implementation templates.

**Open questions to resolve during the spike** (not blockers, but unknowns):
- mDNS browse/resolve for commissionable + operational discovery from the
  controller side on desktop (Linux): confirm the discovery path and how it
  interacts with host avahi (cf. prior hub-flakiness notes).
- Subscription *post-establishment* report listening — `im/client.rs` notes
  that establishment is covered but server-initiated reports need a listener
  layered on top. We need that for live health dashboards (vs. one-shot reads).
- App-to-app commissioning payload ingestion shape (mobile only; desktop uses
  manual code / QR for now).

---

## 5. Fabric lifetime as a per-session policy

The core exposes fabric lifetime as a **policy**, not a baked-in mode:

```rust
enum FabricPolicy {
    /// Commission, keep the NOC on our fabric, persist device identity
    /// locally. Repeat access is a cheap CASE session.
    Persistent,
    /// Commission on a throwaway fabric, do the work, then RemoveFabric
    /// (self-destruct). No local identity persisted.
    Ephemeral,
}
```

- **Persistent** path needs: local device store (NodeID, serial/UniqueID,
  user tags), Identify-cluster blink for physical disambiguation, and
  **re-sync handling**:
  - *Device forgot us* (factory reset): CASE fails → flag offline → prompt
    re-share → on re-commission, match by `BasicInformation` serial/UniqueID
    and overwrite the stale record (no duplicate).
  - *We forgot device* (DB wiped / reinstall): before `AddNOC`, read the
    `Fabrics` list over the PASE session and `RemoveFabric` any fabric whose
    root public key matches ours (orphan cleanup) — prevents fabric-slot leak.
- **Ephemeral** path needs none of the above but accepts the "too broken to
  pair" risk and ~10–30 s pairing latency per fetch.

Both share the same commission/diagnostic/log-fetch primitives; only the
before/after lifecycle differs.

---

## 6. Proposed crate / workspace layout

```
matter-peek/                         (cargo workspace)
├── Cargo.toml
├── DESIGN.md                        (this file)
├── core/                            # matter-peek-core — the reusable lib
│   ├── controller/                  #   commission, CASE, decommission
│   ├── diagnostics/                 #   typed reads of each diag cluster
│   ├── logs/                        #   DiagnosticLogs + BDX receiver
│   ├── store/                       #   device/tag persistence (sqlx/rusqlite)
│   ├── model/                       #   UI-facing DTOs (serde) — the contract
│   └── api/                         #   Controller facade (the single API)
├── rpc/                             # WS/HTTP+JSON server over core::api
├── uniffi/                          # UniFFI wrapper over core::api (later)
├── cli/                             # thin CLI for spikes / headless testing
├── web/                             # web UI (Yew or TS) — WS/JSON client
└── (ios/, android/  — later)
```

- `core::model` (serde DTOs) + `core::api` (the `Controller` facade) are the
  **single source of truth**. `rpc`, `uniffi`, and `cli` are all thin
  adapters over `core::api`. This is what keeps web and mobile in lock-step.
- `rs-matter` is consumed by `core` only. UI layers never see `rs-matter`
  types — they see `core::model` DTOs.

---

## 7. The core API (sketch — drives both transports)

```rust
// Commands (UI → core)
trait Controller {
    async fn discover(&self, filter: DiscoveryFilter) -> Vec<DiscoveredDevice>;
    async fn commission(&self, payload: SetupPayload, policy: FabricPolicy)
        -> Result<DeviceHandle>;
    async fn decommission(&self, dev: DeviceHandle) -> Result<()>;

    async fn read_health(&self, dev: DeviceHandle, which: DiagSet) -> DeviceHealth;
    async fn subscribe_health(&self, dev: DeviceHandle, which: DiagSet) -> SubId;

    async fn retrieve_logs(&self, dev: DeviceHandle, intent: LogIntent)
        -> Result<LogTransfer>;   // progress streamed as events

    async fn identify(&self, dev: DeviceHandle, secs: u16) -> Result<()>; // blink
    // tags / store
    async fn set_tags(&self, dev: DeviceHandle, tags: Vec<String>);
    async fn list_devices(&self, filter: TagFilter) -> Vec<StoredDevice>;
}

// Events (core → UI), streamed over WS / UniFFI callbacks
enum Event {
    CommissionProgress { dev: DeviceHandle, step: CommStep, pct: u8 },
    LogProgress { transfer: TransferId, received: u64, total: Option<u64> },
    HealthReport { dev: DeviceHandle, report: DeviceHealth },
    DeviceState { dev: DeviceHandle, state: Reachability },
}
```

The DTOs (`DeviceHealth`, `DiagSet`, `LogIntent`, …) live in `core::model`
and are the same on every transport.

---

## 8. Phased plan

**Phase 0 — Vertical spike (validate the core primitives).**
A headless Rust CLI (`cli/`) against a real or simulated device:
discover → commission (ephemeral) → read `sw_diag` + `gen_diag` → retrieve
`CrashLogs` over BDX → `RemoveFabric`. Proves every hard primitive end-to-end
on the dev desktop before any API/UI design hardens. Templates:
`tests/commissioning.rs`, `tests/diag_logs.rs`.

**Phase 1 — Core API + RPC + minimal web UI (lock-step).**
Define `core::model` + `core::api`; expose via `rpc/` (WS/JSON); build a
minimal web UI that drives the spike's flow through the UI. Iterate the API
shape against real UI needs. Add `Persistent` policy + local store + tags +
Identify-blink + re-sync handling.

**Phase 2 — Health dashboard + subscriptions.**
Live diagnostics via subscription report listening; tag-based filtering UI;
uptime / crash-count stats computed in core.

**Phase 3 — Mobile via UniFFI.**
`uniffi/` over the same `core::api`; SwiftUI + Jetpack Compose UIs;
Google Home Mobile SDK / Apple MatterSupport app-to-app handoff for
frictionless commissioning.

**Phase 4 — Consent-gated cloud upload.**
Implement the `core` upload seam: push fetched logs/core-dumps to a vendor
endpoint over HTTPS with explicit user consent. Pluggable per-vendor crash
parsers.

---

## 9. Key risks / things to watch
- **mDNS on Linux desktop** colliding with host avahi (see prior project
  notes on hub flakiness) — validate discovery early in Phase 0.
- **Subscription post-establishment reports** need a listener abstraction on
  top of `im::client` — confirm feasibility before promising a live dashboard.
- **"Too broken to pair"** devices under the ephemeral model — document the
  limitation; recommend persistent policy for flaky hardware.
- **Fabric-slot exhaustion** — orphan cleanup before `AddNOC` is mandatory in
  the persistent path.
- **UniFFI async** bridging for long-running commission/BDX flows — proven by
  Mozilla but worth a spike before Phase 3.

---

## 10. Immediate next step
Per decision: this document first. On approval, proceed to **Phase 0** (the
vertical spike) to de-risk the core primitives before committing the API shape.
```
```
