#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

REMOTE_HOST="${HETZNER_HOST:-${1:-}}"
REMOTE_USER="${HETZNER_USER:-root}"
REMOTE_PORT="${HETZNER_PORT:-22}"
REMOTE_KEY="${HETZNER_KEY_PATH:-$HOME/.ssh/polymarket_hetzner}"
REMOTE_ROOT="${HETZNER_REMOTE_ROOT:-/root/go/polymarket-agent}"
SLEEVES=(
  unlawful_baseline
  unlawful_broad_hours
  unlawful_press
)

log() {
  echo "[deploy-paper-hetzner] $1"
}

fail() {
  echo "[deploy-paper-hetzner] ERROR: $1" >&2
  exit 1
}

require_remote() {
  [[ -n "$REMOTE_HOST" ]] || fail "set HETZNER_HOST or pass <host> as the first argument"
  [[ -f "$REMOTE_KEY" ]] || fail "ssh key not found: $REMOTE_KEY"
}

ssh_base() {
  ssh \
    -o BatchMode=yes \
    -o StrictHostKeyChecking=no \
    -o ConnectTimeout=10 \
    -p "$REMOTE_PORT" \
    -i "$REMOTE_KEY" \
    "${REMOTE_USER}@${REMOTE_HOST}" \
    "$@"
}

rsync_base() {
  rsync -az --delete \
    -e "ssh -o BatchMode=yes -o StrictHostKeyChecking=no -o ConnectTimeout=10 -p $REMOTE_PORT -i $REMOTE_KEY" \
    "$@"
}

require_remote

log "probing ${REMOTE_USER}@${REMOTE_HOST}:${REMOTE_PORT}"
ssh_base 'hostname && whoami && uname -a'

log "creating remote checkout root: $REMOTE_ROOT"
ssh_base "mkdir -p '$REMOTE_ROOT'"

log "syncing repo without local artifacts"
rsync_base \
  --exclude '.git/' \
  --exclude '.venv/' \
  --exclude 'target/' \
  --exclude 'reports/' \
  --exclude 'data/' \
  --exclude 'polymarket-exec/data/' \
  --exclude 'polymarket-exec/.env' \
  --exclude '*.sqlite-shm' \
  --exclude '*.sqlite-wal' \
  "$ROOT_DIR/" "${REMOTE_USER}@${REMOTE_HOST}:$REMOTE_ROOT/"

log "verifying remote toolchain"
ssh_base "
  command -v bash >/dev/null &&
  command -v python3 >/dev/null &&
  command -v cargo >/dev/null &&
  command -v systemctl >/dev/null
"

log "installing user service templates"
ssh_base "cd '$REMOTE_ROOT' && polymarket-exec/ops/systemd/install_user_paper_services.sh"

log "reloading user systemd"
ssh_base "systemctl --user daemon-reload"

log "enabling linger for ${REMOTE_USER}"
ssh_base "loginctl enable-linger '${REMOTE_USER}' || true"

for sleeve in "${SLEEVES[@]}"; do
  log "restarting $sleeve"
  ssh_base "systemctl --user enable --now polymarket-exec@${sleeve}"
done

log "paper services status"
ssh_base "cd '$REMOTE_ROOT' && polymarket-exec/ops/systemd/manage_unlawful_paper_services.sh status || true"

log "health checks"
ssh_base "
  curl -fsS http://127.0.0.1:9108/healthz &&
  curl -fsS http://127.0.0.1:9109/healthz &&
  curl -fsS http://127.0.0.1:9110/healthz
"

cat <<EOF

Deploy complete.

Remote root:
  $REMOTE_ROOT

Useful follow-ups:
  ssh -i $REMOTE_KEY -p $REMOTE_PORT ${REMOTE_USER}@${REMOTE_HOST} \\
    'cd $REMOTE_ROOT && polymarket-exec/ops/systemd/manage_unlawful_paper_services.sh logs'

  ssh -i $REMOTE_KEY -p $REMOTE_PORT ${REMOTE_USER}@${REMOTE_HOST} \\
    'cat ~/.config/polymarket-exec/common.env'
EOF
