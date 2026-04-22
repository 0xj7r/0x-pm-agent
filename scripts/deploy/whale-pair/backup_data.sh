#!/usr/bin/env bash
# Create a recoverable backup of the whale-pair data directory.
#
# Default source is /opt/polymarket-agent/data when run on the deploy host.
# Backups are written under $DATA_DIR/backups unless --dest is provided.
#
# The script snapshots SQLite ledgers with `sqlite3 .backup` when available,
# excludes the backups directory from the archive payload, writes a manifest +
# checksum, and prunes older archives by default.

set -euo pipefail

usage() {
    cat >&2 <<'EOF'
Usage: backup_data.sh [--dest DIR] [--keep N] [--name LABEL] [--no-prune]

Options:
  --dest DIR     backup output directory (default: $DATA_DIR/backups)
  --keep N       keep the newest N archives after completion (default: 7)
  --name LABEL   append a label to the archive basename
  --no-prune     disable pruning of older archives
EOF
}

REPO_ROOT="${REPO_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}"
DATA_DIR="${DATA_DIR:-$REPO_ROOT/data}"
BACKUP_DIR_DEFAULT="$DATA_DIR/backups"
BACKUP_DIR="$BACKUP_DIR_DEFAULT"
KEEP="${KEEP:-7}"
LABEL=""
PRUNE=1

while [ $# -gt 0 ]; do
    case "$1" in
        --dest)
            BACKUP_DIR="${2:?missing value for --dest}"
            shift 2
            ;;
        --keep)
            KEEP="${2:?missing value for --keep}"
            shift 2
            ;;
        --name)
            LABEL="${2:?missing value for --name}"
            shift 2
            ;;
        --no-prune)
            PRUNE=0
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "ERROR: unknown arg: $1" >&2
            usage
            exit 64
            ;;
    esac
done

if [ ! -d "$DATA_DIR" ]; then
    echo "ERROR: data dir not found: $DATA_DIR" >&2
    exit 2
fi

case "$KEEP" in
    ''|*[!0-9]*)
        echo "ERROR: --keep must be a non-negative integer" >&2
        exit 64
        ;;
esac

mkdir -p "$BACKUP_DIR"

HOSTNAME_SHORT="$(hostname -s 2>/dev/null || hostname || echo unknown-host)"
STAMP="$(date -u +'%Y%m%dT%H%M%SZ')"
SAFE_LABEL=""
if [ -n "$LABEL" ]; then
    SAFE_LABEL="-$(printf '%s' "$LABEL" | tr -cs 'A-Za-z0-9._-' '-')"
fi
BASE="whale-pair-data-${HOSTNAME_SHORT}-${STAMP}${SAFE_LABEL}"
ARCHIVE_PATH="$BACKUP_DIR/${BASE}.tar.gz"
CHECKSUM_PATH="$BACKUP_DIR/${BASE}.sha256"
MANIFEST_PATH="$BACKUP_DIR/${BASE}.manifest.json"

TMP_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/whale-pair-backup.XXXXXX")"
cleanup() {
    rm -rf "$TMP_ROOT"
}
trap cleanup EXIT

STAGE_DIR="$TMP_ROOT/stage"
STAGE_DATA="$STAGE_DIR/data"
mkdir -p "$STAGE_DATA"

# Copy non-database files first, excluding the local backup store.
(
    cd "$DATA_DIR"
    tar \
        --exclude='./backups' \
        --exclude='*.db' \
        -cf - .
) | (
    cd "$STAGE_DATA"
    tar -xf -
)

DB_COUNT=0
DB_MODE="copy"
while IFS= read -r db_path; do
    rel_path="${db_path#$DATA_DIR/}"
    dest_path="$STAGE_DATA/$rel_path"
    mkdir -p "$(dirname "$dest_path")"
    if command -v sqlite3 >/dev/null 2>&1; then
        if sqlite3 "$db_path" ".timeout 5000" ".backup $dest_path" >/dev/null 2>&1; then
            DB_MODE="sqlite-backup"
        else
            cp -p "$db_path" "$dest_path"
            DB_MODE="copy"
        fi
    else
        cp -p "$db_path" "$dest_path"
    fi
    DB_COUNT=$((DB_COUNT + 1))
done < <(
    find "$DATA_DIR" \
        -path "$BACKUP_DIR" -prune -o \
        -path "$BACKUP_DIR/*" -prune -o \
        -type f -name '*.db' -print
)

tar -C "$STAGE_DIR" -czf "$ARCHIVE_PATH" data

if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$ARCHIVE_PATH" >"$CHECKSUM_PATH"
elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$ARCHIVE_PATH" >"$CHECKSUM_PATH"
else
    printf 'sha256-unavailable  %s\n' "$(basename "$ARCHIVE_PATH")" >"$CHECKSUM_PATH"
fi

ARCHIVE_SIZE_BYTES="$(wc -c <"$ARCHIVE_PATH" | tr -d ' ')"
cat >"$MANIFEST_PATH" <<EOF
{
  "created_at_utc": "$(date -u +'%Y-%m-%dT%H:%M:%SZ')",
  "host": "$HOSTNAME_SHORT",
  "source_data_dir": "$DATA_DIR",
  "archive_path": "$ARCHIVE_PATH",
  "checksum_path": "$CHECKSUM_PATH",
  "db_copy_mode": "$DB_MODE",
  "db_count": $DB_COUNT,
  "archive_size_bytes": $ARCHIVE_SIZE_BYTES
}
EOF

if [ "$PRUNE" -eq 1 ] && [ "$KEEP" -gt 0 ]; then
    archive_count="$(find "$BACKUP_DIR" -maxdepth 1 -type f -name 'whale-pair-data-*.tar.gz' | wc -l | tr -d ' ')"
    if [ "${archive_count:-0}" -gt "$KEEP" ]; then
        remove_count=$((archive_count - KEEP))
        find "$BACKUP_DIR" -maxdepth 1 -type f -name 'whale-pair-data-*.tar.gz' | sort | head -n "$remove_count" | \
        while IFS= read -r old_archive; do
            [ -n "$old_archive" ] || continue
            stem="${old_archive%.tar.gz}"
            rm -f "$old_archive" "${stem}.sha256" "${stem}.manifest.json"
        done
    fi
fi

echo "Backup created:"
echo "  archive : $ARCHIVE_PATH"
echo "  checksum: $CHECKSUM_PATH"
echo "  manifest: $MANIFEST_PATH"
echo "  db mode : $DB_MODE"
echo "  db count: $DB_COUNT"
