"""Historical data fetcher for backtesting.

Uses PolyBackTest API for free historical order book data at 1-minute resolution.
Also supports Polymarket's own historical endpoints.
"""

from __future__ import annotations

import logging
from dataclasses import dataclass
from datetime import datetime

import httpx

from models.market import Market, MarketCategory, OrderBook, PricePoint

logger = logging.getLogger(__name__)

POLYBACKTEST_URL = "https://api.polybacktest.com"
GAMMA_URL = "https://gamma-api.polymarket.com"


@dataclass
class HistoricalSnapshot:
    """A point-in-time snapshot of a market."""

    market_id: str
    timestamp: datetime
    yes_price: float
    no_price: float
    yes_book: OrderBook
    no_book: OrderBook
    volume: float = 0.0


@dataclass
class MarketResolution:
    """How a market resolved."""

    market_id: str
    question: str
    resolved_to: str  # "Yes" or "No"
    resolved_at: datetime


class HistoricalDataClient:
    def __init__(self):
        self._http = httpx.AsyncClient(timeout=60.0)

    async def get_resolved_markets(
        self,
        category: MarketCategory | None = None,
        limit: int = 100,
        offset: int = 0,
    ) -> list[MarketResolution]:
        """Fetch resolved markets from Gamma API for backtesting."""
        params = {
            "limit": limit,
            "offset": offset,
            "closed": "true",
        }

        try:
            resp = await self._http.get(f"{GAMMA_URL}/markets", params=params)
            resp.raise_for_status()
            raw_markets = resp.json()

            resolutions = []
            for raw in raw_markets:
                # Only include markets that actually resolved
                outcome = raw.get("outcome")
                if not outcome:
                    continue

                resolved_at = None
                if raw.get("endDate"):
                    try:
                        resolved_at = datetime.fromisoformat(
                            raw["endDate"].replace("Z", "+00:00")
                        )
                    except (ValueError, TypeError):
                        resolved_at = datetime.utcnow()

                resolution = MarketResolution(
                    market_id=str(raw.get("id", "")),
                    question=raw.get("question", ""),
                    resolved_to=outcome,
                    resolved_at=resolved_at or datetime.utcnow(),
                )
                resolutions.append(resolution)

            logger.info(f"Fetched {len(resolutions)} resolved markets")
            return resolutions

        except Exception as e:
            logger.error(f"Failed to fetch resolved markets: {e}")
            return []

    async def get_price_history(
        self,
        market_id: str,
        start: datetime | None = None,
        end: datetime | None = None,
        interval_minutes: int = 60,
    ) -> list[HistoricalSnapshot]:
        """Fetch historical price data for a market.

        Tries PolyBackTest first, falls back to Gamma API.
        """
        snapshots = await self._try_polybacktest(market_id, start, end, interval_minutes)
        if not snapshots:
            snapshots = await self._try_gamma_history(market_id)
        return snapshots

    async def _try_polybacktest(
        self,
        market_id: str,
        start: datetime | None,
        end: datetime | None,
        interval_minutes: int,
    ) -> list[HistoricalSnapshot]:
        """Try fetching from PolyBackTest API."""
        try:
            params = {"market_id": market_id, "interval": interval_minutes}
            if start:
                params["start"] = start.isoformat()
            if end:
                params["end"] = end.isoformat()

            resp = await self._http.get(f"{POLYBACKTEST_URL}/v1/snapshots", params=params)
            resp.raise_for_status()
            data = resp.json()

            snapshots = []
            for point in data.get("snapshots", []):
                timestamp_raw = point.get("timestamp")
                if not timestamp_raw:
                    continue

                timestamp = self._parse_timestamp(timestamp_raw)
                if not timestamp:
                    continue

                snapshot = HistoricalSnapshot(
                    market_id=market_id,
                    timestamp=timestamp,
                    yes_price=self._safe_float(point.get("yes_price"), 0.5),
                    no_price=self._safe_float(point.get("no_price"), 0.5),
                    yes_book=OrderBook(
                        bids=[
                            PricePoint(
                                self._safe_float(p.get("price"), 0.0),
                                self._safe_float(p.get("size"), 0.0),
                            )
                            for p in point.get("yes_bids", [])
                            if isinstance(p, dict)
                        ],
                        asks=[
                            PricePoint(
                                self._safe_float(p.get("price"), 0.0),
                                self._safe_float(p.get("size"), 0.0),
                            )
                            for p in point.get("yes_asks", [])
                            if isinstance(p, dict)
                        ],
                    ),
                    no_book=OrderBook(
                        bids=[
                            PricePoint(
                                self._safe_float(p.get("price"), 0.0),
                                self._safe_float(p.get("size"), 0.0),
                            )
                            for p in point.get("no_bids", [])
                            if isinstance(p, dict)
                        ],
                        asks=[
                            PricePoint(
                                self._safe_float(p.get("price"), 0.0),
                                self._safe_float(p.get("size"), 0.0),
                            )
                            for p in point.get("no_asks", [])
                            if isinstance(p, dict)
                        ],
                    ),
                    volume=self._safe_float(point.get("volume"), 0.0),
                )
                snapshots.append(snapshot)

            return snapshots

        except Exception as e:
            logger.debug(f"PolyBackTest unavailable for {market_id}: {e}")
            return []

    async def _try_gamma_history(self, market_id: str) -> list[HistoricalSnapshot]:
        """Fallback: fetch price history from Gamma API."""
        try:
            resp = await self._http.get(f"{GAMMA_URL}/markets/{market_id}/history")
            resp.raise_for_status()
            data = resp.json()

            snapshots = []
            for point in data.get("history", []):
                timestamp_raw = point.get("t")
                timestamp = self._parse_timestamp(timestamp_raw) if timestamp_raw else None
                if not timestamp:
                    continue

                yes_price = self._safe_float(point.get("p"), 0.5)
                snapshot = HistoricalSnapshot(
                    market_id=market_id,
                    timestamp=timestamp,
                    yes_price=yes_price,
                    no_price=1.0 - yes_price,
                    yes_book=OrderBook(),
                    no_book=OrderBook(),
                )
                snapshots.append(snapshot)

            return snapshots

        except Exception as e:
            logger.debug(f"Gamma history unavailable for {market_id}: {e}")
            return []

    async def close(self):
        await self._http.aclose()

    @staticmethod
    def _safe_float(value, default: float) -> float:
        try:
            return float(value)
        except (TypeError, ValueError):
            return default

    @staticmethod
    def _parse_timestamp(value: str) -> datetime | None:
        if not value:
            return None
        try:
            return datetime.fromisoformat(value.replace("Z", "+00:00"))
        except (TypeError, ValueError):
            return None
