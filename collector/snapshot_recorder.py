"""Live snapshot recorder for building historical data.

Uses separate async loops for:
- market discovery and Polymarket subscription updates
- underlying coin price ingestion from Binance
- periodic snapshot production
- batched SQLite writes through a bounded queue

Usage:
    python collector/snapshot_recorder.py --coin btc
    python collector/snapshot_recorder.py --coin eth --coin sol --interval 1.0
"""
from __future__ import annotations

import argparse
import asyncio
import logging
import sys
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent.parent))

from clients.binance_ws import BinanceWSClient, OrderBookSnapshot, TradeUpdate
from clients.market_scanner import MarketWindowScanner
from clients.polymarket_ws import PolymarketWSClient
from models.market import MarketWindow
from shared.constants import COIN_CONFIGS, db_path
from shared.db import init_coin_db
from shared.supabase_client import SupabaseClient, SupabaseConfig

logger = logging.getLogger(__name__)


@dataclass
class SnapshotWrite:
    market_id: str
    slug: str
    question: str
    start_time: str
    end_time: str
    snapshot_time: str
    underlying_price: float | None
    up_price: float
    down_price: float
    bid_price: float | None
    ask_price: float | None
    bid_size: float | None
    ask_size: float | None
    elapsed_s: float


class SnapshotRecorder:
    """Queue-backed live recorder for a single coin."""

    def __init__(
        self,
        coin: str = "btc",
        interval: float = 1.0,
        scan_interval: float = 5.0,
        queue_size: int = 5000,
        batch_size: int = 200,
    ) -> None:
        self._coin = coin.lower()
        self._interval = interval
        self._scan_interval = scan_interval
        self._batch_size = batch_size
        self._scanner = MarketWindowScanner(coin=self._coin)
        self._poly_ws = PolymarketWSClient()
        self._db_path = db_path(self._coin)
        self._conn = init_coin_db(self._db_path)
        self._queue: asyncio.Queue[SnapshotWrite] = asyncio.Queue(maxsize=queue_size)
        self._stop = asyncio.Event()
        self._current_price: float = 0.0
        self._bid_price: float | None = None
        self._ask_price: float | None = None
        self._bid_size: float | None = None
        self._ask_size: float | None = None
        self._active_windows: dict[str, MarketWindow] = {}
        self._windows_seen: set[str] = set()
        try:
            self._supa = SupabaseClient()
        except Exception:
            self._supa = None
            logger.warning("[%s] Supabase not configured, local-only mode", coin.upper())

    async def _on_trade(self, update: TradeUpdate) -> None:
        self._current_price = update.price

    async def _on_book(self, snap: OrderBookSnapshot) -> None:
        self._bid_price = snap.best_bid
        self._ask_price = snap.best_ask
        self._bid_size = snap.bid_size
        self._ask_size = snap.ask_size

    def _live_token_price(self, token_id: str, fallback: float) -> float:
        live = self._poly_ws.get_price(token_id)
        return live if live > 0 else fallback

    def _market_row(self, item: SnapshotWrite) -> dict[str, object]:
        return {
            "coin": self._coin,
            "market_id": item.market_id,
            "slug": item.slug,
            "market_type": "5m",
            "start_time": item.start_time,
            "end_time": item.end_time,
            "price_start": item.underlying_price,
            "price_end": item.underlying_price,
            "winner": None,
            "final_volume": None,
            "final_liquidity": None,
        }

    async def _scan_loop(self) -> None:
        while not self._stop.is_set():
            try:
                now = datetime.now(timezone.utc)
                windows = await self._scanner.find_active_windows()
                active = {w.market_id: w for w in windows if w.is_active(now)}
                self._active_windows = active

                token_ids: list[str] = []
                for window in active.values():
                    token_ids.extend(
                        token_id
                        for token_id in (window.up_token_id, window.down_token_id)
                        if token_id
                    )
                if token_ids:
                    await self._poly_ws.subscribe(token_ids)

                logger.debug(
                    "[%s] active_windows=%s queue=%s",
                    self._coin.upper(),
                    len(active),
                    self._queue.qsize(),
                )
            except Exception as exc:
                logger.error("[%s] scan loop failed: %s", self._coin.upper(), exc, exc_info=True)

            try:
                await asyncio.wait_for(self._stop.wait(), timeout=self._scan_interval)
            except asyncio.TimeoutError:
                pass

    async def _snapshot_loop(self) -> None:
        while not self._stop.is_set():
            try:
                now = datetime.now(timezone.utc)
                produced = 0
                for window in list(self._active_windows.values()):
                    elapsed = (now - window.start_time).total_seconds()
                    up = self._live_token_price(window.up_token_id, window.up_price)
                    down = self._live_token_price(window.down_token_id, window.down_price)
                    write = SnapshotWrite(
                        market_id=window.market_id,
                        slug=window.slug,
                        question=getattr(window, 'question', '') or window.slug,
                        start_time=window.start_time.isoformat(),
                        end_time=window.end_time.isoformat(),
                        snapshot_time=now.isoformat(),
                        underlying_price=self._current_price or None,
                        up_price=up,
                        down_price=down,
                        bid_price=self._bid_price,
                        ask_price=self._ask_price,
                        bid_size=self._bid_size,
                        ask_size=self._ask_size,
                        elapsed_s=round(elapsed, 2),
                    )
                    try:
                        self._queue.put_nowait(write)
                        produced += 1
                    except asyncio.QueueFull:
                        logger.warning(
                            "[%s] snapshot queue full, dropping snapshot for %s",
                            self._coin.upper(),
                            window.market_id,
                        )
                if produced:
                    logger.info(
                        "[%s] enqueued=%s backlog=%s underlying=%.2f",
                        self._coin.upper(),
                        produced,
                        self._queue.qsize(),
                        self._current_price or 0.0,
                    )
            except Exception as exc:
                logger.error(
                    "[%s] snapshot loop failed: %s",
                    self._coin.upper(),
                    exc,
                    exc_info=True,
                )

            try:
                await asyncio.wait_for(self._stop.wait(), timeout=self._interval)
            except asyncio.TimeoutError:
                pass

    def _flush_batch(self, batch: list[SnapshotWrite]) -> None:
        for item in batch:
            self._conn.execute(
                """
                INSERT INTO markets (
                    market_id, slug, market_type, start_time, end_time,
                    price_start, price_end, winner, final_volume, final_liquidity
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                ON CONFLICT(market_id) DO UPDATE SET
                    slug=excluded.slug,
                    market_type=excluded.market_type,
                    start_time=excluded.start_time,
                    end_time=excluded.end_time,
                    price_end=excluded.price_end
                """,
                (
                    item.market_id,
                    item.slug,
                    "5m",
                    item.start_time,
                    item.end_time,
                    item.underlying_price,
                    item.underlying_price,
                    None,
                    None,
                    None,
                ),
            )
            if item.market_id not in self._windows_seen and item.underlying_price is not None:
                self._conn.execute(
                    "UPDATE markets SET price_start = COALESCE(price_start, ?) WHERE market_id = ?",
                    (item.underlying_price, item.market_id),
                )
                self._windows_seen.add(item.market_id)

            self._conn.execute(
                """
                INSERT OR REPLACE INTO snapshots
                (market_id, time, price, price_up, price_down)
                VALUES (?, ?, ?, ?, ?)
                """,
                (
                    item.market_id,
                    item.snapshot_time,
                    item.underlying_price,
                    item.up_price,
                    item.down_price,
                ),
            )
        self._conn.commit()

        if self._supa:
            try:
                market_rows = []
                seen_market_ids: set[str] = set()
                for item in batch:
                    if item.market_id in seen_market_ids:
                        continue
                    seen_market_ids.add(item.market_id)
                    market_rows.append(self._market_row(item))
                self._supa.upsert_markets(market_rows)
                supa_rows = [{
                    "coin": self._coin,
                    "market_id": item.market_id,
                    "time": item.snapshot_time,
                    "price": item.underlying_price,
                    "price_up": item.up_price,
                    "price_down": item.down_price,
                    "bid_price": item.bid_price,
                    "ask_price": item.ask_price,
                    "bid_size": item.bid_size,
                    "ask_size": item.ask_size,
                    "spread": round(item.up_price + item.down_price - 1.0, 4),
                    "elapsed_s": item.elapsed_s,
                } for item in batch]
                self._supa.insert_snapshots_batch(supa_rows)
            except Exception as exc:
                logger.warning("[%s] Supabase write failed: %s", self._coin.upper(), exc)

    async def _writer_loop(self) -> None:
        while not self._stop.is_set() or not self._queue.empty():
            batch: list[SnapshotWrite] = []
            try:
                first = await asyncio.wait_for(self._queue.get(), timeout=1.0)
                batch.append(first)
                while len(batch) < self._batch_size:
                    try:
                        batch.append(self._queue.get_nowait())
                    except asyncio.QueueEmpty:
                        break
            except asyncio.TimeoutError:
                continue

            try:
                self._flush_batch(batch)
            except Exception as exc:
                logger.error("[%s] writer loop failed: %s", self._coin.upper(), exc, exc_info=True)
            finally:
                for _ in batch:
                    self._queue.task_done()

    async def run(self) -> None:
        """Run the recorder until stopped."""
        binance_symbol = COIN_CONFIGS.get(self._coin, COIN_CONFIGS["btc"])["binance_symbol"]
        binance = BinanceWSClient(
            on_trade=self._on_trade,
            on_book_update=self._on_book,
            symbol=binance_symbol,
        )
        tasks = [
            asyncio.create_task(binance.connect(), name=f"{self._coin}-binance"),
            asyncio.create_task(self._poly_ws.connect(), name=f"{self._coin}-polymarket"),
            asyncio.create_task(self._scan_loop(), name=f"{self._coin}-scan"),
            asyncio.create_task(self._snapshot_loop(), name=f"{self._coin}-snapshot"),
            asyncio.create_task(self._writer_loop(), name=f"{self._coin}-writer"),
        ]
        logger.info(
            "Snapshot recorder started: coin=%s interval=%.2fs scan=%.2fs db=%s",
            self._coin,
            self._interval,
            self._scan_interval,
            self._db_path,
        )
        try:
            await asyncio.gather(*tasks)
        finally:
            self._stop.set()
            await binance.close()
            await self._poly_ws.close()
            await self._scanner.close()
            for task in tasks:
                task.cancel()
            await asyncio.gather(*tasks, return_exceptions=True)
            self._conn.close()

    async def stop(self) -> None:
        self._stop.set()


async def main(coins: list[str], interval: float, scan_interval: float) -> None:
    recorders = [
        SnapshotRecorder(coin=coin, interval=interval, scan_interval=scan_interval)
        for coin in coins
    ]
    tasks = [asyncio.create_task(recorder.run()) for recorder in recorders]
    try:
        await asyncio.gather(*tasks)
    except KeyboardInterrupt:
        for recorder in recorders:
            await recorder.stop()
        await asyncio.gather(*tasks, return_exceptions=True)


if __name__ == "__main__":
    logging.basicConfig(
        level=logging.INFO,
        format="%(asctime)s [%(levelname)s] %(name)s: %(message)s",
        datefmt="%Y-%m-%d %H:%M:%S",
    )
    parser = argparse.ArgumentParser(description="Snapshot price recorder")
    parser.add_argument("--coin", action="append", choices=["btc", "eth", "sol"], help="Coin(s) to record")
    parser.add_argument("--interval", type=float, default=1.0, help="Seconds between snapshot production cycles")
    parser.add_argument("--scan-interval", type=float, default=5.0, help="Seconds between market discovery scans")
    args = parser.parse_args()
    asyncio.run(main(args.coin or ["btc"], args.interval, args.scan_interval))
