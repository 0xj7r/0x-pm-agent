#!/usr/bin/env bash
# Promote a passive whale-pair standby to live trading.

set -euo pipefail

usage() {
    cat >&2 <<'EOF'
Usage: promote_standby.sh --confirm-primary-stopped [--reason TEXT] [--skip-health-check] [--allow-stale-restore] [--dry-run]

Options:
  --confirm-primary-stopped  explicit operator acknowledgement that the primary is down or isolated
  --reason TEXT              incident/failover note recorded in the standby role file
  --skip-health-check        bypass health.sh during readiness checks
  --allow-stale-restore      allow promotion when the restored archive is not the newest local backup
  --dry-run                  validate guardrails only; do not call start.sh --live
EOF
}

REPO_ROOT="${REPO_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}"
DATA_DIR="${DATA_DIR:-$REPO_ROOT/data}"
ROLE_FILE="${ROLE_FILE:-$DATA_DIR/whale_pair_standby.role}"
RESTORE_META="${RESTORE_META:-$DATA_DIR/whale_pair_restore.meta}"
BACKUP_DIR="${BACKUP_DIR:-$DATA_DIR/backups}"
CONFIRM_PRIMARY_STOPPED=0
SKIP_HEALTH_CHECK=0
ALLOW_STALE_RESTORE=0
DRY_RUN=0
REASON="${FAILOVER_REASON:-manual-failover}"

while [ $# -gt 0 ]; do
    case "$1" in
        --confirm-primary-stopped)
            CONFIRM_PRIMARY_STOPPED=1
            shift
            ;;
        --reason)
            REASON="${2:?missing value for --reason}"
            shift 2
            ;;
        --skip-health-check)
            SKIP_HEALTH_CHECK=1
            shift
            ;;
        --allow-stale-restore)
            ALLOW_STALE_RESTORE=1
            shift
            ;;
        --dry-run)
            DRY_RUN=1
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

if [ "$CONFIRM_PRIMARY_STOPPED" -ne 1 ]; then
    echo "ERROR: promotion requires --confirm-primary-stopped to avoid split-brain live trading" >&2
    exit 64
fi

if [ ! -f "$ROLE_FILE" ]; then
    echo "ERROR: standby role file missing: $ROLE_FILE" >&2
    exit 2
fi

ROLE_VALUE="$(grep -E '^role=' "$ROLE_FILE" | head -1 | cut -d= -f2-)"
if [ "$ROLE_VALUE" != "passive-standby" ]; then
    echo "ERROR: role file does not describe a passive standby (role=$ROLE_VALUE)" >&2
    exit 3
fi

STATUS_ARGS=(--assert-ready)
if [ "$SKIP_HEALTH_CHECK" -eq 1 ]; then
    STATUS_ARGS+=(--skip-health-check)
fi
if ! bash "$REPO_ROOT/scripts/deploy/whale-pair/standby_status.sh" "${STATUS_ARGS[@]}"; then
    echo "ERROR: standby readiness checks failed" >&2
    exit 4
fi

if [ "$ALLOW_STALE_RESTORE" -ne 1 ] && [ -f "$RESTORE_META" ]; then
    RESTORED_ARCHIVE="$(grep -E '^archive_basename=' "$RESTORE_META" | head -1 | cut -d= -f2-)"
    LATEST_BACKUP="$(find "$BACKUP_DIR" -maxdepth 1 -type f -name 'whale-pair-data-*.tar.gz' | sort | tail -1 || true)"
    if [ -n "$RESTORED_ARCHIVE" ] && [ -n "$LATEST_BACKUP" ] && [ "$(basename "$LATEST_BACKUP")" != "$RESTORED_ARCHIVE" ]; then
        echo "ERROR: restored archive $RESTORED_ARCHIVE is not the newest local backup $(basename "$LATEST_BACKUP")" >&2
        echo "       rerun standby_bootstrap.sh --restore-latest or pass --allow-stale-restore deliberately" >&2
        exit 5
    fi
fi

if [ "$DRY_RUN" -eq 1 ]; then
    echo "Standby promotion dry-run passed."
    echo "Role file  : $ROLE_FILE"
    echo "Restore meta: $RESTORE_META"
    echo "Reason     : $REASON"
    exit 0
fi

WHALE_PAIR_ALLOW_STANDBY_PROMOTION=1 \
    bash "$REPO_ROOT/scripts/deploy/whale-pair/start.sh" --live

PRIMARY_LABEL="$(grep -E '^primary=' "$ROLE_FILE" | head -1 | cut -d= -f2-)"
BOOTSTRAPPED_AT="$(grep -E '^bootstrapped_at_utc=' "$ROLE_FILE" | head -1 | cut -d= -f2-)"
RESTORED_ARCHIVE="$(grep -E '^archive_basename=' "$RESTORE_META" 2>/dev/null | head -1 | cut -d= -f2-)"
cat >"$ROLE_FILE" <<EOF
role=promoted-primary
previous_role=passive-standby
primary=${PRIMARY_LABEL:-unknown-primary}
bootstrapped_at_utc=${BOOTSTRAPPED_AT:-unknown}
promoted_at_utc=$(date -u +'%Y-%m-%dT%H:%M:%SZ')
promotion_reason=$REASON
restore_meta=$RESTORE_META
restored_archive=${RESTORED_ARCHIVE:-}
EOF
chmod 0640 "$ROLE_FILE" || true

echo "Standby promoted and role file updated: $ROLE_FILE"
