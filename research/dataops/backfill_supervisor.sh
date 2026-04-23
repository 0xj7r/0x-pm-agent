#!/bin/bash
# Supervisor loop for the PolyBackTest fetcher.
#
# Runs the fetcher for a single coin in an infinite retry loop. If the
# fetcher exits non-zero (crash, unrecoverable retry exhaustion, auth
# failure), waits briefly and restarts. On successful (exit 0)
# completion, exits normally.
#
# The fetcher itself is idempotent: the existing_snaps check skips
# markets that already have rows in the db, so restart behaves as a
# checkpoint resume.
#
# Usage (inside tmux or nohup):
#   ./scripts/backfill_supervisor.sh btc /var/log/backfill-btc.log
#   ./scripts/backfill_supervisor.sh eth /var/log/backfill-eth.log
#
# Optional env vars:
#   BACKFILL_PYTHON   path to python interpreter (default: python3)
#   BACKFILL_WORKDIR  working dir (default: parent of this script)
#   BACKFILL_LIMIT    market limit (default: 9000)
#   BACKFILL_WORKERS  snapshot workers (default: 6)
#   BACKFILL_RESTART_DELAY  seconds to wait between restarts (default: 10)

set -u

COIN="${1:?usage: $0 <coin> <logfile>}"
LOGFILE="${2:?usage: $0 <coin> <logfile>}"

PYTHON="${BACKFILL_PYTHON:-python3}"
WORKDIR="${BACKFILL_WORKDIR:-$(cd "$(dirname "$0")/.." && pwd)}"
LIMIT="${BACKFILL_LIMIT:-9000}"
WORKERS="${BACKFILL_WORKERS:-6}"
RESTART_DELAY="${BACKFILL_RESTART_DELAY:-10}"

cd "$WORKDIR"

attempt=0
while true; do
  attempt=$((attempt + 1))
  ts=$(date -u +'%Y-%m-%dT%H:%M:%SZ')
  echo "=== [$ts] supervisor: starting $COIN fetcher, attempt $attempt ===" | tee -a "$LOGFILE"

  "$PYTHON" -m backtesting.data.fetcher \
    --coin "$COIN" \
    --limit "$LIMIT" \
    --move-threshold 0 \
    --snapshot-workers "$WORKERS" \
    >> "$LOGFILE" 2>&1

  rc=$?
  ts=$(date -u +'%Y-%m-%dT%H:%M:%SZ')
  if [ $rc -eq 0 ]; then
    echo "=== [$ts] supervisor: $COIN fetcher exited cleanly, stopping supervisor ===" | tee -a "$LOGFILE"
    exit 0
  fi

  echo "=== [$ts] supervisor: $COIN fetcher exited with code $rc, restarting in ${RESTART_DELAY}s ===" | tee -a "$LOGFILE"
  sleep "$RESTART_DELAY"
done
