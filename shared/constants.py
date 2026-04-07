"""Shared constants for all modules."""
from pathlib import Path

PROJECT_ROOT = Path(__file__).parent.parent
BACKTESTING_DIR = PROJECT_ROOT / "backtesting"
WEATHER_DB_PATH = BACKTESTING_DIR / "weather.db"

POLYBACKTEST_API_BASE = "https://api.polybacktest.com"
# PolyBackTest API keys are not coin-scoped at the API level despite the
# per-coin layout below. The eth and sol keys both work for any coin's
# data; the original btc key (***POLYBACKTEST_KEY_REMOVED***) was
# stale and returned 401 Invalid API key on every request, which the
# fetcher silently swallowed before commit a3 hardened it. Until we get
# a fresh per-coin key set, all coins use the eth key. The sol key is a
# fallback if eth ever rate limits or revokes.
POLYBACKTEST_API_KEYS = {
    "btc": "***POLYBACKTEST_KEY_REMOVED***",
    "eth": "***POLYBACKTEST_KEY_REMOVED***",
    "sol": "***POLYBACKTEST_KEY_REMOVED***",
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
