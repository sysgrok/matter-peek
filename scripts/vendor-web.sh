#!/bin/sh
# Build the web UI and give a copy to each crate that embeds it, for
# packaging (`cargo package` / `cargo publish`): `matter-peek` (the server)
# and `matter-peek-app` (the app). A packaged crate has no ../matter-peek-web.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)

(cd "$root/matter-peek-web" && trunk build --release)
for crate in matter-peek matter-peek-app; do
    rm -rf "$root/$crate/web-dist"
    cp -r "$root/matter-peek-web/dist" "$root/$crate/web-dist"
done
