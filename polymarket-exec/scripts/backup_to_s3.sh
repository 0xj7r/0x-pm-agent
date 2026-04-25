#!/usr/bin/env bash
# Periodic backup of polymarket-exec runtime data to S3.
# Designed for systemd timer or cron use. Idempotent: only re-uploads files
# that have changed since last run (using local manifest).
#
# WHY: sustained shadow_live sessions accumulate SQLite + JSONL on local
# disk. This script ports them to S3 so local disk doesn't fill.
#
# WHAT IT BACKS UP:
#   data/runtime/*/order-store.sqlite       (SQLite, VACUUM'd to snapshot)
#   data/execution/audit/*/audit.jsonl*     (audit logs incl. rotated)
#   data/execution/live/*/journal.jsonl*    (live journals)
#   data/execution/paper/*/journal.jsonl*   (paper journals)
#   data/calibration/*.snap.jsonl           (Phase 5 book snapshots)
#   data/calibration/*.report.json          (Phase 3 paper reports)
#
# REQUIRED ENV:
#   POLYMARKET_BACKUP_S3_BUCKET            S3 bucket name
#   POLYMARKET_BACKUP_S3_PREFIX            (optional) prefix in bucket;
#                                          default: polymarket-exec/$(hostname)
#   POLYMARKET_BACKUP_AWS_PROFILE          (optional) AWS CLI profile
#
# OPTIONAL ENV:
#   POLYMARKET_BACKUP_PRUNE_LOCAL=true     after upload, delete LOCAL
#                                          rotated journals older than
#                                          POLYMARKET_BACKUP_PRUNE_AGE_HOURS
#   POLYMARKET_BACKUP_PRUNE_AGE_HOURS=72   default 72h
#   POLYMARKET_BACKUP_DRY_RUN=true         show what would happen, don't upload
#
# EXIT CODES:
#   0  success
#   1  missing required env
#   2  AWS CLI not installed
#   3  upload failure (one or more files)

set -euo pipefail

if [ -z "${POLYMARKET_BACKUP_S3_BUCKET:-}" ]; then
  echo "ERROR: POLYMARKET_BACKUP_S3_BUCKET is required" >&2
  exit 1
fi
if ! command -v aws >/dev/null 2>&1; then
  echo "ERROR: aws CLI not installed (brew install awscli or apt install awscli)" >&2
  exit 2
fi

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BUCKET="$POLYMARKET_BACKUP_S3_BUCKET"
PREFIX="${POLYMARKET_BACKUP_S3_PREFIX:-polymarket-exec/$(hostname -s)}"
AWS_PROFILE_OPT=""
if [ -n "${POLYMARKET_BACKUP_AWS_PROFILE:-}" ]; then
  AWS_PROFILE_OPT="--profile ${POLYMARKET_BACKUP_AWS_PROFILE}"
fi
DRY_RUN="${POLYMARKET_BACKUP_DRY_RUN:-false}"
PRUNE_LOCAL="${POLYMARKET_BACKUP_PRUNE_LOCAL:-false}"
PRUNE_AGE_HOURS="${POLYMARKET_BACKUP_PRUNE_AGE_HOURS:-72}"

MANIFEST="$ROOT_DIR/data/.backup_manifest"
mkdir -p "$(dirname "$MANIFEST")"
touch "$MANIFEST"
TIMESTAMP="$(date -u +%Y%m%dT%H%M%SZ)"

log() { echo "[backup_to_s3 $TIMESTAMP] $*"; }
fail_count=0

upload_file() {
  local local_path="$1"
  local s3_key="$2"
  local cur_hash
  cur_hash="$(shasum -a 256 "$local_path" 2>/dev/null | awk '{print $1}')" \
    || cur_hash="$(sha256sum "$local_path" | awk '{print $1}')"
  local manifest_line
  manifest_line="$(grep -F "$local_path" "$MANIFEST" 2>/dev/null || true)"
  if [ -n "$manifest_line" ] && [[ "$manifest_line" == *"$cur_hash"* ]]; then
    log "skip unchanged: $local_path"
    return 0
  fi
  if [ "$DRY_RUN" = "true" ]; then
    log "DRY RUN would upload $local_path -> s3://$BUCKET/$s3_key"
  else
    if aws s3 cp $AWS_PROFILE_OPT "$local_path" "s3://$BUCKET/$s3_key" \
        --only-show-errors; then
      log "uploaded $local_path -> s3://$BUCKET/$s3_key"
      grep -vF "$local_path" "$MANIFEST" > "$MANIFEST.tmp" 2>/dev/null || true
      mv "$MANIFEST.tmp" "$MANIFEST" 2>/dev/null || true
      echo "$local_path $cur_hash $TIMESTAMP" >> "$MANIFEST"
    else
      log "FAILED upload $local_path"
      fail_count=$((fail_count + 1))
    fi
  fi
}

# 1. SQLite order stores: VACUUM INTO a snapshot, then upload the snapshot.
for db in "$ROOT_DIR"/data/runtime/*/order-store.sqlite; do
  [ -f "$db" ] || continue
  sleeve="$(basename "$(dirname "$db")")"
  snap="${db}.backup-${TIMESTAMP}.sqlite"
  if [ "$DRY_RUN" != "true" ]; then
    sqlite3 "$db" "VACUUM INTO '$snap';" || {
      log "FAILED VACUUM INTO for $db"
      fail_count=$((fail_count + 1))
      continue
    }
  fi
  upload_file "$snap" "$PREFIX/sqlite/$sleeve/order-store-$TIMESTAMP.sqlite"
  [ "$DRY_RUN" = "true" ] || rm -f "$snap"
done

# 2. JSONL stuff: audit, journal (live + paper), book snapshots, paper reports.
for pattern in \
  "$ROOT_DIR"/data/execution/audit/*/audit.jsonl* \
  "$ROOT_DIR"/data/execution/live/*/journal.jsonl* \
  "$ROOT_DIR"/data/execution/paper/*/journal.jsonl* \
  "$ROOT_DIR"/data/calibration/*.snap.jsonl \
  "$ROOT_DIR"/data/calibration/*.report.json
do
  for f in $pattern; do
    [ -f "$f" ] || continue
    rel="${f#$ROOT_DIR/}"
    upload_file "$f" "$PREFIX/$rel"
  done
done

# 3. Optional: prune rotated journals/audits older than N hours.
# NEVER prunes the active (non-rotated) file. Only files matching .N suffix
# (e.g. journal.jsonl.1, journal.jsonl.2) past PRUNE_AGE_HOURS.
if [ "$PRUNE_LOCAL" = "true" ] && [ "$DRY_RUN" != "true" ]; then
  log "pruning rotated logs older than ${PRUNE_AGE_HOURS}h"
  find "$ROOT_DIR/data/execution" \
    -type f \
    \( -name "journal.jsonl.*" -o -name "audit.jsonl.*" \) \
    -mmin +$((PRUNE_AGE_HOURS * 60)) \
    -print -delete
fi

if [ $fail_count -gt 0 ]; then
  log "completed with $fail_count failures"
  exit 3
fi
log "completed successfully"
