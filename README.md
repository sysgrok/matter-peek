# matter-peek

A vendor-agnostic **Matter mini commissioner / controller** for installers and
technicians: commission into one or more Matter devices, inspect their health
via the standard diagnostic clusters, and pull **Diagnostic Logs** (crash dumps
/ logs) over BDX.

Built as a native Rust core over [`rs-matter`](https://github.com/project-chip/rs-matter),
with a web UI for desktop validation and (later) native mobile UIs via UniFFI.

See [`DESIGN.md`](DESIGN.md) for the full architecture and roadmap.

## Status

A local Matter controller for diagnostics: a web UI listing the devices on our
fabric (live health, logs, OTA updates) and commissioning new ones (with mDNS
discovery), over a WS/JSON RPC server; an MCP server for AI agents; and a
headless CLI.

## Layout

```
core/   matter-peek-core — the reusable controller library (wraps rs-matter)
cli/    headless CLI for spikes / testing (binary: matter-peek)
proto/  matter-peek-proto — wire contract (serde) shared by rpc + UIs
rpc/    matter-peek-rpc — local WS/JSON server over the core (binary)
web/    matter-peek-web — Yew/Wasm web UI (WS/JSON client; separate wasm crate)
uniffi/ mobile bindings over the core           (Phase 3)
```

## Run the web stack (Phase 1)

Three processes: a Matter accessory, the RPC server, and the web UI.

```sh
# 1. An accessory (here, rs-matter's system_tests with a crash log)
( cd ../rs-matter && cargo run --bin system_tests -- --crash_log /tmp/crash.log )

# 2. The RPC server (native; default ws://127.0.0.1:8087)
cargo run -p matter-peek-rpc

# 3. The web UI (needs `trunk` + the wasm32 target on nightly)
( cd web && trunk serve )   # then open the printed http://… URL
```

Headless smoke test of the full server↔core↔device pipe (no browser):

```sh
cargo run -p matter-peek-rpc --example ws_client
```

## Build & run the spike

```sh
cargo build
# Against an rs-matter example device / chip-all-clusters-app on the defaults:
cargo run -p matter-peek-cli            # passcode 20202021, peer [::1]:5540
cargo run -p matter-peek-cli -- 20202021 '[::1]:5540'
# Any setup code works: a passcode, a manual pairing code or an MT: QR payload.
cargo run -p matter-peek-cli -- 3497-011-2332
```

For local co-development against working clones, uncomment the `[patch.crates-io]`
line in the workspace `Cargo.toml` to point `rs-matter` at `../rs-matter/rs-matter`.

## License

Proprietary — all rights reserved. Not open source (at least not yet).
