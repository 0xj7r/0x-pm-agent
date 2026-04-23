#!/usr/bin/env bash
set -euo pipefail

script_name="$(basename "$0")"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

canonical_target=""

if [[ "$script_name" == whale_pair_*.py || "$script_name" == whale_pair_*.sh ]]; then
  canonical_target="whale_pair/cmd/$script_name"
elif [[ -f "$SCRIPT_DIR/analysis/$script_name" ]]; then
  canonical_target="analysis/$script_name"
elif [[ -f "$SCRIPT_DIR/bots/$script_name" ]]; then
  canonical_target="bots/$script_name"
elif [[ -f "$SCRIPT_DIR/dataops/$script_name" ]]; then
  canonical_target="dataops/$script_name"
else
  echo "Unknown legacy script wrapper: $script_name" >&2
  exit 2
fi

exec_target="$SCRIPT_DIR/$canonical_target"
case "$exec_target" in
  *.py)
    exec python3 "$exec_target" "$@"
    ;;
  *)
    exec "$exec_target" "$@"
    ;;
esac
