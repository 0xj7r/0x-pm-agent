#!/usr/bin/env bash
# Validate the Dublin whale-pair .env file has the required secrets and shape.
# Usage: check_env.sh [path-to-env-file]
# Exit 0 if OK, non-zero with a diagnostic on stderr otherwise.

set -euo pipefail

ENV_FILE="${1:-/opt/polymarket-agent/.env}"

if [ ! -f "$ENV_FILE" ]; then
    echo "ERROR: env file not found: $ENV_FILE" >&2
    exit 2
fi

REQUIRED_VARS=(
    POLYMARKET_PRIVATE_KEY
    POLYMARKET_SIGNATURE_TYPE
    POLYMARKET_FUNDER
    STRATEGY_CONFIG
)

MISSING=()
for var in "${REQUIRED_VARS[@]}"; do
    if ! grep -qE "^${var}=.+" "$ENV_FILE"; then
        MISSING+=("$var")
    fi
done

if [ "${#MISSING[@]}" -gt 0 ]; then
    echo "ERROR: missing required env vars in $ENV_FILE:" >&2
    for var in "${MISSING[@]}"; do
        echo "  - $var" >&2
    done
    exit 3
fi

# Signature type must be 0, 1, or 2.
SIG_VALUE=$(grep -E "^POLYMARKET_SIGNATURE_TYPE=" "$ENV_FILE" | head -1 | cut -d= -f2- | tr -d '"' | tr -d "'")
case "$SIG_VALUE" in
    0|1|2) : ;;
    *)
        echo "ERROR: POLYMARKET_SIGNATURE_TYPE must be 0, 1, or 2 (got: '$SIG_VALUE')" >&2
        exit 4
        ;;
esac

# Funder must look like a hex address.
FUNDER_VALUE=$(grep -E "^POLYMARKET_FUNDER=" "$ENV_FILE" | head -1 | cut -d= -f2- | tr -d '"' | tr -d "'")
if ! printf '%s' "$FUNDER_VALUE" | grep -qiE '^0x[0-9a-f]{40}$'; then
    echo "ERROR: POLYMARKET_FUNDER must be a 0x-prefixed 40-char hex address" >&2
    exit 5
fi

# Private key must look like hex (allow with or without 0x prefix).
KEY_VALUE=$(grep -E "^POLYMARKET_PRIVATE_KEY=" "$ENV_FILE" | head -1 | cut -d= -f2- | tr -d '"' | tr -d "'")
KEY_STRIPPED="${KEY_VALUE#0x}"
if ! printf '%s' "$KEY_STRIPPED" | grep -qiE '^[0-9a-f]{64}$'; then
    echo "ERROR: POLYMARKET_PRIVATE_KEY must be a 64-char hex string (optional 0x prefix)" >&2
    exit 6
fi

# File permissions sanity. Dublin .env holds a live key; refuse world-readable.
if [ "$(uname)" = "Linux" ]; then
    PERMS=$(stat -c '%a' "$ENV_FILE")
else
    PERMS=$(stat -f '%Lp' "$ENV_FILE")
fi
case "$PERMS" in
    400|600|640|440) : ;;
    *)
        echo "ERROR: $ENV_FILE has permissions $PERMS; expected 600 or similarly restrictive" >&2
        exit 7
        ;;
esac

echo "OK: $ENV_FILE passes whale-pair Dublin env checks"
exit 0
