"""Tests for asymmetric Kelly sizing on cheap tokens."""
from __future__ import annotations

import pytest

from core.risk import RiskManager
from config import Config
from strategies.strategy_config import RiskConfig


def make_risk_manager() -> RiskManager:
    config = Config()
    config.MAX_POSITION_USD = 50.0
    config.MAX_POSITION_PCT = 0.10
    return RiskManager(config)


def test_cheap_token_sizing_basic():
    rm = make_risk_manager()
    risk_cfg = RiskConfig(kelly_multiplier=0.25, cheap_token_multiplier=2.0,
                          max_position_usd=50.0, max_position_pct=0.10)
    size = rm.asymmetric_kelly_size(
        p_win=0.15, token_price=0.02, bankroll=1000.0, risk_cfg=risk_cfg
    )
    assert size == 50.0  # capped at max_position_usd


def test_cheap_token_sizing_small_bankroll():
    rm = make_risk_manager()
    risk_cfg = RiskConfig(kelly_multiplier=0.25, cheap_token_multiplier=2.0,
                          max_position_usd=50.0, max_position_pct=0.10)
    # p=0.15, c=0.02 → edge=0.13, kelly=0.13/0.98=0.1327
    # adjusted = 0.1327 * 0.25 * 2.0 = 0.0663
    # size = 0.0663 * 100 = $6.63 (below 10% cap of $10)
    size = rm.asymmetric_kelly_size(
        p_win=0.15, token_price=0.02, bankroll=100.0, risk_cfg=risk_cfg
    )
    assert size == 6.63


def test_no_edge_returns_zero():
    rm = make_risk_manager()
    risk_cfg = RiskConfig(kelly_multiplier=0.25, cheap_token_multiplier=2.0,
                          max_position_usd=50.0, max_position_pct=0.10)
    size = rm.asymmetric_kelly_size(
        p_win=0.02, token_price=0.02, bankroll=1000.0, risk_cfg=risk_cfg
    )
    assert size == 0.0


def test_negative_edge_returns_zero():
    rm = make_risk_manager()
    risk_cfg = RiskConfig(kelly_multiplier=0.25, cheap_token_multiplier=2.0,
                          max_position_usd=50.0, max_position_pct=0.10)
    size = rm.asymmetric_kelly_size(
        p_win=0.01, token_price=0.05, bankroll=1000.0, risk_cfg=risk_cfg
    )
    assert size == 0.0


def test_non_cheap_token_no_multiplier():
    rm = make_risk_manager()
    risk_cfg = RiskConfig(kelly_multiplier=0.25, cheap_token_multiplier=2.0,
                          max_position_usd=50.0, max_position_pct=0.10)
    size = rm.asymmetric_kelly_size(
        p_win=0.20, token_price=0.10, bankroll=1000.0, risk_cfg=risk_cfg
    )
    assert 25.0 < size < 30.0


def test_minimum_size_floor():
    rm = make_risk_manager()
    risk_cfg = RiskConfig(kelly_multiplier=0.01, cheap_token_multiplier=1.0,
                          max_position_usd=50.0, max_position_pct=0.10)
    size = rm.asymmetric_kelly_size(
        p_win=0.06, token_price=0.05, bankroll=100.0, risk_cfg=risk_cfg
    )
    assert size == 0.0
