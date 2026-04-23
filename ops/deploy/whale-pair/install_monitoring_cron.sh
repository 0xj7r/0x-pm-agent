#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="${REPO_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}"
SCRIPT="$REPO_ROOT/scripts/deploy/whale-pair/refresh_monitoring_metrics.sh"
CRON_EXPR="${WHALE_PAIR_MONITOR_CRON:-* * * * *}"
ENTRY="$CRON_EXPR REPO_ROOT=$REPO_ROOT bash $SCRIPT >/dev/null 2>&1"

TMP="$(mktemp)"
crontab -l 2>/dev/null | grep -v 'refresh_monitoring_metrics.sh' >"$TMP" || true
printf '%s\n' "$ENTRY" >>"$TMP"
crontab "$TMP"
rm -f "$TMP"

echo "Installed cron entry:"
echo "  $ENTRY"
