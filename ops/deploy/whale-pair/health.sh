#!/usr/bin/env bash
# Health check for the Dublin whale-pair live bot.
# Exit 0 if everything is healthy; non-zero with a diagnostic otherwise.
# Safe to call from cron or a monitoring system.

set -euo pipefail

REPO_ROOT="${REPO_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}"
COMPOSE_FILE="$REPO_ROOT/scripts/deploy/whale-pair/docker-compose.whale-pair.yml"
LEDGER="${LEDGER:-$REPO_ROOT/data/whale_pair_live.db}"
MAX_BOOK_AGE_MS="${MAX_BOOK_AGE_MS:-3000}"
MIN_DISK_FREE_GB="${MIN_DISK_FREE_GB:-5}"
LEDGER_STALE_SECONDS="${LEDGER_STALE_SECONDS:-300}"
ENFORCE_LEDGER_WRITE_FRESHNESS="${ENFORCE_LEDGER_WRITE_FRESHNESS:-0}"

fail() {
    echo "UNHEALTHY: $1" >&2
    exit 1
}

# 1. Container up.
if ! docker compose -f "$COMPOSE_FILE" ps --status running 2>/dev/null | grep -q whale-pair-live; then
    fail "container whale-pair-live is not running"
fi

# 2. Logs emitted in the last 60 s.
RECENT_LINES=$(docker compose -f "$COMPOSE_FILE" logs --since 60s whale-pair-live 2>/dev/null | wc -l | tr -d ' ')
if [ "${RECENT_LINES:-0}" -lt 1 ]; then
    fail "no log lines in the last 60 seconds"
fi

RECENT_LOGS=$(docker compose -f "$COMPOSE_FILE" logs --since 120s whale-pair-live 2>/dev/null || true)

# 3. book_ts present in recent logs (bot is ingesting market data).
if ! printf '%s\n' "$RECENT_LOGS" | grep -q "book_ts"; then
    fail "no 'book_ts' telemetry in the last 120 seconds; bot may not be receiving book data"
fi

# 4. book_age_ms below threshold.
RECENT_AGES=$(printf '%s\n' "$RECENT_LOGS" \
    | grep -oE '"book_age_ms":[ ]*[0-9.]+' \
    | awk -F: '{print $2}' \
    | tr -d ' ' \
    | tail -20 || true)
if [ -n "$RECENT_AGES" ]; then
    AVG_AGE=$(printf '%s\n' "$RECENT_AGES" \
        | awk 'BEGIN{s=0;n=0} {s+=$1;n+=1} END{if(n>0) printf "%d", s/n; else print "0"}')
    if [ "${AVG_AGE:-0}" -gt "$MAX_BOOK_AGE_MS" ]; then
        fail "avg book_age_ms=${AVG_AGE} exceeds threshold ${MAX_BOOK_AGE_MS}"
    fi
fi

# 5. Ledger file exists and, when enabled, was modified recently.
if [ ! -f "$LEDGER" ]; then
    fail "ledger file not found: $LEDGER"
fi
if [ "$ENFORCE_LEDGER_WRITE_FRESHNESS" = "1" ]; then
    if [ "$(uname)" = "Linux" ]; then
        LEDGER_MTIME=$(stat -c '%Y' "$LEDGER")
    else
        LEDGER_MTIME=$(stat -f '%m' "$LEDGER")
    fi
    NOW=$(date +%s)
    LEDGER_AGE=$((NOW - LEDGER_MTIME))
    if [ "$LEDGER_AGE" -gt "$LEDGER_STALE_SECONDS" ]; then
        fail "ledger $LEDGER has not been written to in $LEDGER_AGE s (>${LEDGER_STALE_SECONDS}s)"
    fi
fi

# 6. Disk free.
DISK_FREE_GB=$(df -BG "$REPO_ROOT/data" 2>/dev/null | awk 'NR==2 {gsub("G","",$4); print $4}' || echo 0)
if [ "${DISK_FREE_GB:-0}" -lt "$MIN_DISK_FREE_GB" ]; then
    fail "disk free on data dir is ${DISK_FREE_GB}G (<${MIN_DISK_FREE_GB}G)"
fi

# 7. Clock drift (Linux only; skip on other kernels).
if command -v chronyc >/dev/null 2>&1; then
    OFFSET_S=$(chronyc tracking 2>/dev/null | awk '/System time/ {print $4; exit}' || echo 0)
    OFFSET_MS=$(printf '%.0f' "$(echo "${OFFSET_S} * 1000" | bc -l 2>/dev/null || echo 0)" 2>/dev/null || echo 0)
    ABS_OFFSET_MS=${OFFSET_MS#-}
    if [ "${ABS_OFFSET_MS:-0}" -gt 100 ]; then
        fail "clock drift ${ABS_OFFSET_MS}ms exceeds 100ms"
    fi
fi

echo "HEALTHY: whale-pair-live container is up, telemetry fresh, ledger writing, disk OK"
exit 0
