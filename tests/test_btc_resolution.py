"""Tests for BTC paper trade resolution checker."""
from __future__ import annotations

import json
import tempfile
from datetime import datetime, timezone

import pytest

from core.btc_resolution import resolve_paper_trade, PaperTradeRecord


def test_resolve_buy_up_wins_up():
    trade = PaperTradeRecord(
        trade_id="t1",
        market_id="m1",
        direction="UP",
        token_price=0.02,
        size_usd=10.0,
        shares=500.0,
    )
    result = resolve_paper_trade(trade, resolved_direction="UP")
    assert result.won is True
    assert result.pnl_usd == 500.0 * 1.0 - 10.0  # shares * $1 - cost
    assert result.pnl_usd == 490.0


def test_resolve_buy_up_wins_down():
    trade = PaperTradeRecord(
        trade_id="t2",
        market_id="m2",
        direction="UP",
        token_price=0.02,
        size_usd=10.0,
        shares=500.0,
    )
    result = resolve_paper_trade(trade, resolved_direction="DOWN")
    assert result.won is False
    assert result.pnl_usd == -10.0


def test_resolve_buy_down_wins_down():
    trade = PaperTradeRecord(
        trade_id="t3",
        market_id="m3",
        direction="DOWN",
        token_price=0.03,
        size_usd=9.0,
        shares=300.0,
    )
    result = resolve_paper_trade(trade, resolved_direction="DOWN")
    assert result.won is True
    assert result.pnl_usd == 300.0 - 9.0
    assert result.pnl_usd == 291.0


def test_resolve_buy_down_loses():
    trade = PaperTradeRecord(
        trade_id="t4",
        market_id="m4",
        direction="DOWN",
        token_price=0.05,
        size_usd=5.0,
        shares=100.0,
    )
    result = resolve_paper_trade(trade, resolved_direction="UP")
    assert result.won is False
    assert result.pnl_usd == -5.0
