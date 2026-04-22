#!/usr/bin/env bash
# Show passive-standby readiness for the whale-pair deployment.

set -euo pipefail

REPO_ROOT="${REPO_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}"
DATA_DIR="${DATA_DIR:-$REPO_ROOT/data}"
BACKUP_DIR="${BACKUP_DIR:-$DATA_DIR/backups}"
ROLE_FILE="${ROLE_FILE:-$DATA_DIR/whale_pair_standby.role}"
LEDGER_PATH="${LEDGER_PATH:-$DATA_DIR/whale_pair_live.db}"
COMPOSE_FILE="$REPO_ROOT/scripts/deploy/whale-pair/docker-compose.whale-pair.yml"

if [ ! -f "$ROLE_FILE" ]; then
    echo "STANDBY NOT INITIALIZED: missing role file $ROLE_FILE" >&2
    exit 1
fi

echo "=== Whale-Pair Standby Status ==="
cat "$ROLE_FILE"
echo ""

if [ -f "$LEDGER_PATH" ]; then
    LEDGER_SIZE="$(wc -c <"$LEDGER_PATH" | tr -d ' ')"
    if [ "$(uname)" = "Linux" ]; then
        LEDGER_MTIME="$(stat -c '%y' "$LEDGER_PATH")"
    else
        LEDGER_MTIME="$(stat -f '%Sm' -t '%Y-%m-%dT%H:%M:%S%z' "$LEDGER_PATH")"
    fi
    echo "ledger_path=$LEDGER_PATH"
    echo "ledger_size_bytes=$LEDGER_SIZE"
    echo "ledger_mtime=$LEDGER_MTIME"
else
    echo "ledger_path=$LEDGER_PATH"
    echo "ledger_status=missing"
fi
echo ""

LATEST_BACKUP="$(find "$BACKUP_DIR" -maxdepth 1 -type f -name 'whale-pair-data-*.tar.gz' 2>/dev/null | sort | tail -1 || true)"
if [ -n "$LATEST_BACKUP" ]; then
    echo "latest_backup=$LATEST_BACKUP"
else
    echo "latest_backup=none"
fi
echo ""

if command -v docker >/dev/null 2>&1; then
    docker compose -f "$COMPOSE_FILE" ps || true
else
    echo "docker_status=unavailable"
fi
