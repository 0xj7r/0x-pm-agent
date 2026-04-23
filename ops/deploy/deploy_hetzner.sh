#!/usr/bin/env bash
set -euo pipefail

# Deploy the canonical Polymarket runtime to Hetzner
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
    echo "  SUPABASE_URL=<your_url>"
    echo "  SUPABASE_KEY=<your_key>"
    echo ""
    echo "For paper trading, .env still needs runtime secrets if persistence is enabled."
    touch "$APP_DIR/.env"
fi

# Build and start the same image used locally
cd "$APP_DIR"
python scripts/verify_runtime_contract.py
docker compose build
docker compose up -d --remove-orphans

echo ""
echo "=== Deployed ==="
echo "Runtime image rebuilt from the checked-out code and started."
echo ""
echo "  Logs:    docker compose logs -f btc-sniper"
echo "  Status:  docker compose ps"
echo "  Stop:    docker compose down"
echo "  Data:    $DATA_DIR/btc_trades.db"
echo ""
echo "Profiles are selected with STRATEGY_PROFILE inside docker-compose.yml."
