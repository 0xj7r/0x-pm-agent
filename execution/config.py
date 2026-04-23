"""Application configuration loaded from environment variables."""

from __future__ import annotations

import os

from dotenv import load_dotenv

load_dotenv()


def _load_signature_type() -> int | None:
    raw = os.getenv("POLYMARKET_SIGNATURE_TYPE")
    if raw is None or raw.strip() == "":
        return None
    value = int(raw)
    if value not in (0, 1, 2):
        raise ValueError(
            f"POLYMARKET_SIGNATURE_TYPE must be 0 (EOA), 1 (POLY_PROXY), "
            f"or 2 (GNOSIS_SAFE); got {value}"
        )
    return value


def _load_funder() -> str | None:
    raw = os.getenv("POLYMARKET_FUNDER")
    if raw is None or raw.strip() == "":
        return None
    value = raw.strip().lower()
    if not value.startswith("0x"):
        value = "0x" + value
    return value


class Config:
    """Central configuration for the Polymarket trading agent.

    All values are read from environment variables with sensible defaults.
    """

    # Polymarket
    PRIVATE_KEY: str = os.getenv("POLYMARKET_PRIVATE_KEY", "")
    API_KEY: str = os.getenv("POLYMARKET_API_KEY", "")
    API_SECRET: str = os.getenv("POLYMARKET_API_SECRET", "")
    API_PASSPHRASE: str = os.getenv("POLYMARKET_API_PASSPHRASE", "")
    POLYMARKET_SIGNATURE_TYPE: int | None = _load_signature_type()
    POLYMARKET_FUNDER: str | None = _load_funder()
    CLOB_URL: str = "https://clob.polymarket.com"
    GAMMA_URL: str = "https://gamma-api.polymarket.com"
    CHAIN_ID: int = 137  # Polygon

    # On-chain redemption via Polymarket's Gnosis ConditionalTokens (CTF).
    # redeemPositions() converts winning ERC-1155 outcome tokens to USDC.e.
    CTF_ADDRESS: str = os.getenv(
        "CTF_ADDRESS", "0x4D97DCd97eC945f40cF65F87097ACe5EA0476045"
    )
    COLLATERAL_TOKEN_ADDRESS: str = os.getenv(
        "COLLATERAL_TOKEN_ADDRESS",
        "0x2791Bca1F2de4661ED88A30C99A7a9449Aa84174",  # USDC.e on Polygon
    )
    POLYGON_RPC_URL: str = os.getenv("POLYGON_RPC_URL", "https://polygon-bor-rpc.publicnode.com")
    # Optional Polygon WebSocket RPC endpoint for on-chain resolution
    # subscription (eth_subscribe). Empty disables the push path; engine
    # falls back to Gamma polling. Example: wss://polygon-bor-rpc.publicnode.com
    POLYGON_WS_URL: str = os.getenv("POLYGON_WS_URL", "")

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
