"""Polymarket CLOB user-channel WebSocket client.

Push path for order / trade / cancel events on our own orders. Runs in
parallel with existing `get_order_status` polling in `core.engine`; the
engine's `_await_order_fill` uses whichever source reports first.

Endpoint: wss://ws-subscriptions-clob.polymarket.com/ws/user

Auth is done via the first JSON message, not headers. Payload shape:
    {
      "auth": {"apiKey": "...", "secret": "...", "passphrase": "..."},
      "type": "user",
      "markets": ["<condition_id>", ...]   # optional; omit for all markets
    }

Event shapes (as of 2025-Q2 AsyncAPI spec, asyncapi-user.json):

    TradeEvent = {
      event_type: "trade", type: "TRADE", id, taker_order_id,
      market, asset_id, side: "BUY"|"SELL", size, price,
      status: "MATCHED"|"MINED"|"CONFIRMED"|"RETRYING"|"FAILED",
      maker_orders: [{order_id, matched_amount, price, asset_id, ...}],
      owner, timestamp, ...
    }

    OrderEvent = {
      event_type: "order", id, market, asset_id,
      side, original_size, size_matched, price,
      type: "PLACEMENT"|"UPDATE"|"CANCELLATION",
      status, timestamp, ...
    }

Keep-alive: send text "PING" every ~10s, expect "PONG" back.
"""
from __future__ import annotations

import asyncio
import json
import logging
from typing import Any, Awaitable, Callable

logger = logging.getLogger(__name__)

USER_WS_URL = "wss://ws-subscriptions-clob.polymarket.com/ws/user"

OrderEventCallback = Callable[[dict[str, Any]], Awaitable[None] | None]


class PolymarketUserWS:
    """Async consumer of the Polymarket CLOB user channel.

    Usage:
        ws = PolymarketUserWS(api_key, api_secret, api_passphrase,
                              on_event=my_callback)
        task = asyncio.create_task(ws.connect())
        # ...
        await ws.close()

    The callback is invoked with the raw event dict. It may be a coroutine
    function or a plain callable; both are accepted. Exceptions inside the
    callback are logged and do not tear the socket down.
    """

    def __init__(
        self,
        api_key: str,
        api_secret: str,
        api_passphrase: str,
        *,
        on_event: OrderEventCallback | None = None,
        markets: list[str] | None = None,
        url: str = USER_WS_URL,
        ping_interval_s: float = 10.0,
    ) -> None:
        if not api_key or not api_secret or not api_passphrase:
            raise ValueError(
                "PolymarketUserWS requires apiKey / secret / passphrase"
            )
        self._api_key = api_key
        self._api_secret = api_secret
        self._api_passphrase = api_passphrase
        self._url = url
        self._markets: list[str] = list(markets) if markets else []
        self._on_event = on_event
        self._ws = None
        self._running = False
        self._connected = False
        self._reconnect_delay = 1.0
        self._ping_interval_s = ping_interval_s

    @property
    def connected(self) -> bool:
        return self._connected

    def _build_subscribe_payload(self) -> dict[str, Any]:
        """Return the exact subscription JSON shape per docs/asyncapi."""
        payload: dict[str, Any] = {
            "auth": {
                "apiKey": self._api_key,
                "secret": self._api_secret,
                "passphrase": self._api_passphrase,
            },
            "type": "user",
        }
        if self._markets:
            payload["markets"] = list(self._markets)
        return payload

    async def update_markets(self, markets: list[str]) -> None:
        """Replace the subscribed market set.

        Sends UserSubscriptionRequestUpdate if already connected, otherwise
        just stashes for the next (re)connect. The server spec defines
        {"operation": "subscribe"|"unsubscribe", "markets": [...]}.
        """
        new = set(markets)
        old = set(self._markets)
        self._markets = list(new)
        if self._ws is None or not self._connected:
            return

        to_add = list(new - old)
        to_drop = list(old - new)

        try:
            if to_add:
                await self._ws.send(
                    json.dumps({"operation": "subscribe", "markets": to_add})
                )
            if to_drop:
                await self._ws.send(
                    json.dumps(
                        {"operation": "unsubscribe", "markets": to_drop}
                    )
                )
        except Exception as exc:
            logger.warning(f"User WS update_markets failed: {exc}")

    async def connect(self) -> None:
        """Connect and maintain the user WS with exponential backoff.

        Blocks the caller task for the lifetime of the socket; run as a
        background task (`asyncio.create_task`). Idempotent `close()` is
        used to tear down.
        """
        import websockets

        self._running = True
        while self._running:
            try:
                logger.info(f"Connecting to Polymarket user WS ({self._url})")
                async with websockets.connect(self._url) as ws:
                    self._ws = ws
                    self._connected = True
                    self._reconnect_delay = 1.0

                    sub = self._build_subscribe_payload()
                    # Redact secret before logging.
                    redacted = {
                        **sub,
                        "auth": {"apiKey": self._api_key[:6] + "...",
                                 "secret": "***",
                                 "passphrase": "***"},
                    }
                    await ws.send(json.dumps(sub))
                    logger.info(
                        f"User WS subscribed: {redacted} "
                        f"(markets={len(self._markets)})"
                    )

                    ping_task = asyncio.create_task(self._ping_loop(ws))
                    try:
                        async for raw in ws:
                            if raw == "PONG":
                                continue
                            await self._handle_message(raw)
                    finally:
                        ping_task.cancel()

            except Exception as e:
                if not self._running:
                    break
                logger.warning(
                    f"User WS disconnected: {e}. "
                    f"Reconnecting in {self._reconnect_delay:.0f}s. "
                    "Polling fallback remains active."
                )
                self._connected = False
                self._ws = None
                await asyncio.sleep(self._reconnect_delay)
                self._reconnect_delay = min(self._reconnect_delay * 2, 30.0)
        self._connected = False

    async def _ping_loop(self, ws: object) -> None:
        while True:
            await asyncio.sleep(self._ping_interval_s)
            try:
                await ws.send("PING")
            except Exception:
                return

    async def _handle_message(self, raw: str) -> None:
        try:
            data = json.loads(raw)
        except (json.JSONDecodeError, TypeError):
            logger.debug(f"User WS: non-JSON frame ignored: {raw!r:.80}")
            return

        # The user channel may deliver events either as a single object or
        # as a list of objects. Normalize to iteration.
        events: list[dict[str, Any]]
        if isinstance(data, list):
            events = [e for e in data if isinstance(e, dict)]
        elif isinstance(data, dict):
            events = [data]
        else:
            return

        for evt in events:
            await self._dispatch(evt)

    async def _dispatch(self, evt: dict[str, Any]) -> None:
        if self._on_event is None:
            return
        try:
            result = self._on_event(evt)
            if asyncio.iscoroutine(result):
                await result
        except Exception as exc:
            logger.error(
                f"User WS callback raised for event={evt.get('event_type')}: "
                f"{exc}",
                exc_info=True,
            )

    async def close(self) -> None:
        self._running = False
        self._connected = False
        if self._ws is not None:
            try:
                await self._ws.close()
            except Exception:
                pass
            self._ws = None
