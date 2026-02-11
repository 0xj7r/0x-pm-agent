"""Tests for portfolio P&L tracking end-to-end."""

from __future__ import annotations

import pytest

from core.portfolio import Portfolio
from models.market import Outcome
from models.trade import Side, Signal, SignalSource, Trade, TradeResult


def _make_trade(
    market_id: str,
    size_usd: float = 10.0,
    price: float = 0.50,
    paper: bool = True,
    outcome: Outcome = Outcome.YES,
) -> Trade:
    sig = Signal(
        market_id=market_id,
        market_question=f"Q {market_id}",
        outcome=outcome,
        side=Side.BUY,
        source=SignalSource.WEATHER,
        fair_value=0.80,
        market_price=price,
        edge=0.30,
        confidence=0.7,
        reasoning="test",
    )
    return Trade(
        id=f"t-{market_id}",
        signal=sig,
        size_usd=size_usd,
        price=price,
        outcome=outcome,
        side=Side.BUY,
        token_id=f"tok-{market_id}",
        executed=True,
        paper=paper,
    )


class TestPortfolioPnLTracking:
    def test_initial_balance(self) -> None:
        p = Portfolio(initial_balance=100.0)
        assert p.balance_usd == 100.0
        snap = p.snapshot()
        assert snap.balance_usd == 100.0

    def test_paper_trades_dont_affect_balance(self) -> None:
        p = Portfolio(initial_balance=100.0)
        p.record_trade(_make_trade("m1", paper=True))
        p.record_trade(_make_trade("m2", paper=True))
        assert p.balance_usd == 100.0

    def test_wins_and_losses(self) -> None:
        p = Portfolio(initial_balance=100.0)
        p.record_trade(_make_trade("m1", paper=True))
        p.record_trade(_make_trade("m2", paper=True))
        p.record_trade(_make_trade("m3", paper=True))

        # m1 wins $5
        p.record_result(TradeResult(trade_id="t-m1", market_id="m1", resolved=True, won=True, pnl_usd=5.0))
        assert p.balance_usd == 105.0

        # m2 loses $10
        p.record_result(TradeResult(trade_id="t-m2", market_id="m2", resolved=True, won=False, pnl_usd=-10.0))
        assert p.balance_usd == 95.0

        # m3 wins $3
        p.record_result(TradeResult(trade_id="t-m3", market_id="m3", resolved=True, won=True, pnl_usd=3.0))
        assert p.balance_usd == 98.0

    def test_win_rate_calculation(self) -> None:
        p = Portfolio(initial_balance=100.0)
        p.record_result(TradeResult(trade_id="t1", market_id="m1", resolved=True, won=True, pnl_usd=5.0))
        p.record_result(TradeResult(trade_id="t2", market_id="m2", resolved=True, won=False, pnl_usd=-5.0))
        p.record_result(TradeResult(trade_id="t3", market_id="m3", resolved=True, won=True, pnl_usd=5.0))

        snap = p.snapshot()
        assert snap.win_rate == pytest.approx(2 / 3)

    def test_snapshot_captures_correct_state(self) -> None:
        p = Portfolio(initial_balance=100.0)
        p.record_trade(_make_trade("m1", paper=True))
        p.record_trade(_make_trade("m2", paper=True))
        p.record_result(TradeResult(trade_id="t-m1", market_id="m1", resolved=True, won=True, pnl_usd=10.0))

        snap = p.snapshot()
        assert snap.balance_usd == 110.0
        assert snap.num_trades == 2
        assert snap.realized_pnl == 10.0
        assert snap.win_rate == 1.0

    def test_balance_can_go_negative_in_paper_mode(self) -> None:
        """Document behavior: balance CAN go below $0 — no floor enforced."""
        p = Portfolio(initial_balance=10.0)
        p.record_result(TradeResult(trade_id="t1", market_id="m1", resolved=True, won=False, pnl_usd=-20.0))
        assert p.balance_usd == -10.0  # No floor

    def test_api_cost_tracking(self) -> None:
        p = Portfolio(initial_balance=100.0)
        p.record_api_cost(0.25)
        p.record_api_cost(0.15)
        snap = p.snapshot()
        assert snap.total_api_cost == pytest.approx(0.40)
        assert snap.net_pnl == pytest.approx(-0.40)  # no realized pnl, minus api cost
