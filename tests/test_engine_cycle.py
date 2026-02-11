"""Integration tests for TradingEngine cycle."""

from __future__ import annotations

import asyncio
from datetime import datetime
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from config import Config
from core.engine import TradingEngine
from models.market import Market, MarketCategory, Outcome
from models.trade import Side, Signal, SignalSource


def _make_market(market_id: str = "market-001", yes_price: float = 0.50) -> Market:
    return Market(
        id=market_id,
        question=f"Test market {market_id}?",
        description="test",
        category=MarketCategory.WEATHER,
        end_date=datetime(2026, 3, 1),
        active=True,
        yes_token_id=f"yes-tok-{market_id}",
        no_token_id=f"no-tok-{market_id}",
        yes_price=yes_price,
        no_price=1.0 - yes_price,
    )


def _make_signal(market_id: str = "market-001", edge: float = 0.30) -> Signal:
    return Signal(
        market_id=market_id,
        market_question=f"Test market {market_id}?",
        outcome=Outcome.YES,
        side=Side.BUY,
        source=SignalSource.WEATHER,
        fair_value=0.50 + edge,
        market_price=0.50,
        edge=edge,
        confidence=0.7,
        reasoning="test",
        yes_token_id=f"yes-tok-{market_id}",
        no_token_id=f"no-tok-{market_id}",
    )


@pytest.fixture
def engine(tmp_path):
    """Create an engine with all external deps mocked."""
    config = Config()
    config.PAPER_TRADE = True
    config.PAPER_STARTING_BALANCE = 100.0
    config.DB_PATH = str(tmp_path / "test.db")
    config.MIN_EDGE_THRESHOLD = 0.15

    with patch("core.engine.PolymarketClient") as MockPoly, \
         patch("core.engine.WeatherClient") as MockWeather, \
         patch("core.engine.ClaudeClient") as MockClaude:

        mock_poly = MockPoly.return_value
        mock_poly.get_all_active_markets = AsyncMock(return_value=[_make_market()])
        mock_poly.get_balance = AsyncMock(return_value=0.0)
        mock_poly.check_market_resolution = AsyncMock(return_value=None)
        mock_poly.close = AsyncMock()

        mock_weather = MockWeather.return_value
        mock_weather.close = AsyncMock()

        mock_claude = MockClaude.return_value
        mock_claude.total_cost_usd = 0.0

        eng = TradingEngine(config)
        yield eng
        eng.memory.close()


class TestFullCycle:
    @pytest.mark.asyncio
    async def test_single_cycle_creates_trade(self, engine: TradingEngine) -> None:
        """One cycle with a signal should create a trade in SQLite."""
        mock_strategy = MagicMock()
        mock_strategy.name = "test"
        mock_strategy.evaluate = AsyncMock(return_value=[_make_signal("market-001")])
        engine.register_strategy(mock_strategy)

        engine.portfolio.balance_usd = 100.0
        engine.risk.set_bankroll(100.0)

        await engine._run_cycle()

        open_trades = engine.memory.get_open_trades()
        assert len(open_trades) == 1
        assert open_trades[0]["market_id"] == "market-001"

    @pytest.mark.asyncio
    async def test_second_cycle_no_duplicates(self, engine: TradingEngine) -> None:
        """Second cycle with same market data should not create new trades."""
        mock_strategy = MagicMock()
        mock_strategy.name = "test"
        mock_strategy.evaluate = AsyncMock(return_value=[_make_signal("market-001")])
        engine.register_strategy(mock_strategy)

        engine.portfolio.balance_usd = 100.0
        engine.risk.set_bankroll(100.0)

        await engine._run_cycle()
        await engine._run_cycle()

        open_trades = engine.memory.get_open_trades()
        assert len(open_trades) == 1  # Still just 1, not 2

    @pytest.mark.asyncio
    async def test_different_markets_both_trade(self, engine: TradingEngine) -> None:
        """Signals for different markets should both execute."""
        mock_strategy = MagicMock()
        mock_strategy.name = "test"
        mock_strategy.evaluate = AsyncMock(return_value=[
            _make_signal("market-001"),
            _make_signal("market-002"),
        ])
        engine.register_strategy(mock_strategy)

        engine.polymarket.get_all_active_markets = AsyncMock(return_value=[
            _make_market("market-001"),
            _make_market("market-002"),
        ])

        engine.portfolio.balance_usd = 100.0
        engine.risk.set_bankroll(100.0)

        await engine._run_cycle()

        open_trades = engine.memory.get_open_trades()
        assert len(open_trades) == 2

    @pytest.mark.asyncio
    async def test_no_signals_no_trades(self, engine: TradingEngine) -> None:
        mock_strategy = MagicMock()
        mock_strategy.name = "test"
        mock_strategy.evaluate = AsyncMock(return_value=[])
        engine.register_strategy(mock_strategy)

        engine.portfolio.balance_usd = 100.0
        engine.risk.set_bankroll(100.0)

        await engine._run_cycle()
        assert engine.memory.get_open_trades() == []
