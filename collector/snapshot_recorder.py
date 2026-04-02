"""Record token price snapshots for a coin's active 5-minute markets.

Polls the Gamma API on a configurable interval and writes price snapshots
to the coin's SQLite database via shared.db.

Usage:
    python collector/snapshot_recorder.py --coin btc
    python collector/snapshot_recorder.py --coin eth --interval 15
"""
from __future__ import annotations

import argparse
import asyncio
import logging
import sys
from datetime import datetime, timezone
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent.parent))

from clients.market_scanner import MarketWindowScanner
from shared.constants import db_path
from shared.db import init_coin_db

logger = logging.getLogger(__name__)


class SnapshotRecorder:
    """Periodically records token prices for active markets."""

    def __init__(self, coin: str = "btc", interval: float = 10.0) -> None:
        self._coin = coin.lower()
        self._interval = interval
        self._scanner = MarketWindowScanner(coin=self._coin)
        self._db_path = db_path(self._coin)
        self._conn = init_coin_db(self._db_path)
        self._running = False

    async def _record_once(self) -> int:
        """Scan for active windows and record a snapshot for each. Returns count."""
        windows = await self._scanner.find_active_windows()
        now = datetime.now(timezone.utc)
        recorded = 0

        for w in windows:
            if not w.is_active(now):
                continue
            try:
                self._conn.execute(
                    "INSERT OR REPLACE INTO snapshots (market_id, time, price_up, price_down) "
                    "VALUES (?, ?, ?, ?)",
                    (w.market_id, now.isoformat(), w.up_price, w.down_price),
                )
                recorded += 1
            except Exception as e:
                logger.warning(f"Failed to record snapshot for {w.market_id}: {e}")

        if recorded:
            self._conn.commit()
        return recorded

    async def run(self) -> None:
        """Run the recording loop until stopped."""
        self._running = True
        logger.info(
            f"Snapshot recorder started: coin={self._coin}, "
            f"interval={self._interval}s, db={self._db_path}"
        )
        while self._running:
            try:
                count = await self._record_once()
                if count:
                    logger.info(f"Recorded {count} snapshot(s)")
            except Exception as e:
                logger.error(f"Recording cycle failed: {e}", exc_info=True)
            await asyncio.sleep(self._interval)

    async def stop(self) -> None:
        self._running = False
        await self._scanner.close()
        self._conn.close()


async def main(coin: str, interval: float) -> None:
    recorder = SnapshotRecorder(coin=coin, interval=interval)
    try:
        await recorder.run()
    except KeyboardInterrupt:
        pass
    finally:
        await recorder.stop()


if __name__ == "__main__":
    logging.basicConfig(
        level=logging.INFO,
        format="%(asctime)s [%(levelname)s] %(name)s: %(message)s",
        datefmt="%Y-%m-%d %H:%M:%S",
    )
    parser = argparse.ArgumentParser(description="Snapshot price recorder")
    parser.add_argument("--coin", default="btc", choices=["btc", "eth", "sol"])
    parser.add_argument("--interval", type=float, default=10.0, help="Seconds between recording cycles")
    args = parser.parse_args()
    asyncio.run(main(args.coin, args.interval))
