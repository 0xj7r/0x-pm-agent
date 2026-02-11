import os
from dotenv import load_dotenv

load_dotenv()


class Config:
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
    MIN_EDGE_THRESHOLD: float = float(os.getenv("MIN_EDGE_THRESHOLD", "0.08"))
    KILL_BALANCE_USD: float = float(os.getenv("KILL_BALANCE_USD", "5.0"))
    SCAN_INTERVAL_SECONDS: int = int(os.getenv("SCAN_INTERVAL_SECONDS", "600"))

    # Strategy toggles
    ENABLE_WEATHER: bool = os.getenv("ENABLE_WEATHER", "true").lower() == "true"
    ENABLE_ARBITRAGE: bool = os.getenv("ENABLE_ARBITRAGE", "true").lower() == "true"
    ENABLE_COPY_TRADING: bool = os.getenv("ENABLE_COPY_TRADING", "true").lower() == "true"

    # Weather
    WEATHER_CITIES: list[str] = [
        c.strip()
        for c in os.getenv(
            "WEATHER_CITIES",
            "New York,Los Angeles,Chicago,Houston,Phoenix",
        ).split(",")
    ]

    # Mode
    PAPER_TRADE: bool = os.getenv("PAPER_TRADE", "true").lower() == "true"

    # Database
    DB_PATH: str = os.getenv("DB_PATH", "trades.db")

    # Optional memory sync target (empty disables MEMORY.md sync)
    MEMORY_PATH: str = os.getenv("MEMORY_PATH", "")

    # Logging
    LOG_LEVEL: str = os.getenv("LOG_LEVEL", "INFO")
