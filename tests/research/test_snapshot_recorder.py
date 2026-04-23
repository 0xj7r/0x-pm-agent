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
            best_bid_up=0.54,
            best_ask_up=0.55,
            bid_size_up=40.0,
            ask_size_up=45.0,
            best_bid_down=0.44,
            best_ask_down=0.45,
            bid_size_down=41.0,
            ask_size_down=46.0,
            orderbook_up_json='{"bids":[{"price":0.54,"size":40.0}],"asks":[{"price":0.55,"size":45.0}]}',
            orderbook_down_json='{"bids":[{"price":0.44,"size":41.0}],"asks":[{"price":0.45,"size":46.0}]}',
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
            best_bid_up=0.55,
            best_ask_up=0.56,
            bid_size_up=42.0,
            ask_size_up=47.0,
            best_bid_down=0.43,
            best_ask_down=0.44,
            bid_size_down=43.0,
            ask_size_down=48.0,
            orderbook_up_json=None,
            orderbook_down_json=None,
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
            best_bid_up=0.59,
            best_ask_up=0.60,
            bid_size_up=50.0,
            ask_size_up=55.0,
            best_bid_down=0.39,
            best_ask_down=0.40,
            bid_size_down=51.0,
            ask_size_down=56.0,
            orderbook_up_json=None,
            orderbook_down_json=None,
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


def test_flush_batch_persists_extended_orderbook_columns_locally():
    from collector.snapshot_recorder import SnapshotWrite

    recorder, mock_conn, _ = _make_recorder("btc")
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
            best_bid_up=0.54,
            best_ask_up=0.55,
            bid_size_up=40.0,
            ask_size_up=45.0,
            best_bid_down=0.44,
            best_ask_down=0.45,
            bid_size_down=41.0,
            ask_size_down=46.0,
            orderbook_up_json='{"bids":[{"price":0.54,"size":40.0}],"asks":[{"price":0.55,"size":45.0}]}',
            orderbook_down_json='{"bids":[{"price":0.44,"size":41.0}],"asks":[{"price":0.45,"size":46.0}]}',
            bid_price=0.54,
            ask_price=0.56,
            bid_size=100.0,
            ask_size=120.0,
            elapsed_s=10.0,
        )
    ]

    recorder._flush_batch(batch)

    snapshot_call = None
    for call in mock_conn.execute.call_args_list:
        sql = call.args[0]
        if "INSERT OR REPLACE INTO snapshots" in sql:
            snapshot_call = call
            break

    assert snapshot_call is not None
    values = snapshot_call.args[1]
    assert values[5:13] == (0.54, 0.55, 40.0, 45.0, 0.44, 0.45, 41.0, 46.0)
    assert values[13] == batch[0].orderbook_up_json
    assert values[14] == batch[0].orderbook_down_json
