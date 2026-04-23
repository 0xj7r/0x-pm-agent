#!/usr/bin/env bash
# Provision a fresh Dublin host for the whale-pair live bot.
# Usage: provision.sh <host-ip-or-ssh-alias>
#
# This script is intentionally conservative: it prepares the host but does not
# start the bot. Starting is a separate, explicit step (start.sh).

set -euo pipefail

if [ $# -lt 1 ]; then
    echo "Usage: $0 <host-ip-or-ssh-alias>" >&2
    exit 64
fi

HOST="$1"
REPO_URL="${REPO_URL:-https://github.com/0xj7r/polymarket-agent.git}"
BRANCH="${BRANCH:-feat/whale-pair-deploy}"
APP_DIR="/opt/polymarket-agent"

echo "=== Dublin whale-pair provisioning: $HOST ==="
echo "    repo:   $REPO_URL"
echo "    branch: $BRANCH"
echo "    dir:    $APP_DIR"
echo ""

ssh "$HOST" bash -se <<EOF
set -euo pipefail

# Install base packages.
sudo apt-get update
sudo apt-get install -y git chrony ca-certificates curl gnupg lsb-release sqlite3

# Install Docker Engine + Compose plugin if missing.
if ! command -v docker >/dev/null 2>&1; then
    sudo install -m 0755 -d /etc/apt/keyrings
    curl -fsSL https://download.docker.com/linux/ubuntu/gpg | \
        sudo gpg --dearmor -o /etc/apt/keyrings/docker.gpg
    sudo chmod a+r /etc/apt/keyrings/docker.gpg
    echo "deb [arch=\$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.gpg] \
https://download.docker.com/linux/ubuntu \$(lsb_release -cs) stable" | \
        sudo tee /etc/apt/sources.list.d/docker.list >/dev/null
    sudo apt-get update
    sudo apt-get install -y docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin
fi

# Make sure chrony is up (clock drift corrupts book_ts/submit_ts telemetry).
sudo systemctl enable --now chrony

# Checkout.
sudo mkdir -p "$APP_DIR"
sudo chown "\$USER:\$USER" "$APP_DIR"
if [ ! -d "$APP_DIR/.git" ]; then
    git clone "$REPO_URL" "$APP_DIR"
fi
cd "$APP_DIR"
git fetch origin "$BRANCH"
git checkout "$BRANCH"
git pull --ff-only origin "$BRANCH"

mkdir -p "$APP_DIR/data" "$APP_DIR/logs"
chmod 0750 "$APP_DIR/data"

# .env is NOT created by this script. Operator copies it by hand.
if [ ! -f "$APP_DIR/.env" ]; then
    echo ""
    echo "NOTE: $APP_DIR/.env does not exist."
    echo "      scp or create it manually with mode 0600, then run:"
    echo "      bash $APP_DIR/scripts/deploy/whale-pair/check_env.sh"
    echo ""
fi
EOF

echo ""
echo "=== Provisioning complete ==="
echo "Next steps:"
echo "  1. scp your .env to $HOST:$APP_DIR/.env (chmod 600)"
echo "  2. ssh $HOST 'bash $APP_DIR/scripts/deploy/whale-pair/check_env.sh'"
echo "  3. ssh $HOST 'bash $APP_DIR/scripts/deploy/whale-pair/start.sh --dry-run'"
