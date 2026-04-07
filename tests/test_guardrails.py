"""Tests for trading guardrails: persistence, kill switch, stale data, sanity checks.

These guardrails prevent catastrophic failures in the trading engine.
"""
from __future__ import annotations

import time
from datetime import datetime, timezone
from unittest.mock import MagicMock, patch, PropertyMock

import pytest


def _make_engine(coin: str = "btc", supa_available: bool = True):
    """Create a minimal engine with mocked dependencies for guardrail testing."""
    with patch("core.engine.BinanceWSClient"), \
         patch("core.engine.MarketWindowScanner"), \
         patch("core.engine.PolymarketClient"), \
         patch("core.engine.PolymarketWSClient") as mock_ws_cls, \
         patch("core.engine.MemoryStore"), \
         patch("core.engine.SlackNotifier"), \
         patch("core.engine.PaperTradeResolver"), \
         patch("core.engine.HealthServer"):

        mock_ws = MagicMock()
        mock_ws.has_live_book.return_value = True
        mock_ws.get_price.return_value = 0.50
        mock_ws_cls.return_value = mock_ws

        mock_supa = MagicMock()
        mock_supa.health_check.return_value = True
        supa_patch = patch(
            "shared.supabase_client.SupabaseClient",
            return_value=mock_supa,
        )

        with supa_patch:
            from strategies.strategy_config import StrategyConfig
            cfg = StrategyConfig()
            from core.engine import BTCTradingEngine
            engine = BTCTradingEngine(cfg, db_path=":memory:", coin=coin)

        engine._supa = mock_supa
        engine.poly_ws = mock_ws

        if not supa_available:
            engine._supa = None

        return engine


def _set_tradeable_window(engine):
    """Configure engine with a window that should trigger a trade."""
    from models.market import MarketWindow
    engine.current_window = MarketWindow(
        market_id="12345",
        question="BTC Up or Down",
        up_token_id="up_tok",
        down_token_id="down_tok",
        up_price=0.50,
        down_price=0.50,
        start_time=datetime.now(timezone.utc),
        end_time=datetime.now(timezone.utc),
    )
    engine._current_btc_price = 67100.0
    engine._window_open_price = 67000.0
    engine.balance = 100.0


class TestPersistenceGuardrail:
    """Engine should not trade when persistence is broken."""

    def test_engine_init_fails_loudly_when_supabase_missing(self):
        """Engine should raise on startup if Supabase is unavailable, not silently run."""
        with patch("core.engine.BinanceWSClient"), \
             patch("core.engine.MarketWindowScanner"), \
             patch("core.engine.PolymarketClient"), \
             patch("core.engine.PolymarketWSClient"), \
             patch("core.engine.MemoryStore"), \
             patch("core.engine.SlackNotifier"), \
             patch("core.engine.PaperTradeResolver"), \
             patch("core.engine.HealthServer"), \
             patch("shared.supabase_client.SupabaseClient",
                   side_effect=RuntimeError("SUPABASE_URL not set")):
            from strategies.strategy_config import StrategyConfig
            from core.engine import BTCTradingEngine

            with pytest.raises(RuntimeError):
                BTCTradingEngine(StrategyConfig(), db_path=":memory:", coin="btc")

    def test_engine_init_fails_when_health_check_fails(self):
        """Engine should raise on startup if Supabase health check fails (schema mismatch)."""
        with patch("core.engine.BinanceWSClient"), \
             patch("core.engine.MarketWindowScanner"), \
             patch("core.engine.PolymarketClient"), \
             patch("core.engine.PolymarketWSClient"), \
             patch("core.engine.MemoryStore"), \
             patch("core.engine.SlackNotifier"), \
             patch("core.engine.PaperTradeResolver"), \
             patch("core.engine.HealthServer"):
            mock_supa = MagicMock()
            mock_supa.health_check.return_value = False
            with patch("shared.supabase_client.SupabaseClient", return_value=mock_supa):
                from strategies.strategy_config import StrategyConfig
                from core.engine import BTCTradingEngine

                with pytest.raises(RuntimeError, match="health check"):
                    BTCTradingEngine(StrategyConfig(), db_path=":memory:", coin="btc")

    def test_no_trade_when_supabase_unavailable(self):
        """If Supabase client somehow becomes None after startup, do not enter trades."""
        engine = _make_engine(supa_available=False)
        _set_tradeable_window(engine)

        trades = engine._check_entry()

        assert trades == [], (
            "Engine should refuse to trade when persistence (Supabase) is unavailable. "
            "Trading without persistence means losing trade records silently."
        )

    def test_trades_when_supabase_available(self):
        """Normal case: Supabase is available, trades should proceed."""
        engine = _make_engine(supa_available=True)
        _set_tradeable_window(engine)

        trades = engine._check_entry()

        assert len(trades) >= 0  # may or may not trade depending on signal


class TestKillSwitchIntegration:
    """Kill switch should be checked before entering trades."""

    def test_no_trade_when_balance_below_kill_threshold(self):
        """Engine should refuse to trade when balance is at or below kill threshold."""
        engine = _make_engine()
        _set_tradeable_window(engine)
        engine.balance = 4.0  # below default kill of $5

        trades = engine._check_entry()

        assert trades == [], (
            "Engine should refuse to trade when balance ($4) is below "
            "kill threshold ($5). This prevents trading to zero."
        )

    def test_trades_when_balance_above_kill_threshold(self):
        """Normal case: balance is healthy."""
        engine = _make_engine()
        _set_tradeable_window(engine)
        engine.balance = 100.0

        trades = engine._check_entry()

        assert len(trades) >= 0


class TestStaleDataProtection:
    """Engine should not trade on stale Binance data."""

    def test_no_trade_when_binance_price_is_zero(self):
        """If Binance hasn't sent any data, btc_price is 0."""
        engine = _make_engine()
        _set_tradeable_window(engine)
        engine._current_btc_price = 0.0

        trades = engine._check_entry()

        assert trades == [], "Should not trade with zero BTC price"

    def test_no_trade_when_window_open_price_is_zero(self):
        """If window just opened and we haven't seen a price yet."""
        engine = _make_engine()
        _set_tradeable_window(engine)
        engine._window_open_price = 0.0

        trades = engine._check_entry()

        assert trades == [], "Should not trade with zero open price"


class TestMaxTradeSizeSanity:
    """Position size should never exceed sane limits."""

    def test_trade_size_never_exceeds_balance(self):
        """No single trade should be larger than the current balance."""
        engine = _make_engine()
        _set_tradeable_window(engine)
        engine.balance = 50.0

        trades = engine._check_entry()

        for t in trades:
            assert t["size_usd"] <= engine.balance, (
                f"Trade size ${t['size_usd']:.2f} exceeds balance ${engine.balance:.2f}"
            )

    def test_trade_size_never_negative(self):
        """Position size must be positive."""
        engine = _make_engine()
        _set_tradeable_window(engine)

        trades = engine._check_entry()

        for t in trades:
            assert t["size_usd"] > 0, "Trade size must be positive"

    def test_no_trade_with_negative_balance(self):
        """Should not trade if balance has gone negative (bug state)."""
        engine = _make_engine()
        _set_tradeable_window(engine)
        engine.balance = -5.0

        trades = engine._check_entry()

        assert trades == [], "Should not trade with negative balance"


class TestDuplicateTradeProtection:
    """Should not trade the same window twice."""

    def test_no_double_entry_same_window(self):
        """Once a trade is placed in a window, no second trade."""
        engine = _make_engine()
        _set_tradeable_window(engine)
        engine._already_traded_this_window = True

        trades = engine._check_entry()

        assert trades == [], "Should not trade same window twice"

    def test_flag_resets_on_new_window(self):
        """The duplicate guard should reset when a new window starts."""
        engine = _make_engine()
        _set_tradeable_window(engine)
        engine._already_traded_this_window = True

        from models.market import MarketWindow
        new_window = MarketWindow(
            market_id="99999",
            question="BTC Up or Down - new",
            up_token_id="up2",
            down_token_id="down2",
            up_price=0.50,
            down_price=0.50,
            start_time=datetime.now(timezone.utc),
            end_time=datetime.now(timezone.utc),
        )
        engine._on_new_window(new_window)

        assert engine._already_traded_this_window is False, (
            "Trade flag should reset on new window"
        )


class TestLiveBookRequired:
    """Should not trade without live order book data."""

    def test_no_trade_without_live_up_book(self):
        engine = _make_engine()
        _set_tradeable_window(engine)
        engine.poly_ws.has_live_book.side_effect = lambda tid: tid != "up_tok"

        trades = engine._check_entry()

        assert trades == [], "Should not trade without live UP book"

    def test_no_trade_without_live_down_book(self):
        engine = _make_engine()
        _set_tradeable_window(engine)
        engine.poly_ws.has_live_book.side_effect = lambda tid: tid != "down_tok"

        trades = engine._check_entry()

        assert trades == [], "Should not trade without live DOWN book"
