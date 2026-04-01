#!/usr/bin/env bash
set -euo pipefail

# Deploy Polymarket BTC Sniper to Hetzner
# Usage: scp -r . hetzner:/opt/polymarket-agent/ && ssh hetzner bash /opt/polymarket-agent/deploy.sh

APP_DIR="/opt/polymarket-agent"
DATA_DIR="$APP_DIR/data"

echo "=== Polymarket BTC Sniper Deployment ==="

# Create data directory
mkdir -p "$DATA_DIR"

# Check for .env
if [ ! -f "$APP_DIR/.env" ]; then
    echo "WARNING: No .env file found. Create one with at minimum:"
    echo "  POLYMARKET_PRIVATE_KEY=<your_key>  (only needed for live trading)"
    echo "  ANTHROPIC_API_KEY=<your_key>        (needed for researcher)"
    echo ""
    echo "For paper trading, .env can be empty — creating blank file."
    touch "$APP_DIR/.env"
fi

# Build and start
cd "$APP_DIR"
docker compose build
docker compose up -d

echo ""
echo "=== Deployed ==="
echo "Paper trading is now running."
echo ""
echo "  Logs:    docker compose logs -f btc-sniper"
echo "  Status:  docker compose ps"
echo "  Stop:    docker compose down"
echo "  Data:    $DATA_DIR/btc_trades.db"
echo ""
echo "Check back in 48 hours to see if the signal fires."
