from __future__ import annotations

import json

from backtesting.data.fetcher import CoinDataFetcher


def test_extract_book_can_truncate_and_store_json():
    book = {
        "bids": [
            {"price": 0.48, "size": 12},
            {"price": 0.47, "size": 11},
        ],
        "asks": [
            {"price": 0.52, "size": 13},
            {"price": 0.53, "size": 14},
        ],
    }

    best_bid, best_ask, bid_size, ask_size, payload = CoinDataFetcher._extract_book(
        book,
        include_orderbook=True,
        depth=1,
    )

    assert (best_bid, best_ask, bid_size, ask_size) == (0.48, 0.52, 12, 13)
    parsed = json.loads(payload)
    assert parsed == {
        "bids": [{"price": 0.48, "size": 12}],
        "asks": [{"price": 0.52, "size": 13}],
    }


def test_fetch_snapshot_rows_respects_store_orderbooks_flag(monkeypatch):
    fetcher = CoinDataFetcher("btc", store_orderbooks=True, orderbook_depth=1)

    def _fake_fetch(_client, _market_id):
        return [
            {
                "time": "2026-04-22T08:45:00Z",
                "btc_price": 78000.0,
                "price_up": 0.51,
                "price_down": 0.49,
                "orderbook_up": {
                    "bids": [{"price": 0.50, "size": 25}, {"price": 0.49, "size": 20}],
                    "asks": [{"price": 0.51, "size": 30}, {"price": 0.52, "size": 35}],
                },
                "orderbook_down": {
                    "bids": [{"price": 0.48, "size": 28}, {"price": 0.47, "size": 22}],
                    "asks": [{"price": 0.49, "size": 32}, {"price": 0.50, "size": 36}],
                },
            }
        ]

    monkeypatch.setattr(fetcher, "fetch_snapshots", _fake_fetch)
    market_id, rows = fetcher.fetch_snapshot_rows({"market_id": "mkt-1"})

    assert market_id == "mkt-1"
    assert len(rows) == 1
    row = rows[0]
    assert row[0] == "mkt-1"
    assert row[5:13] == (0.5, 0.51, 25, 30, 0.48, 0.49, 28, 32)
    assert json.loads(row[13]) == {
        "bids": [{"price": 0.5, "size": 25}],
        "asks": [{"price": 0.51, "size": 30}],
    }
    assert json.loads(row[14]) == {
        "bids": [{"price": 0.48, "size": 28}],
        "asks": [{"price": 0.49, "size": 32}],
    }
