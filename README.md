# matter-peek

A vendor-agnostic **Matter mini commissioner / controller** for installers and
technicians: commission into one or more Matter devices, inspect their health
via the standard diagnostic clusters, and pull **Diagnostic Logs** (crash dumps
/ logs) over BDX.

Built as a native Rust core over [`rs-matter`](https://github.com/project-chip/rs-matter),
with a web UI for desktop validation and (later) native mobile UIs via UniFFI.

See [`DESIGN.md`](DESIGN.md) for the full architecture and roadmap.

## Status

**Phase 0 — vertical spike.** A headless CLI commissions an externally-running
Matter accessory end-to-end. Diagnostics + log retrieval land next.

## Layout

```
core/   matter-peek-core — the reusable controller library (wraps rs-matter)
cli/    headless CLI for spikes / testing (binary: matter-peek)
rpc/    WS/JSON server over core::api            (Phase 1)
uniffi/ mobile bindings over core::api           (Phase 3)
web/    web UI — WS/JSON client                   (Phase 1)
```

## Build & run the spike

```sh
cargo build
# Against an rs-matter example device / chip-all-clusters-app on the defaults:
cargo run -p matter-peek-cli            # passcode 20202021, peer [::1]:5540
cargo run -p matter-peek-cli -- 20202021 '[::1]:5540'
```

For local co-development against working clones, uncomment the `[patch.crates-io]`
line in the workspace `Cargo.toml` to point `rs-matter` at `../rs-matter/rs-matter`.

## License

Proprietary — all rights reserved. Not open source (at least not yet).
