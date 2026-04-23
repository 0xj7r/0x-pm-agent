"""Tests for Binance WebSocket client."""
from __future__ import annotations

import json

import pytest

from clients.binance_ws import BinanceWSClient, OrderBookSnapshot, TradeUpdate


def test_trade_update_from_raw():
    raw = {
        "e": "trade",
        "E": 1711843200000,
        "s": "BTCUSDT",
        "p": "84350.50",
        "q": "0.123",
        "m": False,
        "T": 1711843200000,
    }
    update = TradeUpdate.from_raw(raw)
    assert update.price == 84350.50
    assert update.quantity == 0.123
    assert update.is_buyer_maker is False
    assert update.is_buy is True


def test_trade_update_sell():
    raw = {
        "e": "trade",
        "E": 1711843200000,
        "s": "BTCUSDT",
        "p": "84350.50",
        "q": "0.05",
        "m": True,
        "T": 1711843200000,
    }
    update = TradeUpdate.from_raw(raw)
    assert update.is_buyer_maker is True
    assert update.is_buy is False


def test_order_book_snapshot_microprice():
    snap = OrderBookSnapshot(
        best_bid=84350.0,
        best_ask=84351.0,
        bid_size=2.0,
        ask_size=1.0,
        timestamp_ms=1711843200000,
    )
    assert abs(snap.microprice - 84350.6667) < 0.001
    assert snap.mid == 84350.5


@pytest.mark.asyncio
async def test_client_callback_invoked():
    received = []

    async def on_trade(update: TradeUpdate) -> None:
        received.append(update)

    client = BinanceWSClient(on_trade=on_trade)

    raw_msg = json.dumps({
        "e": "trade",
        "E": 1711843200000,
        "s": "BTCUSDT",
        "p": "84500.00",
        "q": "0.5",
        "m": False,
        "T": 1711843200000,
    })

    await client._handle_message(raw_msg)
    assert len(received) == 1
    assert received[0].price == 84500.00


@pytest.mark.asyncio
async def test_client_ignores_non_trade_messages():
    received = []

    async def on_trade(update: TradeUpdate) -> None:
        received.append(update)

    client = BinanceWSClient(on_trade=on_trade)
    await client._handle_message(json.dumps({"e": "depthUpdate", "data": {}}))
    assert len(received) == 0
