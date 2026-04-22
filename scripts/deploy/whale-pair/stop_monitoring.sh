#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="${REPO_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}"
COMPOSE_FILE="$REPO_ROOT/scripts/deploy/whale-pair/docker-compose.monitoring.whale-pair.yml"

docker compose -f "$COMPOSE_FILE" down
