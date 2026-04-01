"""Discover active Bitcoin Up/Down 5-minute markets from the Gamma API.

Polls the Gamma API for markets matching the BTC Up/Down slug pattern,
parses them into MarketWindow objects, and identifies the current
tradeable window.
"""
from __future__ import annotations

import logging
import re
from datetime import datetime, timezone

import httpx

from models.market import MarketWindow

logger = logging.getLogger(__name__)

BTC_KEYWORDS = ["bitcoin up or down", "btc up or down"]


def _safe_float(value: str | float | None, default: float = 0.0) -> float:
    try:
        return float(value)
    except (TypeError, ValueError):
        return default


def _parse_end_date(raw: str | None) -> datetime | None:
    if not raw:
        return None
    try:
        return datetime.fromisoformat(raw.replace("Z", "+00:00"))
    except (ValueError, TypeError):
        return None


def parse_btc_market(raw: dict) -> MarketWindow | None:
    """Parse a raw Gamma API market into a MarketWindow, or None if not a BTC Up/Down market."""
    question = raw.get("question", "")
    if not any(kw in question.lower() for kw in BTC_KEYWORDS):
        return None

    if raw.get("closed", False) or not raw.get("active", True):
        return None

    end_date = _parse_end_date(raw.get("endDate"))
    if not end_date:
        return None

    tokens = raw.get("tokens", [])
    up_token_id = ""
    down_token_id = ""
    up_price = 0.5
    down_price = 0.5

    for tok in tokens:
        outcome = tok.get("outcome", "").lower()
        token_id = tok.get("token_id", "")
        price = _safe_float(tok.get("price"), 0.5)

        if outcome in ("up", "yes"):
            up_token_id = token_id
            up_price = price
        elif outcome in ("down", "no"):
            down_token_id = token_id
            down_price = price

    if not up_token_id or not down_token_id:
        return None

    # Estimate start_time as 5 minutes before end
    from datetime import timedelta
    start_time = end_date - timedelta(minutes=5)

    return MarketWindow(
        market_id=raw.get("id", ""),
        question=question,
        start_time=start_time,
        end_time=end_date,
        up_token_id=up_token_id,
        down_token_id=down_token_id,
        up_price=up_price,
        down_price=down_price,
    )


class MarketWindowScanner:
    """Polls Gamma API for active BTC Up/Down 5-minute markets."""

    def __init__(self, gamma_url: str = "https://gamma-api.polymarket.com") -> None:
        self._gamma_url = gamma_url
        self._http = httpx.AsyncClient(timeout=30.0)

    async def _fetch_btc_markets(self) -> list[dict]:
        """Fetch markets from Gamma API that match BTC Up/Down pattern."""
        try:
            resp = await self._http.get(
                f"{self._gamma_url}/markets",
                params={
                    "limit": 50,
                    "active": "true",
                    "closed": "false",
                },
            )
            resp.raise_for_status()
            all_markets = resp.json()
            return [
                m for m in all_markets
                if any(kw in m.get("question", "").lower() for kw in BTC_KEYWORDS)
            ]
        except Exception as e:
            logger.warning(f"Failed to fetch BTC markets: {e}")
            return []

    async def find_active_windows(self) -> list[MarketWindow]:
        """Find all active BTC Up/Down market windows."""
        raw_markets = await self._fetch_btc_markets()
        windows = []
        for raw in raw_markets:
            window = parse_btc_market(raw)
            if window:
                windows.append(window)
        return windows

    async def get_current_window(self) -> MarketWindow | None:
        """Get the currently active (tradeable) market window."""
        now = datetime.now(timezone.utc)
        windows = await self.find_active_windows()

        # Find a window where now is between start and end
        for w in windows:
            if w.is_active(now):
                return w

        # If none active, return the soonest upcoming window
        upcoming = [w for w in windows if w.start_time > now]
        if upcoming:
            upcoming.sort(key=lambda w: w.start_time)
            return upcoming[0]

        return None

    async def get_next_window(self, current_id: str) -> MarketWindow | None:
        """Get the next window after the current one."""
        windows = await self.find_active_windows()
        windows.sort(key=lambda w: w.start_time)
        for i, w in enumerate(windows):
            if w.market_id == current_id and i + 1 < len(windows):
                return windows[i + 1]
        return None

    async def close(self) -> None:
        await self._http.aclose()
