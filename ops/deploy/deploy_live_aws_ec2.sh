#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

REMOTE_HOST="${AWS_LIVE_HOST:-${1:-}}"
REMOTE_USER="${AWS_LIVE_USER:-ubuntu}"
REMOTE_PORT="${AWS_LIVE_PORT:-22}"
REMOTE_KEY="${AWS_LIVE_KEY_PATH:-$HOME/.ssh/polymarket_aws_live}"
REMOTE_ROOT="${AWS_LIVE_REMOTE_ROOT:-/home/$REMOTE_USER/go/polymarket-agent}"
AWS_REGION="${AWS_REGION:-eu-west-1}"

log() {
  echo "[deploy-live-aws-ec2] $1"
}

fail() {
  echo "[deploy-live-aws-ec2] ERROR: $1" >&2
  exit 1
}

require_remote() {
  [[ -n "$REMOTE_HOST" ]] || fail "set AWS_LIVE_HOST or pass <host> as the first argument"
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

log "target region hint: $AWS_REGION"
log "probing ${REMOTE_USER}@${REMOTE_HOST}:${REMOTE_PORT}"
ssh_base 'hostname && whoami && uname -a'

log "creating remote checkout root: $REMOTE_ROOT"
ssh_base "mkdir -p '$REMOTE_ROOT'"

log "syncing repo without local artifacts or secrets"
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
  (command -v cargo >/dev/null || test -x \"\$HOME/.cargo/bin/cargo\") &&
  command -v systemctl >/dev/null
"

log "installing user service templates"
ssh_base "cd '$REMOTE_ROOT' && polymarket-exec/ops/systemd/install_user_paper_services.sh"

log "building single release binary"
ssh_base "cd '$REMOTE_ROOT' && CARGO_BIN=\$(command -v cargo || printf '%s/.cargo/bin/cargo' \"\$HOME\") && \"\$CARGO_BIN\" build --release -p polymarket-exec && mkdir -p \"\$HOME/.local/bin\" && install -m 0755 target/release/polymarket-exec \"\$HOME/.local/bin/polymarket-exec\" && test -x \"\$HOME/.local/bin/polymarket-exec\" && rm -rf target"

log "reloading user systemd"
ssh_base "systemctl --user daemon-reload"

log "enabling linger for ${REMOTE_USER}"
ssh_base "sudo loginctl enable-linger '${REMOTE_USER}' || true"

log "installed but did not start live smoke"
ssh_base "systemctl --user status polymarket-exec-live-smoke.service --no-pager || true"

cat <<EOF

Deploy complete.

Remote root:
  $REMOTE_ROOT

Tinylive env to edit on the AWS host:
  ~/.config/polymarket-exec/btc_5m_mm_tinylive.env

Operator kill switch:
  touch ~/.config/polymarket-exec/live.kill
  rm -f ~/.config/polymarket-exec/live.kill

Run smoke manually after secrets and market ids are configured:
  systemctl --user start polymarket-exec-live-smoke.service
  journalctl --user -u polymarket-exec-live-smoke.service -n 200 -f
EOF
