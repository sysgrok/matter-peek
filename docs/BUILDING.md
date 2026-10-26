# Building matter-peek

## Layout

```
matter-peek-core/   the API (`MatterPeek`, its types, wire messages and MCP
                    tools) and the controller behind it (wraps rs-matter); also
                    the UniFFI bindings
matter-peek-server/ the server: a controller and its assistant for the
                    connections to them (`Hub`), and the API over WebSocket/JSON
                    with the web UI, on one port
matter-peek/        the executable, `matter-peek`: the server (`serve`), the
                    command line, the MCP server for AI agents and the terminal
                    chat
matter-peek-web/    the Yew/Wasm web UI (a WebSocket client; a separate wasm
                    crate, built with trunk and taken into the server)
matter-peek-app/    the web UI as a desktop and Android app (Tauri), on the
                    controller in the app or on a server
```

Everything goes through the API. The web UI talks to the server that serves
it; the CLI (also as the MCP server) does too with `--server`, else it runs a
controller in its own process. Only one process at a time can hold the store.
[DESIGN.md](../DESIGN.md) has the architecture.

## Requirements

- Rust 1.91 or newer for the workspace; nightly with the `wasm32-unknown-unknown`
  target, and [trunk](https://trunkrs.dev), for the web UI
  (`matter-peek-web/rust-toolchain.toml` picks nightly).
- Linux, with Avahi for mDNS and BlueZ for Bluetooth commissioning (both over
  D-Bus, so nothing to link against).
- For the app on Linux: `sudo apt install libwebkit2gtk-4.1-dev libdbus-1-dev`
  (Windows' WebView2 and macOS' WKWebView come with the OS).

## One executable

`matter-peek` is all of it: the server with its web UI (`matter-peek serve`),
the command line, the MCP server and the terminal chat. Built with the web UI
inside it, it is the only file to install:

```sh
# The web UI, small (built for size, through wasm-opt), for it to take in
( cd matter-peek-web && trunk build --release )

# The executable, with the web UI inside (each file gzipped)
cargo build --profile dist -p matter-peek            # target/dist/matter-peek

# Static (musl): no shared libraries at all. ring's bit of C wants a C
# compiler for the target (see .cargo/config.toml): clang for x86_64, the
# aarch64 cross gcc (gcc-aarch64-linux-gnu) for a Raspberry Pi
cargo build --profile dist --target x86_64-unknown-linux-musl -p matter-peek
cargo build --profile dist --target aarch64-unknown-linux-musl -p matter-peek
```

The `dist` profile is `release` with LTO, one codegen unit, `opt-level =
"s"` and no symbols: a static x86_64 executable of about 11 MB, against 17 MB
as `release`. `MATTER_PEEK_WEB_DIST` takes the web UI from elsewhere than
`matter-peek-web/dist`.

`cargo install --git https://github.com/sysgrok/matter-peek matter-peek` builds
it too, with the copy of the web UI kept in git (`matter-peek-server/web-dist`,
see [Packaging](#packaging)).

## Run from the tree

```sh
# An accessory to try it on (here, rs-matter's system_tests with a crash log)
( cd ../rs-matter && cargo run -p rs-matter-tests --bin system_tests -- --crash_log /tmp/crash.log )

# The server: open http://127.0.0.1:8087/ (the API is at ws://127.0.0.1:8087/ws)
cargo run --release --bin matter-peek -- serve [--listen 127.0.0.1:8087]

# While working on the web UI: trunk rebuilds it on each change and passes its
# API on to the server (open http://127.0.0.1:8080/)
( cd matter-peek-web && trunk serve )

# The CLI: through the server, or on its own while no server runs
cargo run --bin matter-peek -- --server ws://127.0.0.1:8087 discover
cargo run --bin matter-peek -- --help
```

## The app

`matter-peek-app` is the web UI as a desktop or Android app (Tauri). It runs on
a controller of its own, in the app, or on a server (`192.168.1.10:8087`, …):
asked at first launch, changed from the top bar. On Android, it has no
Bluetooth commissioning yet.

```sh
# The desktop app, with the web UI built first (see above)
cargo install --path matter-peek-app      # or: (cd matter-peek-app && cargo tauri build)

# Android: the SDK with NDK 27 and a JDK (ANDROID_HOME, NDK_HOME, JAVA_HOME set)
(cd matter-peek-app && cargo tauri android build --apk --target aarch64)
```

`matter-peek-server` embeds the web UI, for the server and for the app's
window (`matter-peek://localhost/`).

## Packaging

Neither `cargo install --git` nor a packaged `matter-peek-server` has a built
`matter-peek-web` next to it, so a copy of the built web UI is kept in git, in
`matter-peek-server/web-dist`, and goes into the package. Run
`scripts/vendor-web.sh` to refresh it whenever the web UI changes, and before
`cargo package` / `cargo publish`.

## Mobile bindings

`core` exports `MatterPeek` over [UniFFI](https://mozilla.github.io/uniffi-rs/)
with its `uniffi` feature. Build it as a library, then generate the bindings:

```sh
cargo rustc -p matter-peek-core --lib --features uniffi --crate-type cdylib
cargo run -p matter-peek-core --features uniffi-cli --bin uniffi-bindgen -- \
    generate --library target/debug/libmatter_peek_core.so --language kotlin --out-dir out
# --language swift for iOS / macOS
```

## Working on rs-matter alongside

The workspace `Cargo.toml` patches `rs-matter` and `rs-matter-codegen` to
rs-matter's git `main` until a release has what matter-peek uses. To work
against a local clone instead, point those `[patch.crates-io]` entries at
`../rs-matter/rs-matter` and `../rs-matter/rs-matter-codegen`.
