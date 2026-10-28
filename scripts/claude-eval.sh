#!/bin/sh
# matter-peek's MCP tools, driven by Claude Code on your Claude subscription,
# with nothing else: no auto-memory, no CLAUDE.md, no user settings, none of
# Claude Code's own tools. What a strong model makes of the tools alone.
#
#   scripts/claude-eval.sh                          # the read-only tools (and identify)
#   scripts/claude-eval.sh --allow-mutate           # also the ones that change things
#   scripts/claude-eval.sh --server ws://127.0.0.1:17335  # through a running server
#   scripts/claude-eval.sh -- -p "Which devices are offline?"   # more Claude Code flags
#
# Claude Code asks before each tool call, as usual.

set -eu

repo=$(cd "$(dirname "$0")/.." && pwd)
peek="$repo/target/debug/matter-peek"

server=""
mutate=""
while [ $# -gt 0 ]; do
    case "$1" in
        --allow-mutate) mutate=', "--allow-mutate"' ;;
        --server) server="\"--server\", \"$2\", "; shift ;;
        --) shift; break ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
    shift
done

[ -x "$peek" ] || { echo "build it first: cargo build --bin matter-peek" >&2; exit 1; }

dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT

cat > "$dir/mcp.json" <<EOF
{ "mcpServers": { "matter-peek": { "command": "$peek", "args": [ $server"mcp"$mutate ] } } }
EOF

cd "$dir"
CLAUDE_CODE_DISABLE_AUTO_MEMORY=1 claude \
    --settings '{"autoMemoryEnabled": false}' \
    --setting-sources project,local \
    --tools "" \
    --strict-mcp-config --mcp-config "$dir/mcp.json" \
    --append-system-prompt "Here you are not a coding assistant: you are the \
assistant of a user of matter-peek, a local Matter smart-home controller, and \
you inspect, diagnose and repair their devices only through the matter-peek \
tools. Answer in plain words for a non-technical person." \
    "$@"
