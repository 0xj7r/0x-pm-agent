#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: send_test_alert.sh [--severity warning|critical] [--service NAME] [--alertname NAME]

Post a synthetic firing alert to the local whale-pair Alertmanager so Discord /
Telegram / webhook routing can be validated on demand.
EOF
}

ALERTMANAGER_URL="${ALERTMANAGER_URL:-http://127.0.0.1:${WHALE_PAIR_ALERTMANAGER_PORT:-9093}}"
SEVERITY="${WHALE_PAIR_TEST_ALERT_SEVERITY:-warning}"
SERVICE="${WHALE_PAIR_TEST_ALERT_SERVICE:-whale-pair-live}"
ALERTNAME="${WHALE_PAIR_TEST_ALERT_NAME:-WhalePairSyntheticTestAlert}"

while [ "$#" -gt 0 ]; do
  case "$1" in
    --severity)
      [ "$#" -ge 2 ] || {
        echo "missing value for --severity" >&2
        exit 1
      }
      SEVERITY="$2"
      shift 2
      ;;
    --service)
      [ "$#" -ge 2 ] || {
        echo "missing value for --service" >&2
        exit 1
      }
      SERVICE="$2"
      shift 2
      ;;
    --alertname)
      [ "$#" -ge 2 ] || {
        echo "missing value for --alertname" >&2
        exit 1
      }
      ALERTNAME="$2"
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

case "$SEVERITY" in
  warning|critical|info) ;;
  *)
    echo "severity must be one of: info, warning, critical" >&2
    exit 1
    ;;
esac

NOW_UTC="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"

PAYLOAD="$(cat <<EOF
[
  {
    "labels": {
      "alertname": "$ALERTNAME",
      "service": "$SERVICE",
      "severity": "$SEVERITY",
      "team": "whale-pair",
      "source": "manual-test"
    },
    "annotations": {
      "summary": "Synthetic whale-pair test alert",
      "description": "Manual transport test from scripts/deploy/whale-pair/send_test_alert.sh"
    },
    "startsAt": "$NOW_UTC",
    "generatorURL": "file://send_test_alert.sh"
  }
]
EOF
)"

curl -fsS \
  -H "Content-Type: application/json" \
  -d "$PAYLOAD" \
  "$ALERTMANAGER_URL/api/v2/alerts" >/dev/null

echo "Posted synthetic alert '$ALERTNAME' to $ALERTMANAGER_URL"
