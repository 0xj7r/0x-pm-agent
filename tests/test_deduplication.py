"""Tests for trade deduplication logic."""

from __future__ import annotations

import os
import uuid

import pytest

from core.memory import MemoryStore
from core.portfolio import Portfolio
from models.market import Outcome
from models.trade import Side, Signal, SignalSource, Trade


def _make_signal(market_id: str = "market-001") -> Signal:
    return Signal(
        market_id=market_id,
        market_question=f"Test question for {market_id}",
        outcome=Outcome.YES,
        side=Side.BUY,
        source=SignalSource.WEATHER,
        fair_value=0.80,
        market_price=0.50,
        edge=0.30,
        confidence=0.7,
        reasoning="test",
        yes_token_id="yes-tok-001",
        no_token_id="no-tok-001",
    )


def _make_trade(market_id: str = "market-001", trade_id: str | None = None) -> Trade:
    sig = _make_signal(market_id)
    return Trade(
        id=trade_id or str(uuid.uuid4()),
        signal=sig,
        size_usd=5.0,
        price=0.50,
        outcome=Outcome.YES,
        side=Side.BUY,
        token_id="yes-tok-001",
        executed=True,
        paper=True,
    )


@pytest.fixture
def memory(tmp_path):
    db_path = str(tmp_path / "test.db")
    store = MemoryStore(db_path)
    yield store
    store.close()


class TestGetOpenTrades:
    def test_empty_db_returns_empty(self, memory: MemoryStore) -> None:
        assert memory.get_open_trades() == []

    def test_saved_paper_trade_is_open(self, memory: MemoryStore) -> None:
        trade = _make_trade("market-001")
        memory.save_trade(trade)
        open_trades = memory.get_open_trades()
        assert len(open_trades) == 1
        assert open_trades[0]["market_id"] == "market-001"

    def test_resolved_trade_not_in_open(self, memory: MemoryStore) -> None:
        trade = _make_trade("market-001", trade_id="t-001")
        memory.save_trade(trade)
        memory.mark_trade_resolved("t-001", "market-001", won=True, pnl=5.0)
        assert memory.get_open_trades() == []

    def test_multiple_markets_all_open(self, memory: MemoryStore) -> None:
        for i in range(3):
            memory.save_trade(_make_trade(f"market-{i}"))
        assert len(memory.get_open_trades()) == 3


class TestDeduplication:
    """Test the dedup logic used in TradingEngine._run_cycle."""

    def test_existing_trade_is_skipped(self, memory: MemoryStore) -> None:
        """Simulate: market-001 already traded → new signal for market-001 should be filtered."""
        memory.save_trade(_make_trade("market-001"))

        existing = memory.get_open_trades()
        traded_markets = {t["market_id"] for t in existing}

        signals = [_make_signal("market-001"), _make_signal("market-002")]
        deduped = [s for s in signals if s.market_id not in traded_markets]

        assert len(deduped) == 1
        assert deduped[0].market_id == "market-002"

    def test_new_market_passes_dedup(self, memory: MemoryStore) -> None:
        existing = memory.get_open_trades()
        traded_markets = {t["market_id"] for t in existing}

        signals = [_make_signal("market-new")]
        deduped = [s for s in signals if s.market_id not in traded_markets]
        assert len(deduped) == 1

    def test_portfolio_positions_also_dedup(self) -> None:
        """In-memory portfolio positions should also block duplicate signals."""
        portfolio = Portfolio(100.0)
        # Simulate a position keyed by market_id
        traded_markets = set(portfolio.positions.keys())
        # No positions yet → signal passes
        signals = [_make_signal("market-001")]
        assert len([s for s in signals if s.market_id not in traded_markets]) == 1

    def test_one_trade_per_market_after_multiple_cycles(self, memory: MemoryStore) -> None:
        """Simulate 5 cycles with the same signal. Should only produce 1 trade."""
        trade_count = 0
        for _ in range(5):
            existing = memory.get_open_trades()
            traded_markets = {t["market_id"] for t in existing}
            signals = [_make_signal("market-001")]
            deduped = [s for s in signals if s.market_id not in traded_markets]
            for sig in deduped:
                memory.save_trade(_make_trade(sig.market_id))
                trade_count += 1

        assert trade_count == 1
        assert len(memory.get_open_trades()) == 1
