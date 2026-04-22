#!/usr/bin/env bash
# Kill switch for the Dublin whale-pair live bot.
# Modes:
#   --soft : restart the container without the --execute flag. The bot keeps
#            running and observing, but stops submitting orders. Open paired
#            positions on Polymarket are NOT touched.
#   --hard : stop the container entirely. Open paired positions are NOT
#            touched. Operator must reconcile the ledger by hand.
#
# Neither mode withdraws funds, revokes API keys, or cancels standing orders
# on Polymarket. Those steps live outside this script and require deliberate
# operator action.

set -euo pipefail

MODE="${1:-}"
if [ -z "$MODE" ]; then
    echo "Usage: $0 --soft | --hard" >&2
    exit 64
fi

REPO_ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
COMPOSE_FILE="$REPO_ROOT/scripts/deploy/whale-pair/docker-compose.whale-pair.yml"
OVERRIDE="$REPO_ROOT/scripts/deploy/whale-pair/docker-compose.override.yml"

case "$MODE" in
    --soft)
        echo "=== Soft kill: dropping --execute and restarting ==="
        cat >"$OVERRIDE" <<'YAML'
services:
  whale-pair-live:
    environment:
      WHALE_PAIR_MODE: "dry-run-killswitch"
YAML
        docker compose -f "$COMPOSE_FILE" -f "$OVERRIDE" up -d --force-recreate --no-build
        echo "Soft kill applied. Bot is still observing but will not submit."
        echo "Reconcile open pairs in the ledger before re-enabling --execute."
        ;;
    --hard)
        echo "=== Hard kill: stopping container ==="
        docker compose -f "$COMPOSE_FILE" stop whale-pair-live
        echo "Hard kill applied. Container stopped."
        echo "WARNING: open paired positions on Polymarket are NOT automatically closed."
        echo "         Inspect the ledger:"
        echo "           sqlite3 $REPO_ROOT/data/whale_pair_live.db '.tables'"
        echo "         and decide whether to let pairs run to settlement, resolve manually,"
        echo "         or restart the bot in --live mode to let it merge/complete."
        ;;
    *)
        echo "Unknown mode: $MODE" >&2
        echo "Usage: $0 --soft | --hard" >&2
        exit 64
        ;;
esac
