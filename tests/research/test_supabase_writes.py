"""Tests for Supabase trade persistence: entry writes, resolution writes, column mapping.

These tests verify that the engine produces correct payloads for Supabase
and that the SupabaseClient handles them properly. All external API calls
are mocked.
"""
from __future__ import annotations

import json
from datetime import datetime, timezone
from unittest.mock import MagicMock, patch

import pytest


class TestTradeEntryPayload:
    """Verify the Supabase upsert payload for trade entries uses correct column names."""

    def _make_engine(self, coin: str = "btc"):
        """Create a minimal engine with mocked dependencies."""
        with patch("core.engine.BinanceWSClient"), \
             patch("core.engine.MarketWindowScanner"), \
             patch("core.engine.PolymarketClient"), \
             patch("core.engine.PolymarketWSClient") as mock_ws_cls, \
             patch("core.engine.MemoryStore"), \
             patch("core.engine.SlackNotifier"), \
             patch("core.engine.PaperTradeResolver"), \
             patch("core.engine.HealthServer"), \
             patch("shared.supabase_client.SupabaseClient") as mock_supa_cls:

            mock_ws = MagicMock()
            mock_ws.has_live_book.return_value = True
            mock_ws.get_price.side_effect = lambda tid: 0.50
            mock_ws_cls.return_value = mock_ws

            mock_supa = MagicMock()
            mock_supa_cls.return_value = mock_supa

            from strategies.strategy_config import StrategyConfig
            cfg = StrategyConfig()
            from core.engine import BTCTradingEngine
            engine = BTCTradingEngine(cfg, db_path=":memory:", coin=coin)
            engine._supa = mock_supa
            engine.poly_ws = mock_ws

            return engine, mock_supa

    def test_entry_payload_uses_underlying_price_not_btc_price(self):
        engine, mock_supa = self._make_engine("btc")

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
        engine._current_btc_price = 67000.0
        engine._window_open_price = 66900.0
        engine.balance = 100.0

        trades = engine._check_entry()
        # After the phantom-entry fix, _check_entry no longer writes to
        # persistence; the caller records entries only after fill
        # confirmation. Simulate a paper-mode fill so the Supabase payload
        # is produced for inspection.
        for t in trades:
            engine.persistence.record_entry(engine._strategy.name, t)

        if trades:
            call_args = mock_supa.upsert_trade_safe.call_args
            payload = call_args[0][0] if call_args[0] else call_args[1].get("row", {})
            assert "underlying_price" in payload, (
                f"Payload should use 'underlying_price', not 'btc_price'. Keys: {list(payload.keys())}"
            )
            assert "btc_price" not in payload, (
                "Payload should NOT contain 'btc_price' (column is 'underlying_price')"
            )
            assert payload["underlying_price"] == 67000.0

    def test_entry_payload_has_all_required_fields(self):
        engine, mock_supa = self._make_engine("eth")

        from models.market import MarketWindow
        engine.current_window = MarketWindow(
            market_id="99999",
            question="ETH Up or Down",
            up_token_id="up_tok",
            down_token_id="down_tok",
            up_price=0.50,
            down_price=0.50,
            start_time=datetime.now(timezone.utc),
            end_time=datetime.now(timezone.utc),
        )
        engine._current_btc_price = 2050.0
        engine._window_open_price = 2040.0
        engine.balance = 100.0

        trades = engine._check_entry()
        for t in trades:
            engine.persistence.record_entry(engine._strategy.name, t)

        if trades:
            payload = mock_supa.upsert_trade_safe.call_args[0][0]
            required = {
                "id", "coin", "strategy", "market_id", "direction",
                "token_price", "size_usd", "shares", "paper",
                "underlying_price", "move_pct", "created_at",
            }
            missing = required - set(payload.keys())
            assert not missing, f"Missing required fields: {missing}"

    def test_entry_payload_coin_matches_engine_coin(self):
        engine, mock_supa = self._make_engine("eth")

        from models.market import MarketWindow
        engine.current_window = MarketWindow(
            market_id="99999",
            question="ETH Up or Down",
            up_token_id="up_tok",
            down_token_id="down_tok",
            up_price=0.50,
            down_price=0.50,
            start_time=datetime.now(timezone.utc),
            end_time=datetime.now(timezone.utc),
        )
        engine._current_btc_price = 2050.0
        engine._window_open_price = 2040.0
        engine.balance = 100.0

        trades = engine._check_entry()
        for t in trades:
            engine.persistence.record_entry(engine._strategy.name, t)

        if trades:
            payload = mock_supa.upsert_trade_safe.call_args[0][0]
            assert payload["coin"] == "eth"


class TestResolutionPayload:
    """Verify the Supabase upsert payload for trade resolutions."""

    def test_resolution_uses_underlying_price_not_btc_price(self):
        """The resolution upsert must use 'underlying_price' column."""
        from core.trade_persistence import TradePersistence

        # Read the source to verify the column name
        import inspect
        source = inspect.getsource(TradePersistence.record_resolution)
        assert "underlying_price" in source, (
            "record_resolution should use 'underlying_price' in Supabase payload"
        )
        assert '"btc_price"' not in source.replace("trade.get(\"btc_price\")", ""), (
            "record_resolution should not set 'btc_price' as a Supabase column key"
        )


class TestSupabaseClientTradeStats:
    """Verify load_trade_stats computes correct aggregates."""

    def test_stats_with_mixed_wins_and_losses(self):
        from shared.supabase_client import SupabaseClient

        mock_trades = [
            {"won": True, "pnl_usd": 10.0, "size_usd": 20.0, "resolved_at": "2026-04-04T20:00:00Z"},
            {"won": True, "pnl_usd": 5.0, "size_usd": 15.0, "resolved_at": "2026-04-04T20:05:00Z"},
            {"won": False, "pnl_usd": -8.0, "size_usd": 8.0, "resolved_at": "2026-04-04T20:10:00Z"},
            {"won": None, "pnl_usd": None, "size_usd": 12.0, "resolved_at": None},
        ]

        with patch.object(SupabaseClient, "__init__", lambda self, *a, **kw: None):
            client = SupabaseClient.__new__(SupabaseClient)
            with patch.object(client, "load_trades", return_value=mock_trades):
                stats = client.load_trade_stats()

        assert stats["trades_total"] == 4
        assert stats["trades_resolved"] == 3
        assert stats["wins"] == 2
        assert stats["losses"] == 1
        assert stats["total_pnl"] == 7.0
        assert stats["win_rate"] == 66.7
        assert stats["total_wagered"] == 55.0

    def test_stats_empty_trades(self):
        from shared.supabase_client import SupabaseClient

        with patch.object(SupabaseClient, "__init__", lambda self, *a, **kw: None):
            client = SupabaseClient.__new__(SupabaseClient)
            with patch.object(client, "load_trades", return_value=[]):
                stats = client.load_trade_stats()

        assert stats["trades_total"] == 0
        assert stats["wins"] == 0
        assert stats["win_rate"] == 0.0
        assert stats["total_pnl"] == 0.0

    def test_stats_all_wins(self):
        from shared.supabase_client import SupabaseClient

        mock_trades = [
            {"won": True, "pnl_usd": 10.0, "size_usd": 20.0, "resolved_at": "2026-04-04T20:00:00Z"},
            {"won": True, "pnl_usd": 15.0, "size_usd": 25.0, "resolved_at": "2026-04-04T20:05:00Z"},
        ]

        with patch.object(SupabaseClient, "__init__", lambda self, *a, **kw: None):
            client = SupabaseClient.__new__(SupabaseClient)
            with patch.object(client, "load_trades", return_value=mock_trades):
                stats = client.load_trade_stats()

        assert stats["win_rate"] == 100.0
        assert stats["losses"] == 0


class TestHealthServerSupaRefresh:
    """Verify health server reads trade stats from Supabase."""

    def test_refresh_updates_status_from_supabase(self):
        from core.health import HealthServer

        mock_supa = MagicMock()
        mock_supa.load_trade_stats.return_value = {
            "trades_total": 50,
            "trades_resolved": 45,
            "wins": 40,
            "losses": 5,
            "win_rate": 88.9,
            "total_pnl": 500.0,
            "total_wagered": 1000.0,
            "trades": [],
        }

        server = HealthServer(supa=mock_supa)
        server._last_supa_refresh = 0
        server._refresh_supa_stats()

        assert server._status["trades_total"] == 50
        assert server._status["trades_resolved"] == 45
        assert server._status["wins"] == 40
        assert server._status["total_pnl"] == 500.0

    def test_refresh_caches_for_60_seconds(self):
        import time
        from core.health import HealthServer

        mock_supa = MagicMock()
        mock_supa.load_trade_stats.return_value = {
            "trades_total": 10, "trades_resolved": 10,
            "wins": 8, "losses": 2, "win_rate": 80.0,
            "total_pnl": 100.0, "total_wagered": 200.0, "trades": [],
        }

        server = HealthServer(supa=mock_supa)
        server._last_supa_refresh = time.time()
        server._refresh_supa_stats()

        mock_supa.load_trade_stats.assert_not_called()

    def test_refresh_handles_supabase_failure(self):
        from core.health import HealthServer

        mock_supa = MagicMock()
        mock_supa.load_trade_stats.side_effect = Exception("connection refused")

        server = HealthServer(supa=mock_supa)
        server._last_supa_refresh = 0
        server._refresh_supa_stats()

        assert server._status["trades_total"] == 0

    def test_no_supa_client_is_noop(self):
        from core.health import HealthServer

        server = HealthServer(supa=None)
        server._last_supa_refresh = 0
        server._refresh_supa_stats()

        assert server._status["trades_total"] == 0


class TestSupabaseErrorLogging:
    """Verify persistence helpers use upsert_trade_safe (which logs errors internally)."""

    def test_entry_uses_safe_write(self):
        import inspect
        from core.trade_persistence import TradePersistence
        source = inspect.getsource(TradePersistence.record_entry)
        assert "upsert_trade_safe" in source, (
            "TradePersistence should use upsert_trade_safe for entry writes"
        )

    def test_resolution_uses_safe_write(self):
        import inspect
        from core.trade_persistence import TradePersistence
        source = inspect.getsource(TradePersistence.record_resolution)
        assert "upsert_trade_safe" in source, (
            "TradePersistence should use upsert_trade_safe for resolution writes"
        )

    def test_safe_write_logs_at_error_level(self):
        import inspect
        from shared.supabase_client import SupabaseClient
        source = inspect.getsource(SupabaseClient.upsert_trade_safe)
        assert "logger.error" in source
