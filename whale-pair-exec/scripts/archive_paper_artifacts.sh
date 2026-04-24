#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
PAPER_DIR="${WHALE_PAIR_ARCHIVE_PAPER_DIR:-$ROOT_DIR/whale-pair-exec/data/execution/paper}"
RUNTIME_DIR="${WHALE_PAIR_ARCHIVE_RUNTIME_DIR:-$ROOT_DIR/whale-pair-exec/data/runtime}"
ARCHIVE_S3_URI="${WHALE_PAIR_ARCHIVE_S3_URI:-}"
STORAGE_CLASS="${WHALE_PAIR_ARCHIVE_STORAGE_CLASS:-DEEP_ARCHIVE}"
MIN_AGE_MINUTES="${WHALE_PAIR_ARCHIVE_MIN_AGE_MINUTES:-15}"
DELETE_AFTER_UPLOAD="${WHALE_PAIR_ARCHIVE_DELETE_LOCAL_AFTER_UPLOAD:-false}"
INCLUDE_PRE_FIX="${WHALE_PAIR_ARCHIVE_INCLUDE_PRE_FIX:-true}"
INCLUDE_RUNTIME_PRE_FIX="${WHALE_PAIR_ARCHIVE_INCLUDE_RUNTIME_PRE_FIX:-true}"
INCLUDE_ROTATED_SEGMENTS="${WHALE_PAIR_ARCHIVE_INCLUDE_ROTATED_SEGMENTS:-true}"
ACTIVE_JOURNAL_WARN_BYTES="${WHALE_PAIR_ARCHIVE_ACTIVE_JOURNAL_WARN_BYTES:-1073741824}"
LIST_ONLY="${WHALE_PAIR_ARCHIVE_LIST_ONLY:-false}"
DRY_RUN="${WHALE_PAIR_ARCHIVE_DRY_RUN:-false}"
AWS_BIN="${WHALE_PAIR_AWS_BIN:-aws}"

log() {
  echo "[archive-paper-artifacts] $1"
}

fail() {
  echo "[archive-paper-artifacts] ERROR: $1" >&2
  exit 1
}

bool_is_true() {
  case "${1:-}" in
    1|[Tt][Rr][Uu][Ee]|[Yy][Ee][Ss])
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

require_ready() {
  [[ -d "$PAPER_DIR" ]] || fail "paper dir not found: $PAPER_DIR"
  [[ -d "$RUNTIME_DIR" ]] || fail "runtime dir not found: $RUNTIME_DIR"
  if bool_is_true "$LIST_ONLY"; then
    return 0
  fi
  command -v "$AWS_BIN" >/dev/null 2>&1 || fail "aws cli not found: $AWS_BIN"
  [[ -n "$ARCHIVE_S3_URI" ]] || fail "WHALE_PAIR_ARCHIVE_S3_URI is required"
}

age_minutes() {
  local path="$1"
  local now
  now="$(date +%s)"
  local mtime
  mtime="$(stat -f %m "$path")"
  echo $(((now - mtime) / 60))
}

size_bytes() {
  local path="$1"
  stat -f %z "$path"
}

target_prefix() {
  if [[ -n "$ARCHIVE_S3_URI" ]]; then
    printf '%s' "${ARCHIVE_S3_URI%/}"
  else
    printf '%s' "<set WHALE_PAIR_ARCHIVE_S3_URI>"
  fi
}

upload_stream() {
  local target="$1"
  if bool_is_true "$DRY_RUN"; then
    log "dry-run upload -> $target"
    cat >/dev/null
    return 0
  fi
  "$AWS_BIN" s3 cp - "$target" --storage-class "$STORAGE_CLASS" --only-show-errors
}

upload_file() {
  local source="$1"
  local target="$2"
  if bool_is_true "$DRY_RUN"; then
    log "dry-run upload $source -> $target"
    return 0
  fi
  "$AWS_BIN" s3 cp "$source" "$target" --storage-class "$STORAGE_CLASS" --only-show-errors
}

maybe_delete() {
  local path="$1"
  if ! bool_is_true "$DELETE_AFTER_UPLOAD"; then
    return 0
  fi
  if bool_is_true "$DRY_RUN"; then
    log "dry-run delete $path"
    return 0
  fi
  rm -rf "$path"
  log "deleted local artifact $path"
}

check_aws_auth() {
  if bool_is_true "$LIST_ONLY"; then
    return 0
  fi
  if bool_is_true "$DRY_RUN"; then
    log "dry-run mode: skipping aws sts auth check"
    return 0
  fi
  if ! "$AWS_BIN" sts get-caller-identity --output text >/dev/null 2>&1; then
    fail "aws credentials are invalid or unavailable; fix 'aws sts get-caller-identity' before running archive upload"
  fi
}

warn_about_unbounded_active_journals() {
  local rotated_count=0
  while IFS= read -r _; do
    rotated_count=$((rotated_count + 1))
  done < <(find "$PAPER_DIR" -type f \( -name 'journal.*.jsonl' -o -name 'journal.*.jsonl.gz' \))

  while IFS= read -r path; do
    [[ -n "$path" ]] || continue
    local bytes
    bytes="$(size_bytes "$path")"
    if (( bytes < ACTIVE_JOURNAL_WARN_BYTES )); then
      continue
    fi
    local gib
    gib="$(awk -v b="$bytes" 'BEGIN { printf "%.2f", b / 1073741824 }')"
    if (( rotated_count == 0 )); then
      log "WARNING active journal exceeds threshold without any rotated segments present: ${path#$ROOT_DIR/} size=${gib}GiB threshold_bytes=$ACTIVE_JOURNAL_WARN_BYTES"
      log "WARNING verify WHALE_PAIR_EXEC_JOURNAL_ROTATE_BYTES is set in the actual launcher/systemd environment before relying on archive hygiene"
    else
      log "WARNING active journal remains above threshold: ${path#$ROOT_DIR/} size=${gib}GiB threshold_bytes=$ACTIVE_JOURNAL_WARN_BYTES"
    fi
  done < <(find "$PAPER_DIR" -type f -name 'journal.jsonl' | sort)
}

archive_prefixed_backup() {
  local path="$1"
  local rel="${path#$ROOT_DIR/}"
  local target="$(target_prefix)/${rel}.tar.gz"
  if bool_is_true "$LIST_ONLY"; then
    log "candidate backup dir $rel -> $target"
    return 0
  fi
  if bool_is_true "$DRY_RUN"; then
    log "dry-run backup dir $rel -> $target"
    return 0
  fi
  log "archiving backup dir $rel -> $target"
  tar -C "$ROOT_DIR" -czf - "$rel" | upload_stream "$target"
  maybe_delete "$path"
}

archive_prefixed_backups_in_dir() {
  local root="$1"
  while IFS= read -r path; do
    [[ -n "$path" ]] || continue
    if (( $(age_minutes "$path") < MIN_AGE_MINUTES )); then
      continue
    fi
    archive_prefixed_backup "$path"
    archived_any=true
  done < <(find "$root" -mindepth 1 -maxdepth 1 -type d -name '*.pre_fix_*' | sort)
}

archive_rotated_segment() {
  local path="$1"
  local rel="${path#$ROOT_DIR/}"
  if bool_is_true "$LIST_ONLY"; then
    if [[ "$path" == *.gz ]]; then
      log "candidate rotated segment $rel -> $(target_prefix)/${rel}"
    else
      log "candidate rotated segment $rel -> $(target_prefix)/${rel}.gz"
    fi
    return 0
  fi
  if bool_is_true "$DRY_RUN"; then
    if [[ "$path" == *.gz ]]; then
      log "dry-run rotated segment $rel -> $(target_prefix)/${rel}"
    else
      log "dry-run rotated segment $rel -> $(target_prefix)/${rel}.gz"
    fi
    return 0
  fi
  if [[ "$path" == *.gz ]]; then
    local target="${ARCHIVE_S3_URI%/}/${rel}"
    log "archiving rotated segment $rel -> $target"
    upload_file "$path" "$target"
  else
    local target="${ARCHIVE_S3_URI%/}/${rel}.gz"
    log "archiving rotated segment $rel -> $target"
    gzip -c "$path" | upload_stream "$target"
  fi
  maybe_delete "$path"
}

main() {
  require_ready
  local archived_any=false

  warn_about_unbounded_active_journals
  check_aws_auth

  if bool_is_true "$INCLUDE_PRE_FIX"; then
    archive_prefixed_backups_in_dir "$PAPER_DIR"
  fi

  if bool_is_true "$INCLUDE_RUNTIME_PRE_FIX"; then
    archive_prefixed_backups_in_dir "$RUNTIME_DIR"
  fi

  if bool_is_true "$INCLUDE_ROTATED_SEGMENTS"; then
    while IFS= read -r path; do
      [[ -n "$path" ]] || continue
      if (( $(age_minutes "$path") < MIN_AGE_MINUTES )); then
        continue
      fi
      archive_rotated_segment "$path"
      archived_any=true
    done < <(find "$PAPER_DIR" -type f \( -name 'journal.*.jsonl' -o -name 'journal.*.jsonl.gz' \) | sort)
  fi

  if [[ "$archived_any" == "false" ]]; then
    log "nothing eligible for archive in $PAPER_DIR"
  fi
}

main "$@"
