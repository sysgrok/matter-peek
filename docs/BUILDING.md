# Building matter-peek

## Layout

```
matter-peek-core/   the API (`MatterPeek`, its types, the wire messages and the
                    MCP tools) and the controller behind it, which wraps
                    rs-matter; also the UniFFI bindings
matter-peek-server/ the server: a controller and its assistant, shared by the
                    connections to them (`Hub`), and the API over
                    WebSocket/JSON together with the web UI, on one port
matter-peek/        the `matter-peek` executable: the server (`serve`), the
                    command line, the MCP server for AI agents and the terminal
                    chat
matter-peek-web/    the web UI, in Yew/Wasm: a WebSocket client, in a separate
                    wasm crate, built with trunk and embedded in the server
matter-peek-app/    the web UI as a desktop and Android app (Tauri), using the
                    controller in the app or a server
```

Everything goes through the API. The web UI talks to the server that serves
it. The CLI, also when it runs as the MCP server, does the same when it is
given `--server`; otherwise it runs a controller in its own process. Only one
process at a time can hold the store. [DESIGN.md](DESIGN.md) describes the
architecture.

## Requirements

- Rust 1.91 or newer for the workspace, and a C toolchain for linking (for
  example `build-essential`).
- For the web UI: nightly Rust with the `wasm32-unknown-unknown` target, and
  [trunk](https://trunkrs.dev). `matter-peek-web/rust-toolchain.toml` selects
  nightly.
- Linux, with Avahi for mDNS and BlueZ for Bluetooth commissioning. Both are
  used over D-Bus, so there is nothing to link against.
- For the app on Linux:
  `sudo apt install pkg-config libwebkit2gtk-4.1-dev libdbus-1-dev`. On Windows
  and macOS, the web view (WebView2 and WKWebView) comes with the OS.

## One executable

The `matter-peek` executable contains everything: the server with its web UI
(`matter-peek serve`), the command line, the MCP server and the terminal chat.
When it is built with the web UI inside, it is the only file that needs to be
installed:

```sh
# Build the web UI, optimized for size (through wasm-opt), so that the executable can embed it
( cd matter-peek-web && trunk build --release )

# Build the executable, with the web UI inside (each file gzipped)
cargo build --profile dist -p matter-peek            # target/dist/matter-peek

# Build a static executable (musl), which needs no shared libraries at all.
# ring contains some C code, so a C compiler for the target is needed (see
# .cargo/config.toml): clang for x86_64, and the aarch64 cross gcc
# (gcc-aarch64-linux-gnu) for a Raspberry Pi
cargo build --profile dist --target x86_64-unknown-linux-musl -p matter-peek
cargo build --profile dist --target aarch64-unknown-linux-musl -p matter-peek
```

The `dist` profile is the `release` profile with LTO, one codegen unit,
`opt-level = "s"` and no symbols. It produces a static x86_64 executable of
about 13 MB, compared to 20 MB with `release`. `MATTER_PEEK_WEB_DIST` can point
to a web UI in another place than `matter-peek-web/dist`.

`cargo install --git https://github.com/sysgrok/matter-peek matter-peek` also
builds it. It then uses the copy of the web UI that is kept in git, in
`matter-peek-server/web-dist` (see [Packaging](#packaging)).

## Running from the source tree

```sh
# A device to try it with: here, rs-matter's system_tests with a crash log
( cd ../rs-matter && cargo run -p rs-matter-tests --bin system_tests -- --crash_log /tmp/crash.log )

# The server: open http://127.0.0.1:8087/ (the API is at ws://127.0.0.1:8087/ws)
cargo run --release --bin matter-peek -- serve [--listen 127.0.0.1:8087]

# While working on the web UI: trunk rebuilds it on each change, and forwards
# the API requests to the server (open http://127.0.0.1:8080/)
( cd matter-peek-web && trunk serve )

# The CLI: through the server, or on its own when no server is running
cargo run --bin matter-peek -- --server ws://127.0.0.1:8087 discover
cargo run --bin matter-peek -- --help
```

## The app

`matter-peek-app` is the web UI as a desktop or Android app, built with Tauri.
It either runs its own controller inside the app, or connects to a server (for
example `192.168.1.10:8087`). This is asked at the first launch, and can be
changed later from the top bar. On Android, the app does not support Bluetooth
commissioning yet.

```sh
# The desktop app (build the web UI first, as above)
cargo install --path matter-peek-app      # or: (cd matter-peek-app && cargo tauri build)

# Android: needs the SDK with NDK 27 and a JDK, with ANDROID_HOME, NDK_HOME and JAVA_HOME set
(cd matter-peek-app && cargo tauri android build --apk --target aarch64)
```

`matter-peek-server` embeds the web UI, both for the server and for the app's
window (`matter-peek://localhost/`).

## Packaging

Neither `cargo install --git` nor a packaged `matter-peek-server` has a built
`matter-peek-web` next to it. For this reason, a copy of the built web UI is
kept in git, in `matter-peek-server/web-dist`, and it is included in the
package. Run `scripts/vendor-web.sh` to refresh this copy whenever the web UI
changes, and before `cargo package` or `cargo publish`.

## Mobile bindings

`matter-peek-core` exports `MatterPeek` over
[UniFFI](https://mozilla.github.io/uniffi-rs/) when its `uniffi` feature is
enabled. Build it as a library, then generate the bindings:

```sh
cargo rustc -p matter-peek-core --lib --features uniffi --crate-type cdylib
cargo run -p matter-peek-core --features uniffi-cli --bin uniffi-bindgen -- \
    generate --library target/debug/libmatter_peek_core.so --language kotlin --out-dir out
# use --language swift for iOS and macOS
```

## Working on rs-matter at the same time

Until an rs-matter release has everything matter-peek uses, the workspace
`Cargo.toml` patches `rs-matter` and `rs-matter-codegen` to rs-matter's git
`main`. To work against a local clone instead, point those `[patch.crates-io]`
entries at `../rs-matter/rs-matter` and `../rs-matter/rs-matter-codegen`.
