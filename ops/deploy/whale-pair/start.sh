#!/usr/bin/env bash
# Start the whale-pair live bot on the Dublin host.
# Must be run from /opt/polymarket-agent on the host.
#
# Modes:
#   --dry-run : no --execute flag, bot observes but does not submit
#   --live    : adds --execute, actually trades
#
# The mode toggle is handled by exporting WHALE_PAIR_EXECUTE, which the
# compose file interpolates into the container command.

set -euo pipefail

MODE="${1:-}"
if [ -z "$MODE" ]; then
    echo "Usage: $0 --dry-run | --live" >&2
    exit 64
fi

REPO_ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
COMPOSE_FILE="$REPO_ROOT/scripts/deploy/whale-pair/docker-compose.whale-pair.yml"
ENV_FILE="${ENV_FILE:-$REPO_ROOT/.env}"
DATA_DIR="${DATA_DIR:-$REPO_ROOT/data}"
ROLE_FILE="${ROLE_FILE:-$DATA_DIR/whale_pair_standby.role}"

if [ ! -f "$COMPOSE_FILE" ]; then
    echo "ERROR: compose file not found: $COMPOSE_FILE" >&2
    exit 2
fi

if [ "$MODE" = "--live" ] && [ -f "$ROLE_FILE" ]; then
    ROLE_VALUE="$(grep -E '^role=' "$ROLE_FILE" | head -1 | cut -d= -f2-)"
    if [ "$ROLE_VALUE" = "passive-standby" ] && [ "${WHALE_PAIR_ALLOW_STANDBY_PROMOTION:-0}" != "1" ]; then
        echo "ERROR: $ROLE_FILE marks this host as passive-standby; use promote_standby.sh for live failover" >&2
        exit 65
    fi
fi

if [ ! -f "$ENV_FILE" ]; then
    echo "ERROR: env file not found: $ENV_FILE (expected on the deployment host)" >&2
    exit 3
fi

bash "$REPO_ROOT/scripts/deploy/whale-pair/check_env.sh" "$ENV_FILE"

case "$MODE" in
    --dry-run)
        echo "=== Starting whale-pair in DRY-RUN mode (no --execute) ==="
        # We edit the compose command by overlaying through a compose override
        # generated on the fly. Simpler than editing YAML in place.
        OVERRIDE="$REPO_ROOT/scripts/deploy/whale-pair/docker-compose.override.yml"
        cat >"$OVERRIDE" <<'YAML'
services:
  whale-pair-live:
    environment:
      WHALE_PAIR_MODE: "dry-run"
YAML
        docker compose -f "$COMPOSE_FILE" -f "$OVERRIDE" up -d --build
        ;;
    --live)
        echo "=== Starting whale-pair in LIVE mode (--execute enabled) ==="
        OVERRIDE="$REPO_ROOT/scripts/deploy/whale-pair/docker-compose.override.yml"
        # In live mode we append --execute to the command via override.
        cat >"$OVERRIDE" <<'YAML'
services:
  whale-pair-live:
    environment:
      WHALE_PAIR_MODE: "live"
    command:
      - "--db"
      - "/data/whale_pair_live.db"
      - "--loop"
      - "15"
      - "--base-clip-usd"
      - "10.0"
      - "--aggressive-clip-usd"
      - "50.0"
      - "--max-gross-cost-usd"
      - "200.0"
      - "--ws-max-age-ms"
      - "2000"
      - "--execute"
YAML
        docker compose -f "$COMPOSE_FILE" -f "$OVERRIDE" up -d --build
        ;;
    *)
        echo "Unknown mode: $MODE" >&2
        echo "Usage: $0 --dry-run | --live" >&2
        exit 64
        ;;
esac

echo ""
echo "Container status:"
docker compose -f "$COMPOSE_FILE" ps
echo ""
echo "Tail logs with:"
echo "  docker compose -f $COMPOSE_FILE logs -f whale-pair-live"
