"""Tests for market resolution and P&L calculation logic."""

from __future__ import annotations

import uuid

import pytest

from core.memory import MemoryStore
from core.portfolio import Portfolio
from models.market import Outcome
from models.trade import Side, Signal, SignalSource, Trade, TradeResult


def _make_trade(
    market_id: str = "market-001",
    trade_id: str | None = None,
    outcome: Outcome = Outcome.YES,
    price: float = 0.25,
    size_usd: float = 10.0,
) -> Trade:
    sig = Signal(
        market_id=market_id,
        market_question=f"Test {market_id}",
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
        id=trade_id or str(uuid.uuid4()),
        signal=sig,
        size_usd=size_usd,
        price=price,
        outcome=outcome,
        side=Side.BUY,
        token_id="tok-001",
        executed=True,
        paper=True,
    )


@pytest.fixture
def memory(tmp_path):
    store = MemoryStore(str(tmp_path / "test.db"))
    yield store
    store.close()


class TestResolutionPnL:
    """Test P&L calculations matching _check_resolutions logic in engine."""

    @staticmethod
    def _calc_pnl(
        side: str,
        our_outcome: str,
        winning_outcome: str,
        size_usd: float,
        price: float,
    ) -> tuple[bool, float]:
        """Replicates the P&L logic from TradingEngine._check_resolutions."""
        shares = size_usd / price if price > 0 else 0
        if side == "BUY":
            won = our_outcome == winning_outcome
            if won:
                pnl = shares * 1.0 - size_usd
            else:
                pnl = -size_usd
        else:
            won = our_outcome != winning_outcome
            if won:
                pnl = size_usd
            else:
                pnl = -(shares * 1.0 - size_usd)
        return won, pnl

    def test_buy_yes_resolves_yes(self) -> None:
        """BUY YES at $0.25 → YES wins → profit = (shares * $1) - cost."""
        won, pnl = self._calc_pnl("BUY", "Yes", "Yes", size_usd=10.0, price=0.25)
        assert won is True
        shares = 10.0 / 0.25  # 40 shares
        assert pnl == pytest.approx(shares * 1.0 - 10.0)  # $30 profit
        assert pnl == pytest.approx(30.0)

    def test_buy_yes_resolves_no(self) -> None:
        """BUY YES at $0.25 → NO wins → loss = -cost."""
        won, pnl = self._calc_pnl("BUY", "Yes", "No", size_usd=10.0, price=0.25)
        assert won is False
        assert pnl == pytest.approx(-10.0)

    def test_buy_no_resolves_no(self) -> None:
        """BUY NO at $0.60 → NO wins → profit = (shares * $1) - cost."""
        won, pnl = self._calc_pnl("BUY", "No", "No", size_usd=10.0, price=0.60)
        assert won is True
        shares = 10.0 / 0.60
        expected = shares * 1.0 - 10.0  # ~$6.67 profit
        assert pnl == pytest.approx(expected)

    def test_buy_no_resolves_yes(self) -> None:
        """BUY NO at $0.60 → YES wins → loss = -cost."""
        won, pnl = self._calc_pnl("BUY", "No", "Yes", size_usd=10.0, price=0.60)
        assert won is False
        assert pnl == pytest.approx(-10.0)


class TestOnlyClosedMarketsResolve:
    """Test that check_market_resolution only returns results for closed markets."""

    @staticmethod
    def _make_client_with_response(json_data: dict):
        from unittest.mock import AsyncMock, MagicMock, patch

        from clients.polymarket import PolymarketClient

        client = PolymarketClient.__new__(PolymarketClient)
        mock_resp = MagicMock()
        mock_resp.status_code = 200
        mock_resp.raise_for_status = MagicMock()
        mock_resp.json.return_value = json_data
        client._http = MagicMock()
        client._http.get = AsyncMock(return_value=mock_resp)
        client.gamma_url = "https://gamma-api.polymarket.com"
        return client

    @pytest.mark.asyncio
    async def test_open_market_returns_none(self) -> None:
        client = self._make_client_with_response({"closed": False, "outcomePrices": '["0.5","0.5"]'})
        result = await client.check_market_resolution("market-001")
        assert result is None

    @pytest.mark.asyncio
    async def test_closed_resolved_yes(self) -> None:
        client = self._make_client_with_response({"closed": True, "outcomePrices": '["1","0"]'})
        result = await client.check_market_resolution("market-001")
        assert result is not None
        assert result["winning_outcome"] == "Yes"

    @pytest.mark.asyncio
    async def test_closed_resolved_no(self) -> None:
        client = self._make_client_with_response({"closed": True, "outcomePrices": '["0","1"]'})
        result = await client.check_market_resolution("market-001")
        assert result is not None
        assert result["winning_outcome"] == "No"


class TestTradeResolvedOnce:
    """Verify a trade can only be resolved once (no double-counting)."""

    def test_mark_resolved_twice_same_result(self, memory: MemoryStore) -> None:
        trade = _make_trade(trade_id="t-001")
        memory.save_trade(trade)

        memory.mark_trade_resolved("t-001", "market-001", won=True, pnl=30.0)
        # After resolution, trade should no longer appear in open trades
        assert memory.get_open_trades() == []

        # Resolving again with INSERT OR REPLACE won't create a second entry
        memory.mark_trade_resolved("t-001", "market-001", won=True, pnl=30.0)
        # Still no open trades
        assert memory.get_open_trades() == []

        # Only 1 result row
        rows = memory.conn.execute("SELECT COUNT(*) FROM results WHERE trade_id='t-001'").fetchone()
        assert rows[0] == 1
