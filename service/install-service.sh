#!/bin/sh
# Install `matter-peek serve` as a systemd service, or remove it again.
#
#   sudo ./install-service.sh [--listen ADDR] [--bin PATH]
#   sudo ./install-service.sh --uninstall
#
# The service runs as the system user `matter-peek`, which this creates, with
# its store and its access token in /var/lib/matter-peek. It listens on the
# whole network, at 0.0.0.0:17335, unless told otherwise with --listen (for
# example 127.0.0.1:17335 for this machine alone). The executable is the one
# given with --bin, else the `matter-peek` next to this script (as in the
# downloaded archive), else the one on the PATH; it is copied to
# /usr/local/bin. Running this again updates the executable and the service.
set -eu

name=matter-peek
bin=/usr/local/bin/matter-peek
unit=/etc/systemd/system/matter-peek.service
store=/var/lib/matter-peek
listen=0.0.0.0:17335
src=
uninstall=false

usage() {
    sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//'
}

while [ $# -gt 0 ]; do
    case "$1" in
        --listen | --bin)
            if [ $# -lt 2 ]; then
                echo "$1 needs a value" >&2
                exit 2
            fi
            if [ "$1" = --listen ]; then listen=$2; else src=$2; fi
            shift 2
            ;;
        --uninstall)
            uninstall=true
            shift
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            echo "unknown option: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

if [ "$(id -u)" -ne 0 ]; then
    echo "run it as root, for example with sudo" >&2
    exit 1
fi

if $uninstall; then
    systemctl disable --now "$name" 2>/dev/null || true
    rm -f "$unit" "$bin"
    systemctl daemon-reload
    echo "The service is removed. The user $name and the store in $store stay:"
    echo "the store holds the keys of our fabrics, and our devices can only be"
    echo "factory reset without them. Delete it yourself if that is what you want."
    exit 0
fi

if [ -z "$src" ]; then
    here=$(cd "$(dirname "$0")" && pwd)
    if [ -x "$here/matter-peek" ]; then
        src=$here/matter-peek
    else
        src=$(command -v matter-peek || true)
    fi
fi
if [ -z "$src" ] || [ ! -x "$src" ]; then
    echo "no matter-peek executable next to this script or on the PATH: give it with --bin" >&2
    exit 1
fi

# The user the service runs as, without a login. BlueZ lets the `bluetooth`
# group commission over Bluetooth, where there is one.
if ! id "$name" >/dev/null 2>&1; then
    useradd --system --home-dir "$store" --no-create-home \
        --shell "$(command -v nologin || echo /bin/false)" "$name"
fi
if getent group bluetooth >/dev/null; then
    usermod -a -G bluetooth "$name"
fi

# A running service would keep the old executable busy.
systemctl stop "$name" 2>/dev/null || true
if [ "$src" != "$bin" ]; then
    install -m 0755 "$src" "$bin"
fi

cat >"$unit" <<EOF
[Unit]
Description=matter-peek, health and diagnostics for Matter devices
Documentation=https://github.com/sysgrok/matter-peek
Wants=network-online.target
After=network-online.target avahi-daemon.service bluetooth.service

[Service]
User=$name
ExecStart=$bin --store-dir $store serve --listen $listen
# $store, for the user alone: the store holds the keys of our fabrics.
StateDirectory=$name
StateDirectoryMode=0700
Restart=on-failure
RestartSec=5
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable --now "$name"

# The server makes its access token at its first start.
token=
for _ in $(seq 1 30); do
    token=$(cat "$store/access-token" 2>/dev/null || true)
    [ -n "$token" ] && break
    sleep 1
done

port=${listen##*:}
host=${listen%:*}
case "$host" in
    0.0.0.0 | "[::]") host=$(hostname -I 2>/dev/null | awk '{print $1}') ;;
esac

echo "matter-peek runs as a service now."
if [ -n "$token" ]; then
    echo "Open it at: http://${host:-localhost}:$port/?token=$token"
    echo "Its access token is also in $store/access-token."
else
    echo "It has not made its access token yet: see its log."
fi
echo "Its log: journalctl -u $name -f"
if ! systemctl is-active --quiet avahi-daemon; then
    echo "Avahi does not run: matter-peek needs it to find devices (sudo apt install avahi-daemon)." >&2
fi
case "$listen" in
    127.* | localhost:* | "[::1]":*) ;;
    *) echo "There is no TLS yet: the access token and the rest travel unencrypted on the network." ;;
esac
