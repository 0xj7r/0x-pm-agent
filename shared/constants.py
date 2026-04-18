"""Shared constants for all modules."""
import os
from pathlib import Path

PROJECT_ROOT = Path(__file__).parent.parent
BACKTESTING_DIR = PROJECT_ROOT / "backtesting"
WEATHER_DB_PATH = BACKTESTING_DIR / "weather.db"

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

COINS = ["btc", "eth", "sol"]

COIN_CONFIGS = {
    "btc": {"move_threshold": 0.08, "max_entry": 0.55, "binance_symbol": "btcusdt"},
    "eth": {"move_threshold": 0.15, "max_entry": 0.55, "binance_symbol": "ethusdt"},
    "sol": {"move_threshold": 0.08, "max_entry": 0.55, "binance_symbol": "solusdt"},
}

SLUG_PATTERNS = {
    "btc": "btc-updown-5m-{ts}",
    "eth": "eth-updown-5m-{ts}",
    "sol": "sol-updown-5m-{ts}",
}

WEATHER_CITIES = {
    "london": {
        "label": "London",
        "latitude": 51.5072,
        "longitude": -0.1276,
        "timezone": "Europe/London",
        "aliases": ("london",),
    },
    "nyc": {
        "label": "NYC",
        "latitude": 40.7128,
        "longitude": -74.0060,
        "timezone": "America/New_York",
        "aliases": ("nyc", "new york city", "new york"),
    },
    "miami": {
        "label": "Miami",
        "latitude": 25.7617,
        "longitude": -80.1918,
        "timezone": "America/New_York",
        "aliases": ("miami",),
    },
    "buenos_aires": {
        "label": "Buenos Aires",
        "latitude": -34.6037,
        "longitude": -58.3816,
        "timezone": "America/Argentina/Buenos_Aires",
        "aliases": ("buenos aires",),
    },
}


def db_path(coin: str) -> Path:
    """Return the SQLite DB path for a given coin."""
    return BACKTESTING_DIR / f"{coin}.db"
