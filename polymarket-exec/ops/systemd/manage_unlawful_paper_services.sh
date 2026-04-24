#!/usr/bin/env bash
set -euo pipefail

ACTION="${1:-status}"
SERVICES=(
  whale-pair-exec@unlawful_baseline
  whale-pair-exec@unlawful_broad_hours
  whale-pair-exec@unlawful_press
)

usage() {
  cat <<'EOF'
Usage:
  manage_unlawful_paper_services.sh <status|start|stop|restart|enable|disable|logs>

Examples:
  manage_unlawful_paper_services.sh status
  manage_unlawful_paper_services.sh restart
  manage_unlawful_paper_services.sh logs
EOF
}

case "$ACTION" in
  status)
    systemctl --user status "${SERVICES[@]}"
    ;;
  start|stop|restart|enable|disable)
    systemctl --user "$ACTION" "${SERVICES[@]}"
    ;;
  logs)
    journalctl --user -u "${SERVICES[0]}" -u "${SERVICES[1]}" -u "${SERVICES[2]}" -f
    ;;
  *)
    usage
    exit 1
    ;;
esac
