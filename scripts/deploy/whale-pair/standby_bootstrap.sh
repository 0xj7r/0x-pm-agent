#!/usr/bin/env bash
# Bootstrap or refresh a passive whale-pair standby node.
#
# This script is intended to run on the standby host after the repo checkout
# and .env are already present. It restores the latest ledger backup when
# requested, writes a local role marker, and starts the bot in dry-run mode.

set -euo pipefail

usage() {
    cat >&2 <<'EOF'
Usage: standby_bootstrap.sh [--restore-archive PATH | --restore-latest] [--primary LABEL] [--no-start] [--skip-env-check]

Options:
  --restore-archive PATH  restore a specific backup archive before startup
  --restore-latest        restore the newest archive from $BACKUP_DIR
  --primary LABEL         record the primary host/role this standby mirrors
  --no-start              do not call start.sh --dry-run
  --skip-env-check        skip check_env.sh validation
EOF
}

REPO_ROOT="${REPO_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}"
DATA_DIR="${DATA_DIR:-$REPO_ROOT/data}"
BACKUP_DIR="${BACKUP_DIR:-$DATA_DIR/backups}"
ENV_FILE="${ENV_FILE:-$REPO_ROOT/.env}"
RESTORE_ARCHIVE=""
RESTORE_LATEST=0
NO_START=0
SKIP_ENV_CHECK=0
PRIMARY_LABEL="${PRIMARY_LABEL:-unknown-primary}"

while [ $# -gt 0 ]; do
    case "$1" in
        --restore-archive)
            RESTORE_ARCHIVE="${2:?missing value for --restore-archive}"
            shift 2
            ;;
        --restore-latest)
            RESTORE_LATEST=1
            shift
            ;;
        --primary)
            PRIMARY_LABEL="${2:?missing value for --primary}"
            shift 2
            ;;
        --no-start)
            NO_START=1
            shift
            ;;
        --skip-env-check)
            SKIP_ENV_CHECK=1
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

if [ -n "$RESTORE_ARCHIVE" ] && [ "$RESTORE_LATEST" -eq 1 ]; then
    echo "ERROR: specify either --restore-archive or --restore-latest" >&2
    exit 64
fi

mkdir -p "$DATA_DIR" "$BACKUP_DIR" "$REPO_ROOT/logs"

if [ "$SKIP_ENV_CHECK" -ne 1 ]; then
    bash "$REPO_ROOT/scripts/deploy/whale-pair/check_env.sh" "$ENV_FILE"
fi

if [ -n "$RESTORE_ARCHIVE" ]; then
    bash "$REPO_ROOT/scripts/deploy/whale-pair/restore_data.sh" \
        "$RESTORE_ARCHIVE" \
        --target "$DATA_DIR" \
        --force
elif [ "$RESTORE_LATEST" -eq 1 ]; then
    bash "$REPO_ROOT/scripts/deploy/whale-pair/restore_data.sh" \
        --latest \
        --backup-dir "$BACKUP_DIR" \
        --target "$DATA_DIR" \
        --force
fi

ROLE_FILE="$DATA_DIR/whale_pair_standby.role"
cat >"$ROLE_FILE" <<EOF
role=passive-standby
primary=$PRIMARY_LABEL
bootstrapped_at_utc=$(date -u +'%Y-%m-%dT%H:%M:%SZ')
repo_root=$REPO_ROOT
data_dir=$DATA_DIR
EOF
chmod 0640 "$ROLE_FILE" || true

echo "Standby role marker written: $ROLE_FILE"

if [ "$NO_START" -eq 0 ]; then
    bash "$REPO_ROOT/scripts/deploy/whale-pair/start.sh" --dry-run
else
    echo "Standby bootstrap complete. Dry-run start skipped (--no-start)."
fi

echo ""
echo "Recommended follow-up commands:"
echo "  bash $REPO_ROOT/scripts/deploy/whale-pair/standby_status.sh"
echo "  bash $REPO_ROOT/scripts/deploy/whale-pair/health.sh"
