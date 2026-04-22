#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: render_alertmanager_config.sh [--output PATH]

Render the whale-pair Alertmanager config from environment variables into a
runtime file under data/monitoring/alertmanager/.

Supported env vars:
  WHALE_PAIR_DISCORD_WEBHOOK_URL
  WHALE_PAIR_TELEGRAM_BOT_TOKEN
  WHALE_PAIR_TELEGRAM_CHAT_ID
  WHALE_PAIR_ALERT_WEBHOOK_URL
  WHALE_PAIR_ALERT_WEBHOOK_BEARER_TOKEN
  WHALE_PAIR_ALERT_SEND_RESOLVED            (default: true)
  WHALE_PAIR_ALERT_GROUP_BY                 (default: alertname,service,severity)
  WHALE_PAIR_ALERT_GROUP_WAIT               (default: 30s)
  WHALE_PAIR_ALERT_GROUP_INTERVAL           (default: 5m)
  WHALE_PAIR_ALERT_REPEAT_INTERVAL          (default: 4h)
  WHALE_PAIR_ALERT_RESOLVE_TIMEOUT          (default: 5m)
EOF
}

REPO_ROOT="${REPO_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}"
OUTPUT="${OUTPUT:-$REPO_ROOT/data/monitoring/alertmanager/alertmanager.yml}"

while [ "$#" -gt 0 ]; do
  case "$1" in
    --output)
      [ "$#" -ge 2 ] || {
        echo "missing value for --output" >&2
        exit 1
      }
      OUTPUT="$2"
      shift 2
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 1
      ;;
  esac
done

yaml_quote() {
  printf "'%s'" "$(printf '%s' "$1" | sed "s/'/''/g")"
}

bool_or_die() {
  case "$1" in
    true|false) ;;
    *)
      echo "expected true|false, got: $1" >&2
      exit 1
      ;;
  esac
}

DISCORD_WEBHOOK_URL="${WHALE_PAIR_DISCORD_WEBHOOK_URL:-}"
TELEGRAM_BOT_TOKEN="${WHALE_PAIR_TELEGRAM_BOT_TOKEN:-}"
TELEGRAM_CHAT_ID="${WHALE_PAIR_TELEGRAM_CHAT_ID:-}"
ALERT_WEBHOOK_URL="${WHALE_PAIR_ALERT_WEBHOOK_URL:-}"
ALERT_WEBHOOK_BEARER_TOKEN="${WHALE_PAIR_ALERT_WEBHOOK_BEARER_TOKEN:-}"
SEND_RESOLVED="${WHALE_PAIR_ALERT_SEND_RESOLVED:-true}"
GROUP_BY_CSV="${WHALE_PAIR_ALERT_GROUP_BY:-alertname,service,severity}"
GROUP_WAIT="${WHALE_PAIR_ALERT_GROUP_WAIT:-30s}"
GROUP_INTERVAL="${WHALE_PAIR_ALERT_GROUP_INTERVAL:-5m}"
REPEAT_INTERVAL="${WHALE_PAIR_ALERT_REPEAT_INTERVAL:-4h}"
RESOLVE_TIMEOUT="${WHALE_PAIR_ALERT_RESOLVE_TIMEOUT:-5m}"

bool_or_die "$SEND_RESOLVED"

if [ -n "$TELEGRAM_BOT_TOKEN" ] && [ -z "$TELEGRAM_CHAT_ID" ]; then
  echo "WHALE_PAIR_TELEGRAM_CHAT_ID is required when WHALE_PAIR_TELEGRAM_BOT_TOKEN is set" >&2
  exit 1
fi
if [ -z "$TELEGRAM_BOT_TOKEN" ] && [ -n "$TELEGRAM_CHAT_ID" ]; then
  echo "WHALE_PAIR_TELEGRAM_BOT_TOKEN is required when WHALE_PAIR_TELEGRAM_CHAT_ID is set" >&2
  exit 1
fi
if [ -n "$TELEGRAM_CHAT_ID" ] && ! printf '%s' "$TELEGRAM_CHAT_ID" | grep -Eq '^-?[0-9]+$'; then
  echo "WHALE_PAIR_TELEGRAM_CHAT_ID must be an integer" >&2
  exit 1
fi
if [ -n "$ALERT_WEBHOOK_BEARER_TOKEN" ] && [ -z "$ALERT_WEBHOOK_URL" ]; then
  echo "WHALE_PAIR_ALERT_WEBHOOK_URL is required when WHALE_PAIR_ALERT_WEBHOOK_BEARER_TOKEN is set" >&2
  exit 1
fi

OUTPUT_DIR="$(dirname "$OUTPUT")"
mkdir -p "$OUTPUT_DIR" "$OUTPUT_DIR/data"

ROUTE_RECEIVER="blackhole"
if [ -n "$DISCORD_WEBHOOK_URL" ] || [ -n "$TELEGRAM_BOT_TOKEN" ] || [ -n "$ALERT_WEBHOOK_URL" ]; then
  ROUTE_RECEIVER="whale-pair-notifications"
fi

TMP="$(mktemp)"
trap 'rm -f "$TMP"' EXIT

{
  cat <<EOF
global:
  resolve_timeout: $RESOLVE_TIMEOUT

route:
  receiver: $ROUTE_RECEIVER
  group_by:
EOF

  OLD_IFS="$IFS"
  IFS=','
  for label in $GROUP_BY_CSV; do
    trimmed="$(printf '%s' "$label" | sed 's/^[[:space:]]*//; s/[[:space:]]*$//')"
    [ -n "$trimmed" ] || continue
    printf '    - %s\n' "$trimmed"
  done
  IFS="$OLD_IFS"

  cat <<EOF
  group_wait: $GROUP_WAIT
  group_interval: $GROUP_INTERVAL
  repeat_interval: $REPEAT_INTERVAL

receivers:
  - name: blackhole
EOF

  if [ "$ROUTE_RECEIVER" = "whale-pair-notifications" ]; then
    cat <<'EOF'
  - name: whale-pair-notifications
EOF

    if [ -n "$DISCORD_WEBHOOK_URL" ]; then
      cat <<EOF
    discord_configs:
      - send_resolved: $SEND_RESOLVED
        webhook_url: $(yaml_quote "$DISCORD_WEBHOOK_URL")
        title: '{{ template "discord.default.title" . }}'
        message: '{{ template "discord.default.message" . }}'
        content: '{{ template "discord.default.content" . }}'
EOF
    fi

    if [ -n "$TELEGRAM_BOT_TOKEN" ]; then
      cat <<EOF
    telegram_configs:
      - send_resolved: $SEND_RESOLVED
        bot_token: $(yaml_quote "$TELEGRAM_BOT_TOKEN")
        chat_id: $TELEGRAM_CHAT_ID
        message: '{{ template "telegram.default.message" . }}'
EOF
    fi

    if [ -n "$ALERT_WEBHOOK_URL" ]; then
      cat <<EOF
    webhook_configs:
      - send_resolved: $SEND_RESOLVED
        url: $(yaml_quote "$ALERT_WEBHOOK_URL")
EOF
      if [ -n "$ALERT_WEBHOOK_BEARER_TOKEN" ]; then
        cat <<EOF
        http_config:
          authorization:
            type: Bearer
            credentials: $(yaml_quote "$ALERT_WEBHOOK_BEARER_TOKEN")
EOF
      fi
    fi
  fi
} >"$TMP"

mv "$TMP" "$OUTPUT"
chmod 0600 "$OUTPUT" || true

echo "Rendered Alertmanager config: $OUTPUT"
if [ "$ROUTE_RECEIVER" = "blackhole" ]; then
  echo "No outbound alert transport configured; alerts will remain local in Alertmanager." >&2
fi
