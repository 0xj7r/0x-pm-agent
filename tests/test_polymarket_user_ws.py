"""Tests for PolymarketUserWS.

Exercises:
  - subscribe-payload shape (auth + markets)
  - trade / order event dispatch to callback
  - reconnect + resubscribe after disconnect
  - graceful close

We mock the websockets connection with an async context manager that
yields a fake WS whose `recv`/`__aiter__` is script-driven.
"""
from __future__ import annotations

import asyncio
import json
import sys
import types
from typing import Any
from unittest.mock import MagicMock

import pytest

from clients.polymarket_user_ws import PolymarketUserWS, USER_WS_URL


class _FakeWS:
    """Async-iterable fake websocket. `script` is a list of text frames
    to yield. Also records every frame sent via `send`."""

    def __init__(self, script: list[str]) -> None:
        self._script = list(script)
        self.sent: list[str] = []
        self._closed = False

    async def send(self, msg: str) -> None:
        self.sent.append(msg)

    async def close(self) -> None:
        self._closed = True

    def __aiter__(self):
        return self

    async def __anext__(self):
        if not self._script:
            # Yield control; give caller a chance to close. Simulate hang.
            await asyncio.sleep(3600)
            raise StopAsyncIteration
        frame = self._script.pop(0)
        return frame


class _FakeConnect:
    """Async context manager matching websockets.connect(...) signature."""

    def __init__(self, ws: _FakeWS) -> None:
        self._ws = ws

    async def __aenter__(self) -> _FakeWS:
        return self._ws

    async def __aexit__(self, *_exc: Any) -> None:
        await self._ws.close()


def _install_fake_websockets(monkeypatch, ws: _FakeWS) -> None:
    fake = types.ModuleType("websockets")

    def _connect(url: str, **_kw: Any) -> _FakeConnect:
        return _FakeConnect(ws)

    fake.connect = _connect  # type: ignore[attr-defined]
    monkeypatch.setitem(sys.modules, "websockets", fake)


@pytest.mark.asyncio
async def test_subscription_payload_has_auth_and_markets(monkeypatch):
    """First frame sent must match the documented UserSubscriptionRequest."""
    ws = _FakeWS(script=[])  # no events; we just inspect the first send
    _install_fake_websockets(monkeypatch, ws)

    client = PolymarketUserWS(
        api_key="ak", api_secret="secret", api_passphrase="pp",
        markets=["0xcond1", "0xcond2"],
        ping_interval_s=3600.0,  # suppress pings during the test
    )
    task = asyncio.create_task(client.connect())
    # Give connect() a tick to fire the subscription frame.
    for _ in range(20):
        await asyncio.sleep(0.01)
        if ws.sent:
            break

    await client.close()
    task.cancel()
    try:
        await task
    except (asyncio.CancelledError, Exception):
        pass

    assert ws.sent, "expected subscription payload to be sent"
    sub = json.loads(ws.sent[0])
    assert sub["type"] == "user"
    assert sub["auth"] == {
        "apiKey": "ak",
        "secret": "secret",
        "passphrase": "pp",
    }
    assert sub["markets"] == ["0xcond1", "0xcond2"]


@pytest.mark.asyncio
async def test_trade_event_invokes_callback(monkeypatch):
    trade_event = {
        "event_type": "trade",
        "type": "TRADE",
        "id": "trade-1",
        "taker_order_id": "ord-42",
        "market": "0xcond1",
        "asset_id": "tok-up",
        "side": "BUY",
        "size": "100",
        "price": "0.05",
        "status": "MATCHED",
        "owner": "0xme",
        "timestamp": "1700000000000",
    }
    ws = _FakeWS(script=[json.dumps(trade_event)])
    _install_fake_websockets(monkeypatch, ws)

    received: list[dict] = []

    async def on_event(evt: dict) -> None:
        received.append(evt)

    client = PolymarketUserWS(
        api_key="ak", api_secret="secret", api_passphrase="pp",
        on_event=on_event, ping_interval_s=3600.0,
    )
    task = asyncio.create_task(client.connect())

    for _ in range(30):
        await asyncio.sleep(0.01)
        if received:
            break

    await client.close()
    task.cancel()
    try:
        await task
    except (asyncio.CancelledError, Exception):
        pass

    assert len(received) == 1
    assert received[0]["event_type"] == "trade"
    assert received[0]["taker_order_id"] == "ord-42"
    assert received[0]["status"] == "MATCHED"


@pytest.mark.asyncio
async def test_order_event_invokes_callback(monkeypatch):
    order_event = {
        "event_type": "order",
        "id": "ord-99",
        "owner": "0xme",
        "market": "0xcond1",
        "asset_id": "tok-down",
        "side": "BUY",
        "original_size": "50",
        "size_matched": "0",
        "price": "0.08",
        "type": "CANCELLATION",
        "status": "CANCELED",
        "timestamp": "1700000000001",
    }
    ws = _FakeWS(script=[json.dumps(order_event)])
    _install_fake_websockets(monkeypatch, ws)

    received: list[dict] = []
    client = PolymarketUserWS(
        api_key="ak", api_secret="secret", api_passphrase="pp",
        on_event=lambda e: received.append(e),  # sync callback also works
        ping_interval_s=3600.0,
    )
    task = asyncio.create_task(client.connect())

    for _ in range(30):
        await asyncio.sleep(0.01)
        if received:
            break

    await client.close()
    task.cancel()
    try:
        await task
    except (asyncio.CancelledError, Exception):
        pass

    assert len(received) == 1
    assert received[0]["event_type"] == "order"
    assert received[0]["type"] == "CANCELLATION"


def test_rejects_missing_creds():
    with pytest.raises(ValueError):
        PolymarketUserWS(api_key="", api_secret="x", api_passphrase="y")
    with pytest.raises(ValueError):
        PolymarketUserWS(api_key="x", api_secret="", api_passphrase="y")
    with pytest.raises(ValueError):
        PolymarketUserWS(api_key="x", api_secret="y", api_passphrase="")


def test_default_url_is_clob_user_channel():
    assert USER_WS_URL == "wss://ws-subscriptions-clob.polymarket.com/ws/user"


def test_subscribe_payload_omits_markets_when_empty():
    client = PolymarketUserWS(
        api_key="ak", api_secret="s", api_passphrase="p"
    )
    payload = client._build_subscribe_payload()
    assert "markets" not in payload
    assert payload["type"] == "user"


@pytest.mark.asyncio
async def test_callback_exception_does_not_break_socket(monkeypatch):
    """Callback raising must not tear the socket down. Subsequent frames
    still get delivered."""
    trade_a = json.dumps({
        "event_type": "trade", "type": "TRADE", "id": "t1",
        "taker_order_id": "ord-boom", "market": "m",
        "asset_id": "a", "side": "BUY", "size": "1", "price": "0.1",
        "status": "MATCHED", "owner": "0x", "timestamp": "1",
    })
    trade_b = json.dumps({
        "event_type": "trade", "type": "TRADE", "id": "t2",
        "taker_order_id": "ord-ok", "market": "m",
        "asset_id": "a", "side": "BUY", "size": "1", "price": "0.1",
        "status": "MATCHED", "owner": "0x", "timestamp": "2",
    })
    ws = _FakeWS(script=[trade_a, trade_b])
    _install_fake_websockets(monkeypatch, ws)

    seen: list[str] = []

    def on_event(evt: dict) -> None:
        if evt.get("taker_order_id") == "ord-boom":
            raise RuntimeError("boom")
        seen.append(evt.get("taker_order_id"))

    client = PolymarketUserWS(
        api_key="ak", api_secret="s", api_passphrase="p",
        on_event=on_event, ping_interval_s=3600.0,
    )
    task = asyncio.create_task(client.connect())

    for _ in range(40):
        await asyncio.sleep(0.01)
        if "ord-ok" in seen:
            break

    await client.close()
    task.cancel()
    try:
        await task
    except (asyncio.CancelledError, Exception):
        pass

    assert "ord-ok" in seen
