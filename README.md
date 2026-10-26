# matter-peek

**The Matter health & diagnostics sidekick your controller never came with.**

[![CI](https://github.com/sysgrok/matter-peek/actions/workflows/ci.yml/badge.svg)](https://github.com/sysgrok/matter-peek/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](#license)

Google Home, Apple Home and Alexa tell you that a device is *not responding*. **matter-peek tells you why.**

It joins your Matter devices as one more admin, next to the controller you already use, and shows you what they know about themselves: uptime and reboots, Wi-Fi signal and Thread mesh links, free heap and stack, crash logs.

It commissions devices, opens up their whole data model and updates their firmware. When you'd rather ask than click, an AI assistant digs through all of it for you.

It runs on your LAN and needs no account. It is one self-contained executable, written in Rust on [rs-matter](https://github.com/project-chip/rs-matter).

<!-- TODO: a screenshot or a short screen recording of the web UI -->

## Highlights

- **Live health and crash logs.** Uptime, reboots, network faults, heap and stacks, followed live; crash and diagnostic logs pulled over BDX.
- **Commission and share.** Over IP or Bluetooth LE, onto Wi-Fi or Thread. Adopt devices from other ecosystems, or share yours with them.
- **The whole data model.** Every cluster, attribute, command and event, by name and live, with ready-made controls on top.
- **The Thread mesh, from Matter alone.** No Thread credentials needed, with what's wrong in plain words.
- **OTA updates.** matter-peek is your devices' OTA Provider: drop in an image and push it.
- **AI included.** An MCP server for Claude, Cursor & co., and a built-in assistant on any OpenAI-compatible model, local ones included.

And more: [several fabrics, network management, a time source, one filter language](https://github.com/sysgrok/matter-peek/blob/main/docs/GUIDE.md), plus a web UI, a CLI and an app, all on one API.

## Status

matter-peek is fully usable, but not yet released. It runs on Linux today, and on Android through the app. Planned next:

- `cargo install matter-peek` from crates.io: the names are reserved, but the real crates wait for an rs-matter release that has everything matter-peek uses.
- Pre-built binaries for Linux, Windows and macOS.
- Ready-made apps for Android and iPhone.

Until then, build it from GIT, as below.

> **Note:** the server has no authentication yet. Whoever reaches it controls your devices, so keep `matter-peek serve` on localhost (its default) or behind something that guards it.

## Install

matter-peek comes as two executables:

- **`matter-peek`**: the server with its web UI, the CLI, the MCP server and the
  terminal chat, in one file. It is at home on headless boxes too: a Raspberry
  Pi, a NAS, a container.
- **`matter-peek-app`**: the web UI as a desktop app, with a controller of its
  own inside or connected to a `matter-peek` server.

Both build on Linux (x86_64 or aarch64) with Rust 1.91 or newer: the distribution's, or [rustup](https://rustup.rs)'s.

```sh
# The distribution's Rust, which brings the C toolchain Rust links with
sudo apt install cargo                          # Ubuntu 26.04, Debian testing
sudo apt install -t trixie-backports cargo      # Debian 13, Raspberry Pi OS (backports enabled)
sudo apt install cargo-1.91                     # Ubuntu 24.04: then `cargo-1.91` instead of `cargo`

# ... or rustup's
sudo apt install build-essential                # the C toolchain Rust links with
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh    # then open a new shell
```

**`matter-peek`**

```sh
cargo install --locked --git https://github.com/sysgrok/matter-peek matter-peek
```

**`matter-peek-app`**

```sh
sudo apt install pkg-config libwebkit2gtk-4.1-dev libdbus-1-dev
cargo install --locked --git https://github.com/sysgrok/matter-peek matter-peek-app
```

`cargo install` puts them in `~/.cargo/bin`, which rustup adds to `PATH`; with the distribution's Rust, add it yourself. The packages are Debian's, Ubuntu's and Raspberry Pi OS's; other distributions have them under their own names. At run time, both find devices through Avahi and commission over Bluetooth through BlueZ, which most desktops already run (else `sudo apt install avahi-daemon bluez`).

Static and Raspberry Pi builds, and the Android app, are covered in [Building](https://github.com/sysgrok/matter-peek/blob/main/docs/BUILDING.md).

## Quick start

```sh
matter-peek serve        # then open http://127.0.0.1:8087
```

To get your devices in, commission a new one from the **Commission** page, or
adopt one you already have. Have its current controller share it (Google Home:
*Linked Matter apps & services*; Apple Home: *Turn On Pairing Mode*; Home
Assistant: *Share device*), then commission it with the code it shows.

The same from the command line:

```sh
matter-peek discover                         # commissionable devices nearby
matter-peek commission 34970112332 --ble --wifi Home --wifi-password '...'
matter-peek list net:thread is:offline       # the filter language
matter-peek peek 0x1B67B                     # a device's health
matter-peek logs 0x1B67B crash               # its crash log
matter-peek thread findings                  # what is wrong with the mesh
matter-peek --help
```

With a server running, the CLI goes through it: add `--server
ws://127.0.0.1:8087`. Without a server, the CLI runs a controller of its own.

With AI:

```sh
# Claude Code, or any MCP client: read-only unless you add --allow-mutate
claude mcp add matter-peek -- matter-peek --server ws://127.0.0.1:8087 mcp

# The built-in assistant, here on a local model
matter-peek assistant set --url http://localhost:11434/v1 --model qwen3.5:9b
matter-peek chat "Which of my Thread devices have weak links?"
```

## Documentation

- [User guide](https://github.com/sysgrok/matter-peek/blob/main/docs/GUIDE.md):
  devices and filters, fabrics, sharing, time, the Thread mesh, the data model
- [AI](https://github.com/sysgrok/matter-peek/blob/main/docs/AI.md): the MCP
  server and the assistant
- [Building](https://github.com/sysgrok/matter-peek/blob/main/docs/BUILDING.md):
  the crates, static builds, the app on Android, working on the web UI
- [Design](https://github.com/sysgrok/matter-peek/blob/main/DESIGN.md): the
  architecture and the roadmap

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without
any additional terms or conditions.
