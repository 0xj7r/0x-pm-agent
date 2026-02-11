"""Tests for Portfolio tracking."""

from __future__ import annotations

import pytest

from core.portfolio import Portfolio
from models.trade import Outcome, Side, Signal, SignalSource, Trade, TradeResult


def _make_trade(
    market_id: str = "m1",
    size_usd: float = 10.0,
    price: float = 0.50,
    paper: bool = False,
) -> Trade:
    signal = Signal(
        market_id=market_id,
        market_question="Test?",
        outcome=Outcome.YES,
        side=Side.BUY,
        source=SignalSource.WEATHER,
        fair_value=0.7,
        market_price=price,
        edge=0.2,
        confidence=0.7,
    )
    return Trade(
        id=f"t-{market_id}",
        signal=signal,
        size_usd=size_usd,
        price=price,
        outcome=Outcome.YES,
        side=Side.BUY,
        token_id="tok-1",
        executed=True,
        paper=paper,
    )


class TestRecordTrade:
    def test_paper_trade_no_balance_change(self):
        p = Portfolio(initial_balance=100.0)
        p.record_trade(_make_trade(paper=True))
        assert p.balance_usd == 100.0

    def test_live_trade_deducts_balance(self):
        p = Portfolio(initial_balance=100.0)
        p.record_trade(_make_trade(size_usd=10.0, paper=False))
        assert p.balance_usd == 90.0

    def test_creates_position(self):
        p = Portfolio(initial_balance=100.0)
        p.record_trade(_make_trade(paper=False))
        assert len(p.positions) == 1

    def test_trade_list_grows(self):
        p = Portfolio(initial_balance=100.0)
        p.record_trade(_make_trade(paper=True))
        p.record_trade(_make_trade(market_id="m2", paper=True))
        assert len(p.trades) == 2


class TestRecordResult:
    def test_winning_result(self):
        p = Portfolio(initial_balance=90.0)
        result = TradeResult(trade_id="t1", market_id="m1", resolved=True, won=True, pnl_usd=15.0)
        p.record_result(result)
        assert p.balance_usd == 105.0
        assert len(p.results) == 1

    def test_losing_result(self):
        p = Portfolio(initial_balance=90.0)
        result = TradeResult(trade_id="t1", market_id="m1", resolved=True, won=False, pnl_usd=-10.0)
        p.record_result(result)
        assert p.balance_usd == 80.0

    def test_unresolved_no_change(self):
        p = Portfolio(initial_balance=100.0)
        result = TradeResult(trade_id="t1", market_id="m1", resolved=False)
        p.record_result(result)
        assert p.balance_usd == 100.0


class TestSnapshot:
    def test_empty_portfolio(self):
        p = Portfolio(initial_balance=100.0)
        snap = p.snapshot()
        assert snap.balance_usd == 100.0
        assert snap.num_trades == 0
        assert snap.win_rate == 0.0
        assert snap.total_value == 100.0

    def test_with_trades_and_results(self):
        p = Portfolio(initial_balance=100.0)
        p.record_trade(_make_trade(paper=False, size_usd=10.0))
        p.record_result(TradeResult(trade_id="t1", market_id="m1", resolved=True, won=True, pnl_usd=5.0))
        snap = p.snapshot()
        assert snap.num_trades == 1
        assert snap.realized_pnl == 5.0
        assert snap.win_rate == 1.0

    def test_net_pnl_subtracts_api_cost(self):
        p = Portfolio(initial_balance=100.0)
        p.record_api_cost(0.50)
        p.record_result(TradeResult(trade_id="t1", market_id="m1", resolved=True, won=True, pnl_usd=5.0))
        snap = p.snapshot()
        assert snap.net_pnl == pytest.approx(4.50)
