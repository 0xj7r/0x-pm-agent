#!/usr/bin/env bash
# Install a cron entry that creates and replicates whale-pair backups.

set -euo pipefail

usage() {
    cat >&2 <<'EOF'
Usage: install_backup_replication_cron.sh --dest DEST [--schedule CRON] [--keep N] [--name LABEL]

Options:
  --dest DEST       rsync target or local directory for replicated backups
  --schedule CRON   cron schedule (default: 17 0 * * *)
  --keep N          number of local backups to retain (default: 14)
  --name LABEL      optional backup label for maintenance windows
EOF
}

REPO_ROOT="${REPO_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}"
DEST="${BACKUP_REPLICA_DEST:-}"
SCHEDULE="${BACKUP_REPLICATION_SCHEDULE:-17 0 * * *}"
KEEP="${BACKUP_KEEP:-14}"
LABEL=""

while [ $# -gt 0 ]; do
    case "$1" in
        --dest)
            DEST="${2:?missing value for --dest}"
            shift 2
            ;;
        --schedule)
            SCHEDULE="${2:?missing value for --schedule}"
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

if [ -z "$DEST" ]; then
    echo "ERROR: --dest is required" >&2
    exit 64
fi

LABEL_ARG=""
if [ -n "$LABEL" ]; then
    LABEL_ARG="--name $LABEL"
fi

CRON_CMD="cd $REPO_ROOT && BACKUP_REPLICA_DEST='$DEST' bash scripts/deploy/whale-pair/backup_data.sh --keep $KEEP $LABEL_ARG --replicate-hook $REPO_ROOT/scripts/deploy/whale-pair/replicate_backup.sh >> logs/whale-pair-backup.log 2>&1"
(
    crontab -l 2>/dev/null | grep -v 'scripts/deploy/whale-pair/backup_data.sh'
    printf '%s %s\n' "$SCHEDULE" "$CRON_CMD"
) | crontab -

echo "Installed whale-pair backup replication cron:"
echo "  schedule: $SCHEDULE"
echo "  dest    : $DEST"
echo "  keep    : $KEEP"
