#!/usr/bin/env bash
# Restore a whale-pair data backup archive onto the current host.
#
# By default this restores into /opt/polymarket-agent/data. If the target
# already exists and contains files, --force is required; the current target
# is moved aside before extraction so rollback is possible.

set -euo pipefail

usage() {
    cat >&2 <<'EOF'
Usage: restore_data.sh <archive.tar.gz> [--target DIR] [--force]
       restore_data.sh --latest [--backup-dir DIR] [--target DIR] [--force]

Options:
  --latest          restore the newest backup archive from the backup dir
  --backup-dir DIR  directory containing backup archives
  --target DIR      restore destination (default: $REPO_ROOT/data)
  --force           move aside an existing non-empty target before restore
EOF
}

REPO_ROOT="${REPO_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}"
TARGET_DIR="${TARGET_DIR:-$REPO_ROOT/data}"
BACKUP_DIR="${BACKUP_DIR:-$TARGET_DIR/backups}"
ARCHIVE_PATH=""
RESTORE_LATEST=0
FORCE=0

while [ $# -gt 0 ]; do
    case "$1" in
        --latest)
            RESTORE_LATEST=1
            shift
            ;;
        --backup-dir)
            BACKUP_DIR="${2:?missing value for --backup-dir}"
            shift 2
            ;;
        --target)
            TARGET_DIR="${2:?missing value for --target}"
            shift 2
            ;;
        --force)
            FORCE=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            if [ -n "$ARCHIVE_PATH" ]; then
                echo "ERROR: multiple archives provided" >&2
                usage
                exit 64
            fi
            ARCHIVE_PATH="$1"
            shift
            ;;
    esac
done

if [ "$RESTORE_LATEST" -eq 1 ]; then
    if [ -n "$ARCHIVE_PATH" ]; then
        echo "ERROR: specify either an archive path or --latest, not both" >&2
        exit 64
    fi
    ARCHIVE_PATH="$(find "$BACKUP_DIR" -maxdepth 1 -type f -name 'whale-pair-data-*.tar.gz' | sort | tail -1)"
fi

if [ -z "$ARCHIVE_PATH" ]; then
    usage
    exit 64
fi

if [ ! -f "$ARCHIVE_PATH" ]; then
    echo "ERROR: archive not found: $ARCHIVE_PATH" >&2
    exit 2
fi

TMP_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/whale-pair-restore.XXXXXX")"
cleanup() {
    rm -rf "$TMP_ROOT"
}
trap cleanup EXIT

tar -xzf "$ARCHIVE_PATH" -C "$TMP_ROOT"
if [ ! -d "$TMP_ROOT/data" ]; then
    echo "ERROR: archive does not contain top-level data/ payload: $ARCHIVE_PATH" >&2
    exit 3
fi

PREVIOUS_DIR=""
if [ -d "$TARGET_DIR" ] && [ -n "$(find "$TARGET_DIR" -mindepth 1 -maxdepth 1 2>/dev/null | head -1)" ]; then
    if [ "$FORCE" -ne 1 ]; then
        echo "ERROR: target dir is non-empty: $TARGET_DIR (use --force to replace it)" >&2
        exit 4
    fi
    PREVIOUS_DIR="${TARGET_DIR}.pre-restore.$(date -u +'%Y%m%dT%H%M%SZ')"
    mv "$TARGET_DIR" "$PREVIOUS_DIR"
fi

mkdir -p "$TARGET_DIR"
(
    cd "$TMP_ROOT/data"
    tar -cf - .
) | (
    cd "$TARGET_DIR"
    tar -xf -
)

chmod 0750 "$TARGET_DIR" || true
find "$TARGET_DIR" -type f -name '*.db' -exec chmod 0640 {} + 2>/dev/null || true
mkdir -p "$TARGET_DIR/backups"

echo "Restore complete:"
echo "  archive: $ARCHIVE_PATH"
echo "  target : $TARGET_DIR"
if [ -n "$PREVIOUS_DIR" ]; then
    echo "  previous data moved to: $PREVIOUS_DIR"
fi
echo ""
echo "Next steps:"
echo "  1. Inspect restored ledgers with sqlite3"
echo "  2. Run standby bootstrap or start the service in dry-run"
echo "  3. Only promote to --live after health checks pass"
