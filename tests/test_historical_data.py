"""Tests for the historical data pipeline and real-data backtest mode."""
from __future__ import annotations

import sqlite3
import tempfile
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from backtesting.historical_data import (
    generate_slugs,
    init_db,
    fetch_gamma_market,
    fetch_binance_klines,
    load_windows,
    _parse_resolution,
    _slug_to_timestamp,
    fetch_all,
)
from backtesting.btc_backtest import (
    RealWindow,
    run_backtest_real,
    load_real_windows,
)
from strategies.strategy_config import StrategyConfig


# ---------------------------------------------------------------------------
# Slug generation
# ---------------------------------------------------------------------------

class TestGenerateSlugs:
    def test_slug_format(self) -> None:
        from datetime import datetime, timezone
        now = datetime(2025, 4, 1, 12, 0, 0, tzinfo=timezone.utc)
        slugs = generate_slugs(1, now=now)
        assert all(s.startswith("btc-updown-5m-") for s in slugs)

    def test_count_for_one_day(self) -> None:
        from datetime import datetime, timezone
        now = datetime(2025, 4, 1, 12, 0, 0, tzinfo=timezone.utc)
        slugs = generate_slugs(1, now=now)
        assert len(slugs) == 288  # 24h * 12 per hour

    def test_timestamps_increment_by_300(self) -> None:
        from datetime import datetime, timezone
        now = datetime(2025, 4, 1, 12, 0, 0, tzinfo=timezone.utc)
        slugs = generate_slugs(1, now=now)
        timestamps = [_slug_to_timestamp(s) for s in slugs]
        deltas = [timestamps[i+1] - timestamps[i] for i in range(len(timestamps)-1)]
        assert all(d == 300 for d in deltas)


# ---------------------------------------------------------------------------
# Database init
# ---------------------------------------------------------------------------

class TestInitDb:
    def test_creates_tables(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            db_path = Path(tmp) / "test.db"
            conn = init_db(db_path)
            tables = {
                row[0] for row in
                conn.execute(
                    "SELECT name FROM sqlite_master WHERE type='table'"
                ).fetchall()
            }
            assert "resolved_markets" in tables
            assert "binance_klines" in tables
            conn.close()


# ---------------------------------------------------------------------------
# Gamma API parsing
# ---------------------------------------------------------------------------

class TestParseResolution:
    def test_resolved_up(self) -> None:
        event = {
            "markets": [{
                "outcomes": "[\"Up\", \"Down\"]",
                "outcomePrices": "[\"1.0\", \"0.0\"]",
                "closed": True,
            }]
        }
        direction, up_p, down_p = _parse_resolution(event)
        assert direction == "UP"
        assert up_p == 1.0
        assert down_p == 0.0

    def test_resolved_down(self) -> None:
        event = {
            "markets": [{
                "outcomes": "[\"Up\", \"Down\"]",
                "outcomePrices": "[\"0.0\", \"1.0\"]",
                "closed": True,
            }]
        }
        direction, up_p, down_p = _parse_resolution(event)
        assert direction == "DOWN"
        assert down_p == 1.0

    def test_unresolved_returns_none(self) -> None:
        event = {
            "markets": [{
                "outcomes": "[\"Up\", \"Down\"]",
                "outcomePrices": "[\"0.5\", \"0.5\"]",
                "closed": False,
            }]
        }
        direction, _, _ = _parse_resolution(event)
        assert direction is None

    def test_empty_markets(self) -> None:
        direction, _, _ = _parse_resolution({"markets": []})
        assert direction is None


# ---------------------------------------------------------------------------
# Gamma fetch (mocked HTTP)
# ---------------------------------------------------------------------------

class TestFetchGammaMarket:
    def test_success(self) -> None:
        mock_response = MagicMock()
        mock_response.status_code = 200
        mock_response.json.return_value = [{
            "title": "BTC Up or Down 5m",
            "startDate": "2025-04-01T12:00:00Z",
            "endDate": "2025-04-01T12:05:00Z",
            "markets": [{
                "id": "market-123",
                "outcomes": "[\"Up\", \"Down\"]",
                "outcomePrices": "[\"1.0\", \"0.0\"]",
                "closed": True,
            }],
        }]
        mock_response.raise_for_status = MagicMock()

        client = MagicMock()
        client.get.return_value = mock_response

        row = fetch_gamma_market(client, "btc-updown-5m-1712000000")
        assert row is not None
        assert row["resolved_direction"] == "UP"
        assert row["market_id"] == "market-123"

    def test_404_returns_none(self) -> None:
        mock_response = MagicMock()
        mock_response.status_code = 404

        client = MagicMock()
        client.get.return_value = mock_response

        row = fetch_gamma_market(client, "btc-updown-5m-9999999999")
        assert row is None

    def test_empty_list_returns_none(self) -> None:
        mock_response = MagicMock()
        mock_response.status_code = 200
        mock_response.json.return_value = []
        mock_response.raise_for_status = MagicMock()

        client = MagicMock()
        client.get.return_value = mock_response

        row = fetch_gamma_market(client, "btc-updown-5m-1712000000")
        assert row is None


# ---------------------------------------------------------------------------
# Binance fetch (mocked HTTP)
# ---------------------------------------------------------------------------

class TestFetchBinanceKlines:
    def test_parses_kline_array(self) -> None:
        candle = [
            1712000000000,  # open time
            "60000.0",      # open
            "60100.0",      # high
            "59900.0",      # low
            "60050.0",      # close
            "100.5",        # volume
            1712000059999,  # close time
            "6030000.0",    # quote asset volume
            150,            # number of trades
            "55.3",         # taker buy base volume
            "3318000.0",    # taker buy quote volume
            "0",
        ]
        mock_response = MagicMock()
        mock_response.json.return_value = [candle]
        mock_response.raise_for_status = MagicMock()

        client = MagicMock()
        client.get.return_value = mock_response

        rows = fetch_binance_klines(client, "btc-updown-5m-1712000000", 1712000000)
        assert len(rows) == 1
        assert rows[0]["open"] == 60000.0
        assert rows[0]["taker_buy_volume"] == 55.3
        assert rows[0]["slug"] == "btc-updown-5m-1712000000"


# ---------------------------------------------------------------------------
# load_windows from pre-populated DB
# ---------------------------------------------------------------------------

class TestLoadWindows:
    def _make_db(self, tmp: str) -> Path:
        db_path = Path(tmp) / "test.db"
        conn = init_db(db_path)
        conn.execute(
            """INSERT INTO resolved_markets VALUES
               ('btc-updown-5m-1712000000', 'mkt-1', 'BTC Up/Down', '2025-04-01', '2025-04-01',
                'UP', 0.03, 0.97)"""
        )
        conn.execute(
            """INSERT INTO binance_klines VALUES
               ('btc-updown-5m-1712000000', 1712000000000, 60000, 60100, 59900, 60050, 100, 55)"""
        )
        conn.execute(
            """INSERT INTO binance_klines VALUES
               ('btc-updown-5m-1712000000', 1712000060000, 60050, 60200, 60000, 60150, 110, 60)"""
        )
        conn.commit()
        conn.close()
        return db_path

    def test_loads_windows(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            db_path = self._make_db(tmp)
            windows = load_windows(db_path)
            assert len(windows) == 1
            w = windows[0]
            assert w["resolved_direction"] == "UP"
            assert w["up_price"] == 0.03
            assert w["price_delta"] == pytest.approx(
                (60150 - 60000) / 60000 * 100, rel=1e-4
            )

    def test_ofi_calculation(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            db_path = self._make_db(tmp)
            windows = load_windows(db_path)
            w = windows[0]
            total_vol = 100 + 110
            taker_buy = 55 + 60
            expected_ofi = (taker_buy / total_vol) * 2 - 1
            assert w["order_flow_imbalance"] == pytest.approx(expected_ofi, rel=1e-4)


# ---------------------------------------------------------------------------
# Real backtest mode
# ---------------------------------------------------------------------------

class TestRunBacktestReal:
    def test_basic_real_backtest(self) -> None:
        cfg = StrategyConfig()
        cfg.signal.w3_price_delta = 1.0
        cfg.signal.w1_order_flow = 0.3
        cfg.signal.w2_microprice = 0.0
        cfg.signal.w4_acceleration = 0.0
        cfg.signal.confidence_threshold = 0.80
        cfg.execution.max_entry_price = 0.05
        cfg.risk.kelly_multiplier = 0.25
        cfg.risk.cheap_token_multiplier = 2.0
        cfg.risk.max_position_usd = 10.0
        cfg.risk.max_position_pct = 0.10

        windows = [
            RealWindow(
                market_id="mkt-1", slug="btc-updown-5m-1712000000",
                resolved_direction="UP", up_price=0.03, down_price=0.97,
                price_delta=2.5, order_flow_imbalance=0.4,
            ),
            RealWindow(
                market_id="mkt-2", slug="btc-updown-5m-1712000300",
                resolved_direction="DOWN", up_price=0.97, down_price=0.03,
                price_delta=-2.0, order_flow_imbalance=-0.35,
            ),
        ]

        result = run_backtest_real(cfg, windows, starting_balance=100.0)
        assert result.num_trades > 0
        assert isinstance(result.total_pnl, float)
        assert isinstance(result.win_rate, float)

    def test_no_signal_on_flat_market(self) -> None:
        cfg = StrategyConfig()
        cfg.signal.confidence_threshold = 0.95
        cfg.signal.w1_order_flow = 0.0
        cfg.signal.w2_microprice = 0.0
        cfg.signal.w3_price_delta = 0.5
        cfg.signal.w4_acceleration = 0.0
        cfg.execution.max_entry_price = 0.05

        windows = [
            RealWindow(
                market_id="mkt-flat", slug="btc-updown-5m-1712000000",
                resolved_direction="UP", up_price=0.03, down_price=0.97,
                price_delta=0.01, order_flow_imbalance=0.0,
            ),
        ]

        result = run_backtest_real(cfg, windows, starting_balance=100.0)
        assert result.num_trades == 0


# ---------------------------------------------------------------------------
# load_real_windows integration
# ---------------------------------------------------------------------------

class TestLoadRealWindows:
    def test_converts_to_real_window_objects(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            db_path = Path(tmp) / "test.db"
            conn = init_db(db_path)
            conn.execute(
                """INSERT INTO resolved_markets VALUES
                   ('btc-updown-5m-1712000000', 'mkt-1', 'BTC', '2025-04-01', '2025-04-01',
                    'DOWN', 0.95, 0.05)"""
            )
            conn.execute(
                """INSERT INTO binance_klines VALUES
                   ('btc-updown-5m-1712000000', 1712000000000, 60000, 60100, 59900, 59800, 200, 80)"""
            )
            conn.commit()
            conn.close()

            windows = load_real_windows(db_path)
            assert len(windows) == 1
            assert isinstance(windows[0], RealWindow)
            assert windows[0].resolved_direction == "DOWN"
            assert windows[0].slug == "btc-updown-5m-1712000000"


# ---------------------------------------------------------------------------
# fetch_all with mocked HTTP
# ---------------------------------------------------------------------------

class TestFetchAll:
    @patch("backtesting.historical_data.time.sleep")
    @patch("backtesting.historical_data.httpx.Client")
    def test_fetch_all_stores_data(
        self, mock_client_cls: MagicMock, mock_sleep: MagicMock
    ) -> None:
        gamma_resp = MagicMock()
        gamma_resp.status_code = 200
        gamma_resp.json.return_value = [{
            "title": "BTC Up/Down 5m",
            "startDate": "2025-04-01",
            "endDate": "2025-04-01",
            "markets": [{
                "id": "mkt-1",
                "outcomes": "[\"Up\", \"Down\"]",
                "outcomePrices": "[\"1.0\", \"0.0\"]",
                "closed": True,
            }],
        }]
        gamma_resp.raise_for_status = MagicMock()

        binance_resp = MagicMock()
        binance_resp.json.return_value = [[
            1712000000000, "60000", "60100", "59900", "60050",
            "100", 1712000059999, "6030000", 150, "55", "3318000", "0",
        ]]
        binance_resp.raise_for_status = MagicMock()

        mock_client = MagicMock()
        mock_client.__enter__ = MagicMock(return_value=mock_client)
        mock_client.__exit__ = MagicMock(return_value=False)

        def route_get(url: str, **kwargs: object) -> MagicMock:
            if "gamma" in url:
                return gamma_resp
            return binance_resp

        mock_client.get.side_effect = route_get
        mock_client_cls.return_value = mock_client

        with tempfile.TemporaryDirectory() as tmp:
            db_path = Path(tmp) / "test.db"
            from datetime import datetime, timezone
            with patch("backtesting.historical_data.generate_slugs",
                       return_value=["btc-updown-5m-1712000000"]):
                markets, klines = fetch_all(1, db_path=db_path, progress=False)

            assert markets == 1
            assert klines == 1

            conn = sqlite3.connect(str(db_path))
            rows = conn.execute("SELECT * FROM resolved_markets").fetchall()
            assert len(rows) == 1
            conn.close()
