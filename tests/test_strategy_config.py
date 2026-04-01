"""Tests for strategy_config loading and validation."""
from __future__ import annotations

import json
import tempfile

import pytest

from strategies.strategy_config import StrategyConfig, load_strategy_config


def test_load_default_config():
    cfg = StrategyConfig()
    assert cfg.signal.w1_order_flow == 0.3
    assert cfg.signal.w2_microprice == 0.2
    assert cfg.signal.w3_price_delta == 0.4
    assert cfg.signal.w4_acceleration == 0.1
    assert cfg.signal.confidence_threshold == 0.85
    assert cfg.execution.max_entry_price == 0.05
    assert cfg.risk.kelly_multiplier == 0.25
    assert cfg.risk.cheap_token_multiplier == 2.0
    assert cfg.paper.enabled is True
    assert cfg.paper.starting_balance == 100.0


def test_load_from_file():
    data = {
        "version": 2,
        "signal": {
            "w1_order_flow": 0.5,
            "w2_microprice": 0.1,
            "w3_price_delta": 0.3,
            "w4_acceleration": 0.1,
            "confidence_threshold": 0.90,
            "prior": 0.5,
        },
        "execution": {
            "max_entry_price": 0.03,
            "entry_window_early": [0, 60],
            "entry_window_late": [270, 295],
            "enable_early_snipe": True,
            "enable_late_snipe": False,
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
            "cheap_token_multiplier": 3.0,
        },
        "paper": {"enabled": True, "starting_balance": 500.0},
    }
    with tempfile.NamedTemporaryFile(mode="w", suffix=".json", delete=False) as f:
        json.dump(data, f)
        f.flush()
        cfg = load_strategy_config(f.name)

    assert cfg.signal.w1_order_flow == 0.5
    assert cfg.execution.max_entry_price == 0.03
    assert cfg.execution.enable_late_snipe is False
    assert cfg.risk.kelly_multiplier == 0.5
    assert cfg.paper.starting_balance == 500.0


def test_load_missing_file_returns_defaults():
    cfg = load_strategy_config("/nonexistent/path.json")
    assert cfg.signal.confidence_threshold == 0.85
