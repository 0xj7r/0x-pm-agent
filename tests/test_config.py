"""Tests for Config defaults."""

from __future__ import annotations

from config import Config


def test_defaults():
    c = Config()
    assert c.MIN_EDGE_THRESHOLD == 0.15
    assert c.SCAN_INTERVAL_SECONDS == 120
    assert c.EXIT_THRESHOLD == 0.45
    assert c.PAPER_TRADE is True
    assert c.PAPER_STARTING_BALANCE == 100.0
    assert c.MAX_POSITION_PCT == 0.06
    assert c.MAX_POSITION_USD == 2.0
    assert c.DAILY_LOSS_LIMIT_PCT == 0.20
    assert c.MAX_CONCURRENT_POSITIONS == 10
    assert c.LOSS_COOLDOWN_TRADES == 3
    assert c.LOSS_COOLDOWN_SECONDS == 1800
    assert c.KILL_BALANCE_USD == 5.0
    assert c.CHAIN_ID == 137
    assert c.ENABLE_WEATHER is True
    assert c.ENABLE_ARBITRAGE is True
    assert c.ENABLE_COPY_TRADING is True
    assert c.LOG_LEVEL == "INFO"
    assert c.DB_PATH == "trades.db"
