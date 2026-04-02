"""Application configuration loaded from environment variables."""

from __future__ import annotations

import os

from dotenv import load_dotenv

load_dotenv()


class Config:
    """Central configuration for the Polymarket trading agent.

    All values are read from environment variables with sensible defaults.
    """

    # Polymarket
    PRIVATE_KEY: str = os.getenv("POLYMARKET_PRIVATE_KEY", "")
    API_KEY: str = os.getenv("POLYMARKET_API_KEY", "")
    API_SECRET: str = os.getenv("POLYMARKET_API_SECRET", "")
    API_PASSPHRASE: str = os.getenv("POLYMARKET_API_PASSPHRASE", "")
    CLOB_URL: str = "https://clob.polymarket.com"
    GAMMA_URL: str = "https://gamma-api.polymarket.com"
    CHAIN_ID: int = 137  # Polygon

    # Claude
    ANTHROPIC_API_KEY: str = os.getenv("ANTHROPIC_API_KEY", "")
    CLAUDE_MODEL: str = os.getenv("CLAUDE_MODEL", "claude-opus-4-5-20250514")

    # Risk
    MAX_POSITION_PCT: float = float(os.getenv("MAX_POSITION_PCT", "0.06"))
    MIN_EDGE_THRESHOLD: float = float(os.getenv("MIN_EDGE_THRESHOLD", "0.15"))
    KILL_BALANCE_USD: float = float(os.getenv("KILL_BALANCE_USD", "5.0"))
    SCAN_INTERVAL_SECONDS: int = int(os.getenv("SCAN_INTERVAL_SECONDS", "600"))
    EXIT_THRESHOLD: float = float(os.getenv("EXIT_THRESHOLD", "0.45"))
    MAX_POSITION_USD: float = float(os.getenv("MAX_POSITION_USD", "2.0"))
    DAILY_LOSS_LIMIT_PCT: float = float(os.getenv("DAILY_LOSS_LIMIT_PCT", "0.20"))
    MAX_CONCURRENT_POSITIONS: int = int(os.getenv("MAX_CONCURRENT_POSITIONS", "10"))
    LOSS_COOLDOWN_TRADES: int = int(os.getenv("LOSS_COOLDOWN_TRADES", "3"))
    LOSS_COOLDOWN_SECONDS: int = int(os.getenv("LOSS_COOLDOWN_SECONDS", "1800"))

    # Mode
    PAPER_TRADE: bool = os.getenv("PAPER_TRADE", "true").lower() == "true"

    # Paper trading
    PAPER_STARTING_BALANCE: float = float(os.getenv("PAPER_STARTING_BALANCE", "100.0"))

    # Database
    DB_PATH: str = os.getenv("DB_PATH", "trades.db")

    # Optional memory sync target (empty disables MEMORY.md sync)
    MEMORY_PATH: str = os.getenv("MEMORY_PATH", "")

    # Logging
    LOG_LEVEL: str = os.getenv("LOG_LEVEL", "INFO")
