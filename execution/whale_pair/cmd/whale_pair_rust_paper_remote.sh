#!/usr/bin/env bash
# Run the Rust paper variants launcher on a remote host.
#
# Usage:
#   scripts/whale_pair/cmd/whale_pair_rust_paper_remote.sh start [variant]
#   scripts/whale_pair/cmd/whale_pair_rust_paper_remote.sh stop [variant]
#   scripts/whale_pair/cmd/whale_pair_rust_paper_remote.sh status [variant]
#
# Set one of these in your environment:
#   WHALE_PAIR_PAPER_HOST=<host-or-alias>
#   WHALE_PAIR_PAPER_REMOTE_DIR=/opt/polymarket-agent

set -euo pipefail

MODE="${1:-}"
TARGET="${2:-}"

if [ -z "$MODE" ]; then
    echo "Usage: $0 start|stop|status [variant]" >&2
    exit 64
fi

if [ -z "${WHALE_PAIR_PAPER_HOST:-}" ]; then
    echo "ERROR: WHALE_PAIR_PAPER_HOST is required (set host or SSH alias)." >&2
    exit 2
fi

REMOTE_HOST="${WHALE_PAIR_PAPER_HOST}"
REMOTE_USER="${WHALE_PAIR_PAPER_REMOTE_USER:-ubuntu}"
REMOTE_DIR="${WHALE_PAIR_PAPER_REMOTE_DIR:-/opt/polymarket-agent}"
SSH_KEY="${WHALE_PAIR_PAPER_SSH_KEY:-}"
EXTRA_SSH_OPTS="${WHALE_PAIR_PAPER_SSH_OPTS:---BatchMode=yes -o ServerAliveInterval=15 -o ServerAliveCountMax=3}"

if [[ "$REMOTE_HOST" != *"@"* ]]; then
    REMOTE_HOST="${REMOTE_USER}@${REMOTE_HOST}"
fi

IFS=' ' read -r -a ssh_opts <<< "$EXTRA_SSH_OPTS"
if [ -n "$SSH_KEY" ]; then
    ssh_opts+=("-i" "$SSH_KEY")
fi

remote_cmd=(
    "bash"
    "scripts/whale_pair/cmd/whale_pair_rust_paper_variants.sh"
    "$MODE"
)
if [ -n "$TARGET" ]; then
    remote_cmd+=("$TARGET")
fi

remote_cmd_string="${remote_cmd[*]}"
ssh "${ssh_opts[@]}" "$REMOTE_HOST" "cd '$REMOTE_DIR' && ${remote_cmd_string}"
