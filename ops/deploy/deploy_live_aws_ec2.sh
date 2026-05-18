#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

REMOTE_HOST="${AWS_LIVE_HOST:-${1:-}}"
REMOTE_USER="${AWS_LIVE_USER:-ubuntu}"
REMOTE_PORT="${AWS_LIVE_PORT:-22}"
REMOTE_KEY="${AWS_LIVE_KEY_PATH:-$HOME/.ssh/polymarket_aws_live}"
REMOTE_RELEASE_ROOT="${AWS_LIVE_RELEASE_ROOT:-/home/$REMOTE_USER/go/polymarket-agent-releases}"
REMOTE_WORKDIR="${AWS_LIVE_WORKDIR:-/home/$REMOTE_USER/go/polymarket-agent}"
REMOTE_CARGO_TARGET_DIR="${AWS_LIVE_CARGO_TARGET_DIR:-/home/$REMOTE_USER/.cache/polymarket-agent-cargo-target}"
AWS_REGION="${AWS_REGION:-eu-west-1}"
DEPLOY_REF="${AWS_LIVE_REF:-origin/main}"
LIVE_SLEEVE="${AWS_LIVE_SLEEVE:-}"
SKIP_FETCH="${AWS_LIVE_SKIP_FETCH:-0}"
SKIP_BUILD="${AWS_LIVE_SKIP_BUILD:-0}"
SKIP_RESTART="${AWS_LIVE_SKIP_RESTART:-1}"
RESTART_CMD="${AWS_LIVE_RESTART_CMD:-}"
WAIT_FOR_ROLLOVER="${AWS_LIVE_WAIT_FOR_ROLLOVER:-1}"
ROLLOVER_SAFE_SECONDS="${AWS_LIVE_ROLLOVER_SAFE_SECONDS:-12}"
ROLLOVER_MAX_WAIT_SECONDS="${AWS_LIVE_ROLLOVER_MAX_WAIT_SECONDS:-330}"

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
REMOTE_BINARY_NAME="${AWS_LIVE_BINARY_NAME:-polymarket-exec-${LIVE_SLEEVE:-live}-${DEPLOY_SHORT}}"
REMOTE_SERVICE_UNIT="${AWS_LIVE_SERVICE_UNIT:-polymarket-exec@${LIVE_SLEEVE}.service}"
ARCHIVE_PATH="$(mktemp "/tmp/polymarket-agent-${DEPLOY_SHORT}.XXXXXX.tar")"
trap 'rm -f "$ARCHIVE_PATH"' EXIT

log "deploy ref: $DEPLOY_REF ($DEPLOY_COMMIT)"
log "creating clean git archive; local dirty worktree is intentionally ignored"
git archive --format=tar --prefix="polymarket-agent-${DEPLOY_SHORT}/" "$DEPLOY_COMMIT" -o "$ARCHIVE_PATH"

log "target region hint: $AWS_REGION"
log "probing ${REMOTE_USER}@${REMOTE_HOST}:${REMOTE_PORT}"
ssh_base 'hostname && whoami && uname -a'

log "creating remote release root: $REMOTE_RELEASE_ROOT"
ssh_base "mkdir -p '$REMOTE_RELEASE_ROOT' '$REMOTE_WORKDIR' '$REMOTE_CARGO_TARGET_DIR' \"\$HOME/.local/bin\""

log "uploading clean source archive"
rsync_base "$ARCHIVE_PATH" "${REMOTE_USER}@${REMOTE_HOST}:/tmp/polymarket-agent-${DEPLOY_SHORT}.tar"

log "extracting release $DEPLOY_SHORT"
ssh_base "
  set -euo pipefail
  rm -rf '$REMOTE_RELEASE_ROOT/polymarket-agent-${DEPLOY_SHORT}'
  tar -xf '/tmp/polymarket-agent-${DEPLOY_SHORT}.tar' -C '$REMOTE_RELEASE_ROOT'
"

REMOTE_RELEASE_DIR="$REMOTE_RELEASE_ROOT/polymarket-agent-${DEPLOY_SHORT}"

log "syncing clean release source into runtime working directory: $REMOTE_WORKDIR"
ssh_base "
  set -euo pipefail
  command -v rsync >/dev/null
  rsync -a --delete \
    --exclude '.git' \
    --exclude '.venv/' \
    --exclude 'data/' \
    --exclude 'target/' \
    '$REMOTE_RELEASE_DIR/' '$REMOTE_WORKDIR/'
"

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
    export CARGO_TARGET_DIR='$REMOTE_CARGO_TARGET_DIR'
    bash -lc 'cargo build --release -p polymarket-exec --bin polymarket-exec'
    install -m 0755 '$REMOTE_CARGO_TARGET_DIR/release/polymarket-exec' \"\$HOME/.local/bin/$REMOTE_BINARY_NAME\"
    ln -sfn \"\$HOME/.local/bin/$REMOTE_BINARY_NAME\" \"\$HOME/.local/bin/polymarket-exec\"
    printf '%s' '$DEPLOY_COMMIT' > '$REMOTE_WORKDIR/.deploy_commit'
    date -u +%FT%TZ > '$REMOTE_WORKDIR/.last_binary_restart_utc'
    test -x \"\$HOME/.local/bin/$REMOTE_BINARY_NAME\"
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
else
  if [[ "$WAIT_FOR_ROLLOVER" == "1" ]]; then
    log "waiting for start of next 5-minute BTC bar before restart"
    ssh_base "
      set -euo pipefail
      safe_seconds='$ROLLOVER_SAFE_SECONDS'
      max_wait_seconds='$ROLLOVER_MAX_WAIT_SECONDS'
      deadline=\$(( \$(date +%s) + max_wait_seconds ))
      while true; do
        now=\$(date +%s)
        into_bar=\$(( now % 300 ))
        if [ \"\$into_bar\" -le \"\$safe_seconds\" ]; then
          echo \"restart window open: into_bar=\${into_bar}s safe_seconds=\${safe_seconds}s\"
          break
        fi
        if [ \"\$now\" -ge \"\$deadline\" ]; then
          echo \"timed out waiting for rollover; continuing restart after \${max_wait_seconds}s\" >&2
          break
        fi
        sleep_for=\$(( 300 - into_bar + 1 ))
        remaining=\$(( deadline - now ))
        if [ \"\$sleep_for\" -gt \"\$remaining\" ]; then
          sleep_for=\"\$remaining\"
        fi
        echo \"waiting \${sleep_for}s for 5-minute rollover; into_bar=\${into_bar}s\"
        sleep \"\$sleep_for\"
      done
    "
  fi

  if [[ -n "$RESTART_CMD" ]]; then
    log "running custom restart command"
    ssh_base "$RESTART_CMD"
  elif [[ -n "$LIVE_SLEEVE" ]]; then
    log "restarting service unit: $REMOTE_SERVICE_UNIT"
    ssh_base "systemctl --user restart '$REMOTE_SERVICE_UNIT'"
  elif ssh_base "test -x \$HOME/.local/bin/poly-safe-restart.sh" 2>/dev/null; then
    log "running poly-safe-restart for btc_5m_paired_mm_tinylive"
    ssh_base "\$HOME/.local/bin/poly-safe-restart.sh btc_5m_paired_mm_tinylive"
  else
    log "poly-safe-restart.sh not found on remote; skipping (manual restart required)"
  fi
fi

cat <<EOF

Deploy complete.

Remote root:
  $REMOTE_RELEASE_DIR

Runtime working directory:
  $REMOTE_WORKDIR

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
