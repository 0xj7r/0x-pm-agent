"""Shared constants for all modules."""
import os
from pathlib import Path

PROJECT_ROOT = Path(__file__).parent.parent
BACKTESTING_DIR = PROJECT_ROOT / "backtesting"

POLYBACKTEST_API_BASE = "https://api.polybacktest.com"
# PolyBackTest keys are not coin-scoped at the API level; a single key
# works for all coins. Loaded from env so rotation is a .env edit, not
# a source change. Per-coin overrides are supported but optional.
_POLYBACKTEST_DEFAULT_KEY = os.environ.get("POLYBACKTEST_API_KEY", "")
POLYBACKTEST_API_KEYS = {
    "btc": os.environ.get("POLYBACKTEST_API_KEY_BTC", _POLYBACKTEST_DEFAULT_KEY),
    "eth": os.environ.get("POLYBACKTEST_API_KEY_ETH", _POLYBACKTEST_DEFAULT_KEY),
    "sol": os.environ.get("POLYBACKTEST_API_KEY_SOL", _POLYBACKTEST_DEFAULT_KEY),
}
RATE_LIMIT_DELAY = 0.20

COIN_CONFIGS = {
    "btc": {"move_threshold": 0.08, "max_entry": 0.55, "binance_symbol": "btcusdt"},
    "eth": {"move_threshold": 0.15, "max_entry": 0.55, "binance_symbol": "ethusdt"},
    "sol": {"move_threshold": 0.08, "max_entry": 0.55, "binance_symbol": "solusdt"},
    # DOGE has higher % intraday vol than BTC; default move_threshold is a
    # rough starting point — refine after ~1 week of forward-collected data.
    "doge": {"move_threshold": 0.20, "max_entry": 0.55, "binance_symbol": "dogeusdt"},
}

COINS = ["btc", "eth", "sol", "doge"]

SLUG_PATTERNS = {
    "5m": {
        "btc": "btc-updown-5m-{ts}",
        "eth": "eth-updown-5m-{ts}",
        "sol": "sol-updown-5m-{ts}",
        "doge": "doge-updown-5m-{ts}",
    },
    "15m": {
        "btc": "btc-updown-15m-{ts}",
        "eth": "eth-updown-15m-{ts}",
    },
}

# Map market_type string to its window length in minutes. Single source of
# truth for boundary rounding, slug stepping, and end_time derivation.
WINDOW_MINUTES = {"5m": 5, "15m": 15}


def slug_pattern(coin: str, market_type: str = "5m") -> str:
    return SLUG_PATTERNS[market_type][coin]


def db_path(coin: str) -> Path:
    """Return the SQLite DB path for a given coin."""
    return BACKTESTING_DIR / f"{coin}.db"
