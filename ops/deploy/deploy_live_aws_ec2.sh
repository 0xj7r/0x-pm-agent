#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

REMOTE_HOST="${AWS_LIVE_HOST:-${1:-}}"
REMOTE_USER="${AWS_LIVE_USER:-ubuntu}"
REMOTE_PORT="${AWS_LIVE_PORT:-22}"
REMOTE_KEY="${AWS_LIVE_KEY_PATH:-$HOME/.ssh/polymarket_aws_live}"
REMOTE_RELEASE_ROOT="${AWS_LIVE_RELEASE_ROOT:-/home/$REMOTE_USER/go/polymarket-agent-releases}"
REMOTE_CARGO_TARGET_DIR="${AWS_LIVE_CARGO_TARGET_DIR:-/home/$REMOTE_USER/.cache/polymarket-agent-cargo-target}"
AWS_REGION="${AWS_REGION:-eu-west-1}"
DEPLOY_REF="${AWS_LIVE_REF:-origin/main}"
SKIP_FETCH="${AWS_LIVE_SKIP_FETCH:-0}"
SKIP_BUILD="${AWS_LIVE_SKIP_BUILD:-0}"
SKIP_RESTART="${AWS_LIVE_SKIP_RESTART:-1}"
RESTART_CMD="${AWS_LIVE_RESTART_CMD:-}"

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
  rsync -az \
    -e "ssh -o BatchMode=yes -o StrictHostKeyChecking=no -o ConnectTimeout=10 -p $REMOTE_PORT -i $REMOTE_KEY" \
    "$@"
}

require_remote

cd "$ROOT_DIR"
if [[ "$SKIP_FETCH" != "1" && "$DEPLOY_REF" == origin/* ]]; then
  log "fetching origin before resolving $DEPLOY_REF"
  git fetch origin
fi

git rev-parse --verify "$DEPLOY_REF^{commit}" >/dev/null ||
  fail "deploy ref does not resolve to a commit: $DEPLOY_REF"
DEPLOY_COMMIT="$(git rev-parse "$DEPLOY_REF^{commit}")"
DEPLOY_SHORT="${DEPLOY_COMMIT:0:7}"
ARCHIVE_PATH="$(mktemp "/tmp/polymarket-agent-${DEPLOY_SHORT}.XXXXXX.tar")"
trap 'rm -f "$ARCHIVE_PATH"' EXIT

log "deploy ref: $DEPLOY_REF ($DEPLOY_COMMIT)"
log "creating clean git archive; local dirty worktree is intentionally ignored"
git archive --format=tar --prefix="polymarket-agent-${DEPLOY_SHORT}/" "$DEPLOY_COMMIT" -o "$ARCHIVE_PATH"

log "target region hint: $AWS_REGION"
log "probing ${REMOTE_USER}@${REMOTE_HOST}:${REMOTE_PORT}"
ssh_base 'hostname && whoami && uname -a'

log "creating remote release root: $REMOTE_RELEASE_ROOT"
ssh_base "mkdir -p '$REMOTE_RELEASE_ROOT' '$REMOTE_CARGO_TARGET_DIR' \"\$HOME/.local/bin\""

log "uploading clean source archive"
rsync_base "$ARCHIVE_PATH" "${REMOTE_USER}@${REMOTE_HOST}:/tmp/polymarket-agent-${DEPLOY_SHORT}.tar"

log "extracting release $DEPLOY_SHORT"
ssh_base "
  set -euo pipefail
  rm -rf '$REMOTE_RELEASE_ROOT/polymarket-agent-${DEPLOY_SHORT}'
  tar -xf '/tmp/polymarket-agent-${DEPLOY_SHORT}.tar' -C '$REMOTE_RELEASE_ROOT'
"

REMOTE_RELEASE_DIR="$REMOTE_RELEASE_ROOT/polymarket-agent-${DEPLOY_SHORT}"

log "verifying remote toolchain"
ssh_base "
  command -v bash >/dev/null &&
  command -v python3 >/dev/null &&
  (command -v cargo >/dev/null || test -x \"\$HOME/.cargo/bin/cargo\") &&
  command -v systemctl >/dev/null
"

log "installing user service templates"
ssh_base "
  set -euo pipefail
  cd '$REMOTE_RELEASE_DIR'
  mkdir -p \"\$HOME/.config/systemd/user\"
  for unit in polymarket-exec/ops/systemd/*.service polymarket-exec/ops/systemd/*.timer; do
    [ -e \"\$unit\" ] || continue
    install -m 0644 \"\$unit\" \"\$HOME/.config/systemd/user/\$(basename \"\$unit\")\"
  done
"

if [[ "$SKIP_BUILD" == "1" ]]; then
  log "skipping build (AWS_LIVE_SKIP_BUILD=1)"
else
  log "building release binary with persistent Cargo target cache"
  ssh_base "
    set -euo pipefail
    cd '$REMOTE_RELEASE_DIR'
    CARGO_BIN=\$(command -v cargo || printf '%s/.cargo/bin/cargo' \"\$HOME\")
    CARGO_TARGET_DIR='$REMOTE_CARGO_TARGET_DIR' \"\$CARGO_BIN\" build --release -p polymarket-exec --bin polymarket-exec
    install -m 0755 '$REMOTE_CARGO_TARGET_DIR/release/polymarket-exec' \"\$HOME/.local/bin/polymarket-exec-${DEPLOY_SHORT}\"
    ln -sfn \"\$HOME/.local/bin/polymarket-exec-${DEPLOY_SHORT}\" \"\$HOME/.local/bin/polymarket-exec\"
    test -x \"\$HOME/.local/bin/polymarket-exec-${DEPLOY_SHORT}\"
  "
fi

log "reloading user systemd"
ssh_base "systemctl --user daemon-reload"

log "enabling linger for ${REMOTE_USER}"
ssh_base "sudo loginctl enable-linger '${REMOTE_USER}' || true"

log "installed but did not start live smoke"
ssh_base "systemctl --user status polymarket-exec-live-smoke.service --no-pager || true"

if [[ "$SKIP_RESTART" == "1" ]]; then
  log "skipping safe-restart (AWS_LIVE_SKIP_RESTART=1)"
elif [[ -n "$RESTART_CMD" ]]; then
  log "running custom restart command"
  ssh_base "$RESTART_CMD"
elif ssh_base "test -x \$HOME/.local/bin/poly-safe-restart.sh" 2>/dev/null; then
  log "running poly-safe-restart for ${AWS_LIVE_SLEEVE:-btc_5m_paired_mm_tinylive}"
  ssh_base "\$HOME/.local/bin/poly-safe-restart.sh ${AWS_LIVE_SLEEVE:-btc_5m_paired_mm_tinylive}"
else
  log "poly-safe-restart.sh not found on remote; skipping (manual restart required)"
fi

cat <<EOF

Deploy complete.

Remote root:
  $REMOTE_RELEASE_DIR

Deployed ref:
  $DEPLOY_REF ($DEPLOY_COMMIT)

Persistent remote Cargo target dir:
  $REMOTE_CARGO_TARGET_DIR

Tinylive envs to edit on the AWS host:
  paired-MM live:      ~/.config/polymarket-exec/btc_5m_paired_mm_tinylive.env
  pair-cost/hybrid:    ~/.config/polymarket-exec/btc_5m_hybrid_tinylive.env

Operator kill switch:
  touch ~/.config/polymarket-exec/live.kill
  rm -f ~/.config/polymarket-exec/live.kill

Run smoke manually after secrets and market ids are configured:
  systemctl --user start polymarket-exec-live-smoke.service
  journalctl --user -u polymarket-exec-live-smoke.service -n 200 -f
EOF
