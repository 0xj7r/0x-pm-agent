from __future__ import annotations

import json
from unittest.mock import AsyncMock

import pytest

from clients.polymarket_ws import PolymarketWSClient


@pytest.mark.asyncio
async def test_sync_subscriptions_only_sends_delta():
    ws = PolymarketWSClient()
    ws._subscribed_ids = {"old", "keep"}
    ws._ws = AsyncMock()

    await ws.sync_subscriptions(["keep", "new"])

    assert ws._subscribed_ids == {"keep", "new"}
    assert ws._ws.send.await_count == 2
    unsubscribe_msg = json.loads(ws._ws.send.await_args_list[0].args[0])
    subscribe_msg = json.loads(ws._ws.send.await_args_list[1].args[0])
    assert unsubscribe_msg["operation"] == "unsubscribe"
    assert unsubscribe_msg["assets_ids"] == ["old"]
    assert subscribe_msg["assets_ids"] == ["new"]
    assert subscribe_msg["type"] == "market"
    assert ws.stats()["unsubscribe_calls"] == 1
    assert ws.stats()["subscribe_calls"] == 1


@pytest.mark.asyncio
async def test_close_clears_socket_reference():
    ws = PolymarketWSClient()
    socket = AsyncMock()
    ws._ws = socket

    await ws.close()

    socket.close.assert_awaited_once()
    assert ws.connected is False
