"""Shared constants for all modules."""
from pathlib import Path

PROJECT_ROOT = Path(__file__).parent.parent
BACKTESTING_DIR = PROJECT_ROOT / "backtesting"

POLYBACKTEST_API_BASE = "https://api.polybacktest.com"
POLYBACKTEST_API_KEYS = {
    "btc": "pdm_CSreRRkeODfhuBa7rbTqZmnyTliLtixT",
    "eth": "pdm_heNVotL45QU4bumqMO7MGx7OILU4xFrk",
    "sol": "pdm_qQxtv2aGxgFj2CND74YvG6HVNx32n29Y",
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


def db_path(coin: str) -> Path:
    """Return the SQLite DB path for a given coin."""
    return BACKTESTING_DIR / f"{coin}.db"
