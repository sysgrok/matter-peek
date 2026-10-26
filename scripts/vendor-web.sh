#!/bin/sh
# Build the web UI and give `matter-peek-server`, which embeds it (for the
# server and the app's window), a copy of it, kept in git: `cargo install
# --git` and a packaged crate have no built ../matter-peek-web/dist. Run it
# whenever the web UI changes.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)

(cd "$root/matter-peek-web" && trunk build --release)
rm -rf "$root/matter-peek-server/web-dist"
cp -r "$root/matter-peek-web/dist" "$root/matter-peek-server/web-dist"
