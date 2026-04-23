"""Polymarket CLOB WebSocket client for real-time order book updates.

Subscribes to token price changes for the current market window.
No authentication required for the market channel.
"""
from __future__ import annotations

import asyncio
import json
import logging
import time
from dataclasses import dataclass, field

logger = logging.getLogger(__name__)

WS_URL = "wss://ws-subscriptions-clob.polymarket.com/ws/market"


@dataclass
class TokenBook:
    token_id: str
    best_bid: float = 0.0
    best_bid_size: float = 0.0
    best_ask: float = 0.0
    best_ask_size: float = 0.0
    spread: float = 0.0
    last_trade_price: float = 0.0
    last_update: float = 0.0
    bids: list[dict[str, float]] = field(default_factory=list)
    asks: list[dict[str, float]] = field(default_factory=list)

    def as_orderbook(self, depth: int | None = None) -> dict[str, list[dict[str, float]]]:
        """Return a JSON-serialisable orderbook view.

        `depth=None` keeps all stored levels. When `depth` is positive, the
        returned arrays are truncated to that many levels per side.
        """
        if depth is None or depth <= 0:
            bids = self.bids
            asks = self.asks
        else:
            bids = self.bids[:depth]
            asks = self.asks[:depth]
        return {"bids": bids, "asks": asks}


class PolymarketWSClient:
    """Real-time order book data from Polymarket CLOB WebSocket."""

    def __init__(self) -> None:
        self._ws = None
        self._running = False
        self._subscribed_ids: set[str] = set()
        self._books: dict[str, TokenBook] = {}
        self._reconnect_delay = 1.0
        self._stats = {
            "subscribe_calls": 0,
            "unsubscribe_calls": 0,
            "subscribed_tokens": 0,
        }

    def get_book(self, token_id: str) -> TokenBook | None:
        return self._books.get(token_id)

    def get_price(self, token_id: str) -> float:
        """Get best ask price (what we'd pay to buy). Falls back to last trade."""
        book = self._books.get(token_id)
        if not book:
            return 0.0
        if book.best_ask > 0:
            return book.best_ask
        if book.last_trade_price > 0:
            return book.last_trade_price
        return 0.0

    def has_live_book(self, token_id: str) -> bool:
        """Check if we have a live order book (not stale fallback)."""
        book = self._books.get(token_id)
        return book is not None and book.best_ask > 0

    def book_age_ms(self, token_id: str, now: float | None = None) -> float | None:
        """Milliseconds since the last book update for `token_id`.

        Returns None when the token has never been seen or has no update yet.
        `now` is an epoch seconds override for tests.
        """
        book = self._books.get(token_id)
        if book is None or book.last_update <= 0:
            return None
        current = time.time() if now is None else now
        return max(0.0, (current - book.last_update) * 1000.0)

    def is_book_fresh(
        self,
        token_id: str,
        *,
        max_age_ms: float,
        now: float | None = None,
    ) -> bool:
        """True when we have a live ask and its age is within `max_age_ms`."""
        if not self.has_live_book(token_id):
            return False
        age = self.book_age_ms(token_id, now=now)
        if age is None:
            return False
        return age <= max_age_ms

    @property
    def connected(self) -> bool:
        """True while a live WS connection is held."""
        return self._ws is not None

    @property
    def subscribed_count(self) -> int:
        return len(self._subscribed_ids)

    def stats(self) -> dict[str, int]:
        stats = dict(self._stats)
        stats["subscribed_tokens"] = len(self._subscribed_ids)
        return stats

    @staticmethod
    def _build_subscribe_message(
        token_ids: list[str],
        *,
        initial_dump: bool,
    ) -> dict[str, object]:
        return {
            "assets_ids": token_ids,
            "type": "market",
            "initial_dump": initial_dump,
            "level": 2,
            "custom_feature_enabled": True,
        }

    async def subscribe(self, token_ids: list[str]) -> None:
        """Subscribe to new token IDs (can be called while connected)."""
        new_ids = [t for t in token_ids if t and t not in self._subscribed_ids]
        if not new_ids:
            return
        self._stats["subscribe_calls"] += 1

        for tid in new_ids:
            self._subscribed_ids.add(tid)
            if tid not in self._books:
                self._books[tid] = TokenBook(token_id=tid)

        if self._ws:
            try:
                await self._ws.send(
                    json.dumps(self._build_subscribe_message(new_ids, initial_dump=True))
                )
                logger.info("Subscribed to %s tokens", len(new_ids))
            except Exception as e:
                logger.warning(f"Subscribe failed: {e}")

    async def unsubscribe(self, token_ids: list[str]) -> None:
        remove_ids = [t for t in token_ids if t and t in self._subscribed_ids]
        if not remove_ids:
            return
        self._stats["unsubscribe_calls"] += 1

        for tid in remove_ids:
            self._subscribed_ids.discard(tid)

        if self._ws:
            msg = {"operation": "unsubscribe", "assets_ids": remove_ids, "level": 2}
            try:
                await self._ws.send(json.dumps(msg))
                logger.info("Unsubscribed from %s tokens", len(remove_ids))
            except Exception as e:
                logger.warning(f"Unsubscribe failed: {e}")

    async def sync_subscriptions(self, token_ids: list[str]) -> None:
        desired = {t for t in token_ids if t}
        to_unsubscribe = sorted(self._subscribed_ids - desired)
        to_subscribe = sorted(desired - self._subscribed_ids)
        if to_unsubscribe:
            await self.unsubscribe(to_unsubscribe)
        if to_subscribe:
            await self.subscribe(to_subscribe)

    async def connect(self) -> None:
        """Connect and maintain WebSocket with auto-reconnect."""
        import websockets

        self._running = True
        while self._running:
            try:
                logger.info(f"Connecting to Polymarket CLOB WebSocket")
                async with websockets.connect(WS_URL) as ws:
                    self._ws = ws
                    self._reconnect_delay = 1.0

                    if self._subscribed_ids:
                        sub = self._build_subscribe_message(
                            sorted(self._subscribed_ids),
                            initial_dump=True,
                        )
                        await ws.send(json.dumps(sub))

                    ping_task = asyncio.create_task(self._ping_loop(ws))
                    logger.info("Polymarket CLOB WebSocket connected")

                    try:
                        async for raw in ws:
                            if raw == "PONG":
                                continue
                            self._handle_message(raw)
                    finally:
                        ping_task.cancel()
                        try:
                            await ping_task
                        except asyncio.CancelledError:
                            pass

            except Exception as e:
                if not self._running:
                    break
                logger.warning(
                    f"Polymarket WS disconnected: {e}. "
                    f"Reconnecting in {self._reconnect_delay:.0f}s..."
                )
                await asyncio.sleep(self._reconnect_delay)
                self._reconnect_delay = min(self._reconnect_delay * 2, 30.0)
            finally:
                self._ws = None

    async def _ping_loop(self, ws: object) -> None:
        while True:
            await asyncio.sleep(10)
            try:
                await ws.send("PING")
            except Exception:
                return

    def _handle_message(self, raw: str) -> None:
        try:
            data = json.loads(raw)
        except json.JSONDecodeError:
            return

        # Initial dump arrives as a list of book snapshots
        if isinstance(data, list):
            for book in data:
                self._process_book(book)
            return

        evt = data.get("event_type")
        if evt == "book":
            self._process_book(data)
        elif evt == "price_change":
            self._process_price_change(data)
        elif evt == "last_trade_price":
            self._process_trade(data)
        elif evt == "best_bid_ask":
            self._process_bba(data)

    def _process_book(self, data: dict) -> None:
        aid = data.get("asset_id", "")
        if aid not in self._books:
            self._books[aid] = TokenBook(token_id=aid)
        book = self._books[aid]

        bids = data.get("bids", [])
        asks = data.get("asks", [])
        book.bids = [
            {
                "price": float(level.get("price", 0) or 0),
                "size": float(level.get("size", level.get("s", 0)) or 0),
            }
            for level in bids
        ]
        book.asks = [
            {
                "price": float(level.get("price", 0) or 0),
                "size": float(level.get("size", level.get("s", 0)) or 0),
            }
            for level in asks
        ]
        if bids:
            book.best_bid = float(bids[0].get("price", 0))
            book.best_bid_size = float(bids[0].get("size", bids[0].get("s", 0)) or 0)
        if asks:
            book.best_ask = float(asks[0].get("price", 0))
            book.best_ask_size = float(asks[0].get("size", asks[0].get("s", 0)) or 0)
        if book.best_bid > 0 and book.best_ask > 0:
            book.spread = book.best_ask - book.best_bid
        book.last_update = time.time()

    def _process_price_change(self, data: dict) -> None:
        changes = data.get("price_changes", data.get("pc", []))
        for pc in changes:
            aid = pc.get("asset_id", pc.get("a", ""))
            if aid not in self._books:
                continue
            book = self._books[aid]
            bb = pc.get("best_bid", pc.get("bb"))
            ba = pc.get("best_ask", pc.get("ba"))
            if bb is not None:
                book.best_bid = float(bb)
            if ba is not None:
                book.best_ask = float(ba)
            if book.best_bid > 0 and book.best_ask > 0:
                book.spread = book.best_ask - book.best_bid
            book.last_update = time.time()

    def _process_trade(self, data: dict) -> None:
        aid = data.get("asset_id", "")
        if aid in self._books:
            self._books[aid].last_trade_price = float(data.get("price", 0))
            self._books[aid].last_update = time.time()

    def _process_bba(self, data: dict) -> None:
        aid = data.get("asset_id", "")
        if aid not in self._books:
            self._books[aid] = TokenBook(token_id=aid)
        book = self._books[aid]
        book.best_bid = float(data.get("best_bid", 0))
        book.best_ask = float(data.get("best_ask", 0))
        book.spread = float(data.get("spread", 0))
        book.last_update = time.time()

    async def close(self) -> None:
        self._running = False
        if self._ws:
            await self._ws.close()
        self._ws = None
