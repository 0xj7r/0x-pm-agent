"""Binance WebSocket client for real-time BTC/USDT trade and order book data.

Connects to Binance's public WebSocket streams. Provides trade-by-trade
updates with buyer/seller classification (isBuyerMaker field) and
order book snapshots with microprice calculation.

Reconnects automatically with exponential backoff on disconnect.
"""
from __future__ import annotations

import asyncio
import json
import logging
import time
from dataclasses import dataclass
from typing import Awaitable, Callable

logger = logging.getLogger(__name__)

BINANCE_WS_URL = "wss://stream.binance.com:9443/ws/btcusdt@trade"


@dataclass
class TradeUpdate:
    price: float
    quantity: float
    is_buyer_maker: bool
    timestamp_ms: int

    @property
    def is_buy(self) -> bool:
        return not self.is_buyer_maker

    @staticmethod
    def from_raw(raw: dict) -> TradeUpdate:
        return TradeUpdate(
            price=float(raw["p"]),
            quantity=float(raw["q"]),
            is_buyer_maker=raw["m"],
            timestamp_ms=raw.get("T", raw.get("E", 0)),
        )


@dataclass
class OrderBookSnapshot:
    best_bid: float
    best_ask: float
    bid_size: float
    ask_size: float
    timestamp_ms: int

    @property
    def mid(self) -> float:
        return (self.best_bid + self.best_ask) / 2

    @property
    def microprice(self) -> float:
        total = self.bid_size + self.ask_size
        if total == 0:
            return self.mid
        return (self.bid_size * self.best_ask + self.ask_size * self.best_bid) / total


class BinanceWSClient:
    """Async Binance WebSocket client with auto-reconnect."""

    def __init__(
        self,
        on_trade: Callable[[TradeUpdate], Awaitable[None]] | None = None,
        url: str = BINANCE_WS_URL,
    ) -> None:
        self._on_trade = on_trade
        self._url = url
        self._ws = None
        self._running = False
        self._last_message_time: float = 0.0
        self._reconnect_delay: float = 1.0
        self._max_reconnect_delay: float = 30.0

    async def connect(self) -> None:
        """Connect and start receiving messages. Reconnects on failure."""
        import websockets

        self._running = True
        while self._running:
            try:
                logger.info(f"Connecting to Binance WebSocket: {self._url}")
                async with websockets.connect(self._url) as ws:
                    self._ws = ws
                    self._reconnect_delay = 1.0
                    logger.info("Binance WebSocket connected")
                    async for message in ws:
                        self._last_message_time = time.time()
                        await self._handle_message(message)
            except Exception as e:
                if not self._running:
                    break
                logger.warning(
                    f"Binance WebSocket disconnected: {e}. "
                    f"Reconnecting in {self._reconnect_delay:.0f}s..."
                )
                await asyncio.sleep(self._reconnect_delay)
                self._reconnect_delay = min(
                    self._reconnect_delay * 2, self._max_reconnect_delay
                )

    async def _handle_message(self, raw_msg: str) -> None:
        try:
            data = json.loads(raw_msg)
        except json.JSONDecodeError:
            return

        event_type = data.get("e")
        if event_type == "trade" and self._on_trade:
            update = TradeUpdate.from_raw(data)
            await self._on_trade(update)

    @property
    def seconds_since_last_message(self) -> float:
        if self._last_message_time == 0:
            return float("inf")
        return time.time() - self._last_message_time

    async def close(self) -> None:
        self._running = False
        if self._ws:
            await self._ws.close()
