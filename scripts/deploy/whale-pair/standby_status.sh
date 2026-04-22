#!/usr/bin/env bash
# Show passive-standby readiness for the whale-pair deployment.

set -euo pipefail

usage() {
    cat >&2 <<'EOF'
Usage: standby_status.sh [--assert-ready] [--skip-health-check]

Options:
  --assert-ready      exit non-zero when standby is not safe to promote
  --skip-health-check skip running health.sh during readiness assertion
EOF
}

REPO_ROOT="${REPO_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}"
DATA_DIR="${DATA_DIR:-$REPO_ROOT/data}"
BACKUP_DIR="${BACKUP_DIR:-$DATA_DIR/backups}"
ROLE_FILE="${ROLE_FILE:-$DATA_DIR/whale_pair_standby.role}"
LEDGER_PATH="${LEDGER_PATH:-$DATA_DIR/whale_pair_live.db}"
RESTORE_META="${RESTORE_META:-$DATA_DIR/whale_pair_restore.meta}"
COMPOSE_FILE="$REPO_ROOT/scripts/deploy/whale-pair/docker-compose.whale-pair.yml"
ASSERT_READY=0
SKIP_HEALTH_CHECK=0

while [ $# -gt 0 ]; do
    case "$1" in
        --assert-ready)
            ASSERT_READY=1
            shift
            ;;
        --skip-health-check)
            SKIP_HEALTH_CHECK=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "ERROR: unknown arg: $1" >&2
            usage
            exit 64
            ;;
    esac
done

ISSUES=()

if [ ! -f "$ROLE_FILE" ]; then
    echo "STANDBY NOT INITIALIZED: missing role file $ROLE_FILE" >&2
    exit 1
fi

echo "=== Whale-Pair Standby Status ==="
cat "$ROLE_FILE"
echo ""

ROLE_VALUE="$(grep -E '^role=' "$ROLE_FILE" | head -1 | cut -d= -f2-)"
if [ "$ROLE_VALUE" != "passive-standby" ]; then
    ISSUES+=("role marker is '$ROLE_VALUE' (expected passive-standby for promotion checks)")
fi

if [ -f "$RESTORE_META" ]; then
    echo "restore_meta=$RESTORE_META"
    cat "$RESTORE_META"
else
    echo "restore_meta=missing"
    ISSUES+=("restore metadata missing ($RESTORE_META)")
fi
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
    ISSUES+=("ledger missing ($LEDGER_PATH)")
fi
echo ""

LATEST_BACKUP="$(find "$BACKUP_DIR" -maxdepth 1 -type f -name 'whale-pair-data-*.tar.gz' 2>/dev/null | sort | tail -1 || true)"
if [ -n "$LATEST_BACKUP" ]; then
    echo "latest_backup=$LATEST_BACKUP"
    RESTORED_ARCHIVE="$(grep -E '^archive_basename=' "$RESTORE_META" 2>/dev/null | head -1 | cut -d= -f2-)"
    if [ -n "$RESTORED_ARCHIVE" ] && [ "$(basename "$LATEST_BACKUP")" != "$RESTORED_ARCHIVE" ]; then
        ISSUES+=("restored archive '$RESTORED_ARCHIVE' does not match latest backup '$(basename "$LATEST_BACKUP")'")
    fi
else
    echo "latest_backup=none"
    ISSUES+=("no backup archive present in $BACKUP_DIR")
fi
echo ""

if command -v docker >/dev/null 2>&1; then
    docker compose -f "$COMPOSE_FILE" ps || true
else
    echo "docker_status=unavailable"
fi

if [ "$ASSERT_READY" -eq 1 ] && [ "$SKIP_HEALTH_CHECK" -ne 1 ]; then
    if ! bash "$REPO_ROOT/scripts/deploy/whale-pair/health.sh" >/tmp/whale-pair-standby-health.$$ 2>&1; then
        ISSUES+=("health.sh failed: $(tr '\n' ' ' </tmp/whale-pair-standby-health.$$)")
    fi
    rm -f /tmp/whale-pair-standby-health.$$
fi

if [ "${#ISSUES[@]}" -eq 0 ]; then
    echo "promotion_ready=yes"
else
    echo "promotion_ready=no"
    for issue in "${ISSUES[@]}"; do
        echo "promotion_guardrail=$issue"
    done
fi

if [ "$ASSERT_READY" -eq 1 ] && [ "${#ISSUES[@]}" -gt 0 ]; then
    exit 1
fi
