"""Tests for BTC paper trade resolution checker."""
from __future__ import annotations

import pytest

from core.btc_resolution import resolve_paper_trade, PaperTradeRecord, _taker_fee


def test_taker_fee_at_2_cents():
    # fee = size * 0.072 * 0.02 * 0.98 = size * 0.0014
    fee = _taker_fee(0.02, 10.0)
    assert abs(fee - 10.0 * 0.072 * 0.02 * 0.98) < 0.001


def test_taker_fee_at_50_cents():
    # fee = size * 0.072 * 0.50 * 0.50 = size * 0.018
    fee = _taker_fee(0.50, 100.0)
    assert abs(fee - 100.0 * 0.072 * 0.25) < 0.001


def test_resolve_buy_up_wins_up():
    trade = PaperTradeRecord(
        trade_id="t1", market_id="m1", direction="UP",
        token_price=0.02, size_usd=10.0, shares=500.0,
    )
    result = resolve_paper_trade(trade, resolved_direction="UP")
    assert result.won is True
    entry_fee = 10.0 * 0.072 * 0.02 * 0.98
    expected_pnl = 500.0 - 10.0 - entry_fee
    assert abs(result.pnl_usd - expected_pnl) < 0.01


def test_resolve_buy_up_wins_down():
    trade = PaperTradeRecord(
        trade_id="t2", market_id="m2", direction="UP",
        token_price=0.02, size_usd=10.0, shares=500.0,
    )
    result = resolve_paper_trade(trade, resolved_direction="DOWN")
    assert result.won is False
    entry_fee = 10.0 * 0.072 * 0.02 * 0.98
    expected_pnl = -10.0 - entry_fee
    assert abs(result.pnl_usd - expected_pnl) < 0.01


def test_resolve_buy_down_wins_down():
    trade = PaperTradeRecord(
        trade_id="t3", market_id="m3", direction="DOWN",
        token_price=0.03, size_usd=9.0, shares=300.0,
    )
    result = resolve_paper_trade(trade, resolved_direction="DOWN")
    assert result.won is True
    entry_fee = 9.0 * 0.072 * 0.03 * 0.97
    expected_pnl = 300.0 - 9.0 - entry_fee
    assert abs(result.pnl_usd - expected_pnl) < 0.01


def test_resolve_buy_down_loses():
    trade = PaperTradeRecord(
        trade_id="t4", market_id="m4", direction="DOWN",
        token_price=0.05, size_usd=5.0, shares=100.0,
    )
    result = resolve_paper_trade(trade, resolved_direction="UP")
    assert result.won is False
    entry_fee = 5.0 * 0.072 * 0.05 * 0.95
    expected_pnl = -5.0 - entry_fee
    assert abs(result.pnl_usd - expected_pnl) < 0.01


def test_fee_is_negligible_at_cheap_prices():
    """At 2c tokens, the fee should be < 0.2% of position size."""
    fee = _taker_fee(0.02, 100.0)
    assert fee < 0.20  # less than 20 cents on a $100 position
