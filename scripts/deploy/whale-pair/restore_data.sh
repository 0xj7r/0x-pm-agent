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
       restore_data.sh <archive.tar.gz> --inspect

Options:
  --latest          restore the newest backup archive from the backup dir
  --backup-dir DIR  directory containing backup archives
  --target DIR      restore destination (default: $REPO_ROOT/data)
  --force           move aside an existing non-empty target before restore
  --inspect         print archive/manifest/checksum metadata and exit
EOF
}

REPO_ROOT="${REPO_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}"
TARGET_DIR="${TARGET_DIR:-$REPO_ROOT/data}"
BACKUP_DIR="${BACKUP_DIR:-$TARGET_DIR/backups}"
ARCHIVE_PATH=""
RESTORE_LATEST=0
FORCE=0
INSPECT_ONLY=0

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
        --inspect)
            INSPECT_ONLY=1
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

CHECKSUM_PATH="${ARCHIVE_PATH%.tar.gz}.sha256"
MANIFEST_PATH="${ARCHIVE_PATH%.tar.gz}.manifest.json"

if [ -f "$CHECKSUM_PATH" ]; then
    if command -v sha256sum >/dev/null 2>&1; then
        (cd "$(dirname "$ARCHIVE_PATH")" && sha256sum -c "$(basename "$CHECKSUM_PATH")") >/dev/null
    elif command -v shasum >/dev/null 2>&1; then
        expected="$(awk '{print $1}' "$CHECKSUM_PATH")"
        actual="$(shasum -a 256 "$ARCHIVE_PATH" | awk '{print $1}')"
        if [ "$expected" != "$actual" ]; then
            echo "ERROR: checksum verification failed for $ARCHIVE_PATH" >&2
            exit 5
        fi
    fi
fi

if [ "$INSPECT_ONLY" -eq 1 ]; then
    echo "archive=$ARCHIVE_PATH"
    if [ -f "$MANIFEST_PATH" ]; then
        echo "manifest=$MANIFEST_PATH"
        cat "$MANIFEST_PATH"
    else
        echo "manifest=missing"
    fi
    if [ -f "$CHECKSUM_PATH" ]; then
        echo "checksum=$CHECKSUM_PATH"
        cat "$CHECKSUM_PATH"
    else
        echo "checksum=missing"
    fi
    exit 0
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

RESTORE_META="$TARGET_DIR/whale_pair_restore.meta"
cat >"$RESTORE_META" <<EOF
restored_at_utc=$(date -u +'%Y-%m-%dT%H:%M:%SZ')
archive_path=$ARCHIVE_PATH
archive_basename=$(basename "$ARCHIVE_PATH")
checksum_path=$CHECKSUM_PATH
checksum_present=$([ -f "$CHECKSUM_PATH" ] && echo yes || echo no)
manifest_path=$MANIFEST_PATH
manifest_present=$([ -f "$MANIFEST_PATH" ] && echo yes || echo no)
target_dir=$TARGET_DIR
previous_dir=${PREVIOUS_DIR:-}
EOF
chmod 0640 "$RESTORE_META" || true

echo "Restore complete:"
echo "  archive: $ARCHIVE_PATH"
echo "  target : $TARGET_DIR"
echo "  meta   : $RESTORE_META"
if [ -n "$PREVIOUS_DIR" ]; then
    echo "  previous data moved to: $PREVIOUS_DIR"
fi
echo ""
echo "Next steps:"
echo "  1. Inspect restored ledgers with sqlite3"
echo "  2. Review $RESTORE_META and confirm the restored archive is the intended one"
echo "  3. Run standby bootstrap or start the service in dry-run"
echo "  4. Only promote to --live after health checks pass"
