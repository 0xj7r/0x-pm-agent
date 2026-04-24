#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
SYSTEMD_SRC="$ROOT_DIR/polymarket-exec/ops/systemd/polymarket-exec@.service"
LIVE_SMOKE_SERVICE_SRC="$ROOT_DIR/polymarket-exec/ops/systemd/polymarket-exec-live-smoke.service"
ARCHIVE_SERVICE_SRC="$ROOT_DIR/polymarket-exec/ops/systemd/polymarket-exec-archive.service"
ARCHIVE_TIMER_SRC="$ROOT_DIR/polymarket-exec/ops/systemd/polymarket-exec-archive.timer"
COMMON_ENV_SRC="$ROOT_DIR/polymarket-exec/ops/systemd/common.env.example"
SYSTEMD_USER_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/polymarket-exec"
PAPER_DIR="$CONFIG_DIR/paper.d"
LIVE_ENV="$CONFIG_DIR/live.env"

log() {
  echo "[install-user-paper-services] $1"
}

mkdir -p "$SYSTEMD_USER_DIR" "$PAPER_DIR"

install -m 0644 "$SYSTEMD_SRC" "$SYSTEMD_USER_DIR/polymarket-exec@.service"
log "installed systemd template -> $SYSTEMD_USER_DIR/polymarket-exec@.service"
install -m 0644 "$LIVE_SMOKE_SERVICE_SRC" "$SYSTEMD_USER_DIR/polymarket-exec-live-smoke.service"
log "installed live smoke service -> $SYSTEMD_USER_DIR/polymarket-exec-live-smoke.service"
install -m 0644 "$ARCHIVE_SERVICE_SRC" "$SYSTEMD_USER_DIR/polymarket-exec-archive.service"
log "installed archive service -> $SYSTEMD_USER_DIR/polymarket-exec-archive.service"
install -m 0644 "$ARCHIVE_TIMER_SRC" "$SYSTEMD_USER_DIR/polymarket-exec-archive.timer"
log "installed archive timer -> $SYSTEMD_USER_DIR/polymarket-exec-archive.timer"

if [[ ! -f "$CONFIG_DIR/common.env" ]]; then
  install -m 0644 "$COMMON_ENV_SRC" "$CONFIG_DIR/common.env"
  log "wrote common env template -> $CONFIG_DIR/common.env"
else
  log "leaving existing common env in place -> $CONFIG_DIR/common.env"
fi

if [[ ! -f "$LIVE_ENV" ]]; then
  cat >"$LIVE_ENV" <<'EOF'
# Host-local tiny-live / live-smoke secrets and overrides.
# Do not commit this file.
#
# Required before running polymarket-exec-live-smoke.service:
# POLYMARKET_PRIVATE_KEY=
# POLYMARKET_SIGNATURE_TYPE=gnosis_safe
# POLYMARKET_FUNDER_ADDRESS=
# POLYMARKET_API_KEY=
# POLYMARKET_API_SECRET=
# POLYMARKET_API_PASSPHRASE=
# WHALE_PAIR_ASSET_IDS=
# WHALE_PAIR_INSTRUMENT_MARKETS=
# WHALE_PAIR_LIVE_SMOKE_ASSET_ID=
# WHALE_PAIR_LIVE_SMOKE_MARKET_ID=
# WHALE_PAIR_LIVE_SMOKE_PRICE=0.01
# WHALE_PAIR_LIVE_SMOKE_NOTIONAL_USD=1.0
EOF
  chmod 0600 "$LIVE_ENV"
  log "wrote live env template -> $LIVE_ENV"
else
  log "leaving existing live env in place -> $LIVE_ENV"
fi

for sleeve in unlawful_baseline unlawful_broad_hours unlawful_press; do
  sleeve_env="$PAPER_DIR/$sleeve.env"
  if [[ ! -f "$sleeve_env" ]]; then
    cat >"$sleeve_env" <<EOF
# Optional host-local overrides for $sleeve.
# The repo preset at polymarket-exec/env/$sleeve.env is loaded automatically.
# Add only host-specific overrides here if needed.
EOF
    log "wrote empty sleeve override -> $sleeve_env"
  else
    log "leaving existing sleeve override in place -> $sleeve_env"
  fi
done

cat <<'EOF'

Next steps:
1. Review and edit ~/.config/polymarket-exec/common.env
2. Optional per-sleeve overrides live in ~/.config/polymarket-exec/paper.d/
3. Reload user services:
   systemctl --user daemon-reload
4. Enable lingering so paper keeps running after logout:
   sudo loginctl enable-linger "$USER"
5. Enable/start the unlawful sleeves:
   systemctl --user enable --now polymarket-exec@unlawful_baseline
   systemctl --user enable --now polymarket-exec@unlawful_broad_hours
   systemctl --user enable --now polymarket-exec@unlawful_press
6. Optional low-cost archive timer:
   systemctl --user enable --now polymarket-exec-archive.timer
7. Tiny-live smoke is one-shot and must be started manually:
   systemctl --user start polymarket-exec-live-smoke.service
EOF
