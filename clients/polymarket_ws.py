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
    best_ask: float = 0.0
    spread: float = 0.0
    last_trade_price: float = 0.0
    last_update: float = 0.0


class PolymarketWSClient:
    """Real-time order book data from Polymarket CLOB WebSocket."""

    def __init__(self) -> None:
        self._ws = None
        self._running = False
        self._subscribed_ids: set[str] = set()
        self._books: dict[str, TokenBook] = {}
        self._reconnect_delay = 1.0

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

    async def subscribe(self, token_ids: list[str]) -> None:
        """Subscribe to new token IDs (can be called while connected)."""
        new_ids = [t for t in token_ids if t and t not in self._subscribed_ids]
        if not new_ids:
            return

        for tid in new_ids:
            self._subscribed_ids.add(tid)
            if tid not in self._books:
                self._books[tid] = TokenBook(token_id=tid)

        if self._ws:
            msg = {"operation": "subscribe", "assets_ids": new_ids, "level": 2}
            try:
                await self._ws.send(json.dumps(msg))
                logger.info(f"Subscribed to {len(new_ids)} tokens")
            except Exception as e:
                logger.warning(f"Subscribe failed: {e}")

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
                        sub = {
                            "assets_ids": list(self._subscribed_ids),
                            "type": "market",
                            "initial_dump": True,
                            "level": 2,
                            "custom_feature_enabled": True,
                        }
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

            except Exception as e:
                if not self._running:
                    break
                logger.warning(
                    f"Polymarket WS disconnected: {e}. "
                    f"Reconnecting in {self._reconnect_delay:.0f}s..."
                )
                self._ws = None
                await asyncio.sleep(self._reconnect_delay)
                self._reconnect_delay = min(self._reconnect_delay * 2, 30.0)

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
        if bids:
            book.best_bid = float(bids[0].get("price", 0))
        if asks:
            book.best_ask = float(asks[0].get("price", 0))
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
