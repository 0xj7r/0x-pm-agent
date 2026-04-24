#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
SYSTEMD_SRC="$ROOT_DIR/whale-pair-exec/ops/systemd/whale-pair-exec@.service"
ARCHIVE_SERVICE_SRC="$ROOT_DIR/whale-pair-exec/ops/systemd/whale-pair-archive.service"
ARCHIVE_TIMER_SRC="$ROOT_DIR/whale-pair-exec/ops/systemd/whale-pair-archive.timer"
COMMON_ENV_SRC="$ROOT_DIR/whale-pair-exec/ops/systemd/common.env.example"
SYSTEMD_USER_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/whale-pair-exec"
PAPER_DIR="$CONFIG_DIR/paper.d"

log() {
  echo "[install-user-paper-services] $1"
}

mkdir -p "$SYSTEMD_USER_DIR" "$PAPER_DIR"

install -m 0644 "$SYSTEMD_SRC" "$SYSTEMD_USER_DIR/whale-pair-exec@.service"
log "installed systemd template -> $SYSTEMD_USER_DIR/whale-pair-exec@.service"
install -m 0644 "$ARCHIVE_SERVICE_SRC" "$SYSTEMD_USER_DIR/whale-pair-archive.service"
log "installed archive service -> $SYSTEMD_USER_DIR/whale-pair-archive.service"
install -m 0644 "$ARCHIVE_TIMER_SRC" "$SYSTEMD_USER_DIR/whale-pair-archive.timer"
log "installed archive timer -> $SYSTEMD_USER_DIR/whale-pair-archive.timer"

if [[ ! -f "$CONFIG_DIR/common.env" ]]; then
  install -m 0644 "$COMMON_ENV_SRC" "$CONFIG_DIR/common.env"
  log "wrote common env template -> $CONFIG_DIR/common.env"
else
  log "leaving existing common env in place -> $CONFIG_DIR/common.env"
fi

for sleeve in unlawful_baseline unlawful_broad_hours unlawful_press; do
  sleeve_env="$PAPER_DIR/$sleeve.env"
  if [[ ! -f "$sleeve_env" ]]; then
    cat >"$sleeve_env" <<EOF
# Optional host-local overrides for $sleeve.
# The repo preset at whale-pair-exec/env/$sleeve.env is loaded automatically.
# Add only host-specific overrides here if needed.
EOF
    log "wrote empty sleeve override -> $sleeve_env"
  else
    log "leaving existing sleeve override in place -> $sleeve_env"
  fi
done

cat <<'EOF'

Next steps:
1. Review and edit ~/.config/whale-pair-exec/common.env
2. Optional per-sleeve overrides live in ~/.config/whale-pair-exec/paper.d/
3. Reload user services:
   systemctl --user daemon-reload
4. Enable lingering so paper keeps running after logout:
   sudo loginctl enable-linger "$USER"
5. Enable/start the unlawful sleeves:
   systemctl --user enable --now whale-pair-exec@unlawful_baseline
   systemctl --user enable --now whale-pair-exec@unlawful_broad_hours
   systemctl --user enable --now whale-pair-exec@unlawful_press
6. Optional low-cost archive timer:
   systemctl --user enable --now whale-pair-archive.timer
EOF
