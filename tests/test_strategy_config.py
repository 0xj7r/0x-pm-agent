"""Tests for strategy_config loading and validation."""
from __future__ import annotations

import json
import tempfile

import pytest

from strategies.strategy_config import (
    StrategyConfig,
    ThresholdConfig,
    load_strategy_config,
)


def test_load_default_config():
    cfg = StrategyConfig()
    assert "btc" in cfg.coins
    assert "eth" in cfg.coins
    assert "sol" in cfg.coins
    assert cfg.coins["btc"].move_threshold == 0.08
    assert cfg.coins["btc"].max_entry == 0.55
    assert cfg.coins["eth"].move_threshold == 0.15
    assert cfg.risk.kelly_multiplier == 0.25
    assert cfg.paper.enabled is True
    assert cfg.paper.starting_balance == 100.0


def test_load_from_file():
    data = {
        "version": 3,
        "coins": {
            "btc": {"move_threshold": 0.10, "max_entry": 0.60},
            "eth": {"move_threshold": 0.20, "max_entry": 0.50},
        },
        "risk": {
            "max_position_usd": 100,
            "max_position_pct": 0.15,
            "daily_loss_limit_pct": 0.30,
            "kill_balance_usd": 5,
            "max_concurrent_positions": 30,
            "loss_cooldown_trades": 10,
            "loss_cooldown_seconds": 60,
            "kelly_multiplier": 0.5,
        },
        "paper": {"enabled": True, "starting_balance": 500.0},
    }
    with tempfile.NamedTemporaryFile(mode="w", suffix=".json", delete=False) as f:
        json.dump(data, f)
        f.flush()
        cfg = load_strategy_config(f.name)

    assert cfg.version == 3
    assert cfg.coins["btc"].move_threshold == 0.10
    assert cfg.coins["btc"].max_entry == 0.60
    assert cfg.coins["eth"].move_threshold == 0.20
    assert "sol" not in cfg.coins
    assert cfg.risk.kelly_multiplier == 0.5
    assert cfg.paper.starting_balance == 500.0


def test_load_missing_file_returns_defaults():
    cfg = load_strategy_config("/nonexistent/path.json")
    assert cfg.coins["btc"].move_threshold == 0.08
    assert cfg.coins["eth"].move_threshold == 0.15


def test_unknown_fields_ignored():
    data = {
        "version": 3,
        "coins": {
            "btc": {"move_threshold": 0.08, "max_entry": 0.55, "unknown_field": 99},
        },
        "risk": {"kelly_multiplier": 0.25, "bogus": True},
    }
    with tempfile.NamedTemporaryFile(mode="w", suffix=".json", delete=False) as f:
        json.dump(data, f)
        f.flush()
        cfg = load_strategy_config(f.name)

    assert cfg.coins["btc"].move_threshold == 0.08
    assert cfg.risk.kelly_multiplier == 0.25
