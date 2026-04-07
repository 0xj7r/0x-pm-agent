from __future__ import annotations

from unittest.mock import MagicMock, patch


def _make_recorder(coin: str = "btc"):
    with patch("collector.snapshot_recorder.MarketWindowScanner"), \
         patch("collector.snapshot_recorder.PolymarketWSClient"), \
         patch("collector.snapshot_recorder.init_coin_db") as mock_init_db, \
         patch("collector.snapshot_recorder.SupabaseClient") as mock_supa_cls:
        mock_conn = MagicMock()
        mock_init_db.return_value = mock_conn
        mock_supa = MagicMock()
        mock_supa_cls.return_value = mock_supa

        from collector.snapshot_recorder import SnapshotRecorder

        recorder = SnapshotRecorder(coin=coin, interval=1.0, scan_interval=5.0)
        return recorder, mock_conn, mock_supa


def test_flush_batch_upserts_supabase_markets_before_snapshots():
    from collector.snapshot_recorder import SnapshotWrite

    recorder, mock_conn, mock_supa = _make_recorder("btc")
    batch = [
        SnapshotWrite(
            market_id="mkt-1",
            slug="btc-updown-1",
            question="BTC up or down",
            start_time="2026-04-07T10:00:00+00:00",
            end_time="2026-04-07T10:05:00+00:00",
            snapshot_time="2026-04-07T10:00:10+00:00",
            underlying_price=67000.0,
            up_price=0.55,
            down_price=0.45,
            bid_price=0.54,
            ask_price=0.56,
            bid_size=100.0,
            ask_size=120.0,
            elapsed_s=10.0,
        ),
        SnapshotWrite(
            market_id="mkt-1",
            slug="btc-updown-1",
            question="BTC up or down",
            start_time="2026-04-07T10:00:00+00:00",
            end_time="2026-04-07T10:05:00+00:00",
            snapshot_time="2026-04-07T10:00:11+00:00",
            underlying_price=67001.0,
            up_price=0.56,
            down_price=0.44,
            bid_price=0.55,
            ask_price=0.57,
            bid_size=100.0,
            ask_size=120.0,
            elapsed_s=11.0,
        ),
        SnapshotWrite(
            market_id="mkt-2",
            slug="btc-updown-2",
            question="BTC up or down",
            start_time="2026-04-07T10:05:00+00:00",
            end_time="2026-04-07T10:10:00+00:00",
            snapshot_time="2026-04-07T10:05:10+00:00",
            underlying_price=67100.0,
            up_price=0.60,
            down_price=0.40,
            bid_price=0.59,
            ask_price=0.61,
            bid_size=80.0,
            ask_size=90.0,
            elapsed_s=10.0,
        ),
    ]

    recorder._flush_batch(batch)

    mock_conn.commit.assert_called_once()
    mock_supa.upsert_markets.assert_called_once()
    mock_supa.insert_snapshots_batch.assert_called_once()

    market_rows = mock_supa.upsert_markets.call_args[0][0]
    snapshot_rows = mock_supa.insert_snapshots_batch.call_args[0][0]

    assert len(market_rows) == 2
    assert {row["market_id"] for row in market_rows} == {"mkt-1", "mkt-2"}
    assert all(row["coin"] == "btc" for row in market_rows)
    assert all(row["market_type"] == "5m" for row in market_rows)

    assert len(snapshot_rows) == 3
    assert snapshot_rows[0]["market_id"] == "mkt-1"
    assert snapshot_rows[0]["coin"] == "btc"
