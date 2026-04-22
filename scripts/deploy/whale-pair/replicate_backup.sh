#!/usr/bin/env bash
# Replicate whale-pair backup artifacts to a standby host or local directory.

set -euo pipefail

usage() {
    cat >&2 <<'EOF'
Usage: replicate_backup.sh <archive> <checksum> <manifest> [--dest DEST]
       replicate_backup.sh --latest [--backup-dir DIR] --dest DEST

Options:
  --dest DEST       required unless BACKUP_REPLICA_DEST is set; rsync target or local dir
  --backup-dir DIR  source backup directory for --latest mode
  --latest          replicate the newest local archive set
EOF
}

REPO_ROOT="${REPO_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}"
DATA_DIR="${DATA_DIR:-$REPO_ROOT/data}"
BACKUP_DIR="${BACKUP_DIR:-$DATA_DIR/backups}"
DEST="${BACKUP_REPLICA_DEST:-}"
LATEST=0
ARCHIVE_PATH=""
CHECKSUM_PATH=""
MANIFEST_PATH=""

while [ $# -gt 0 ]; do
    case "$1" in
        --dest)
            DEST="${2:?missing value for --dest}"
            shift 2
            ;;
        --backup-dir)
            BACKUP_DIR="${2:?missing value for --backup-dir}"
            shift 2
            ;;
        --latest)
            LATEST=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            if [ -z "$ARCHIVE_PATH" ]; then
                ARCHIVE_PATH="$1"
            elif [ -z "$CHECKSUM_PATH" ]; then
                CHECKSUM_PATH="$1"
            elif [ -z "$MANIFEST_PATH" ]; then
                MANIFEST_PATH="$1"
            else
                echo "ERROR: unexpected arg: $1" >&2
                usage
                exit 64
            fi
            shift
            ;;
    esac
done

if [ -z "$DEST" ]; then
    echo "ERROR: destination required via --dest or BACKUP_REPLICA_DEST" >&2
    exit 64
fi

if [ "$LATEST" -eq 1 ]; then
    if [ -n "$ARCHIVE_PATH" ] || [ -n "$CHECKSUM_PATH" ] || [ -n "$MANIFEST_PATH" ]; then
        echo "ERROR: --latest cannot be combined with explicit artifact paths" >&2
        exit 64
    fi
    ARCHIVE_PATH="$(find "$BACKUP_DIR" -maxdepth 1 -type f -name 'whale-pair-data-*.tar.gz' | sort | tail -1)"
    if [ -z "$ARCHIVE_PATH" ]; then
        echo "ERROR: no backup archives found in $BACKUP_DIR" >&2
        exit 2
    fi
    CHECKSUM_PATH="${ARCHIVE_PATH%.tar.gz}.sha256"
    MANIFEST_PATH="${ARCHIVE_PATH%.tar.gz}.manifest.json"
fi

for path in "$ARCHIVE_PATH" "$CHECKSUM_PATH" "$MANIFEST_PATH"; do
    if [ ! -f "$path" ]; then
        echo "ERROR: backup artifact not found: $path" >&2
        exit 2
    fi
done

case "$DEST" in
    *:*) DEST_IS_REMOTE=1 ;;
    *) DEST_IS_REMOTE=0 ;;
esac

if [ "$DEST_IS_REMOTE" -eq 0 ]; then
    mkdir -p "$DEST"
fi

if command -v rsync >/dev/null 2>&1; then
    rsync -azv "$ARCHIVE_PATH" "$CHECKSUM_PATH" "$MANIFEST_PATH" "$DEST"
else
    case "$DEST" in
        *:*)
            echo "ERROR: rsync is unavailable and destination looks remote: $DEST" >&2
            exit 69
            ;;
        *)
            mkdir -p "$DEST"
            cp -p "$ARCHIVE_PATH" "$CHECKSUM_PATH" "$MANIFEST_PATH" "$DEST"/
            ;;
    esac
fi

echo "Replication complete:"
echo "  archive : $ARCHIVE_PATH"
echo "  checksum: $CHECKSUM_PATH"
echo "  manifest: $MANIFEST_PATH"
echo "  dest    : $DEST"
