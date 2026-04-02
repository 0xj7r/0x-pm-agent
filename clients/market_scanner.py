"""Discover active coin Up/Down 5-minute markets from the Gamma API.

The 5-minute markets use slug pattern: {coin}-updown-5m-{unix_timestamp}
where the timestamp is the window's start time. We generate candidate slugs
for the current and upcoming windows and query the /events endpoint directly.
"""
from __future__ import annotations

import json
import logging
from datetime import datetime, timedelta, timezone

import httpx

from models.market import MarketWindow
from shared.constants import SLUG_PATTERNS

logger = logging.getLogger(__name__)

SLUG_PREFIX_5M = "btc-updown-5m-"


def _safe_float(value: str | float | None, default: float = 0.0) -> float:
    try:
        return float(value)
    except (TypeError, ValueError):
        return default


def _parse_iso(raw: str | None) -> datetime | None:
    if not raw:
        return None
    try:
        return datetime.fromisoformat(raw.replace("Z", "+00:00"))
    except (ValueError, TypeError):
        return None


def parse_coin_event(event: dict, slug_prefix: str) -> MarketWindow | None:
    """Parse a Gamma API event into a MarketWindow."""
    title = event.get("title", "")
    slug = event.get("slug", "")

    if not slug.startswith(slug_prefix):
        return None

    if event.get("closed", False) or not event.get("active", True):
        return None

    # Extract start timestamp from slug
    try:
        start_ts = int(slug.replace(slug_prefix, ""))
        start_time = datetime.fromtimestamp(start_ts, tz=timezone.utc)
        end_time = start_time + timedelta(minutes=5)
    except (ValueError, OSError):
        return None

    markets = event.get("markets", [])
    if not markets:
        return None

    market = markets[0]

    up_token_id = ""
    down_token_id = ""
    up_price = 0.5
    down_price = 0.5

    # Try tokens array first
    for tok in market.get("tokens", []):
        outcome = tok.get("outcome", "").lower()
        token_id = tok.get("token_id", "")
        price = _safe_float(tok.get("price"), 0.5)
        if outcome in ("up", "yes"):
            up_token_id = token_id
            up_price = price
        elif outcome in ("down", "no"):
            down_token_id = token_id
            down_price = price

    # Fallback: clobTokenIds
    if not up_token_id or not down_token_id:
        clob_raw = market.get("clobTokenIds", "[]")
        try:
            clob_ids = json.loads(clob_raw) if isinstance(clob_raw, str) else clob_raw
            if len(clob_ids) >= 2:
                up_token_id = up_token_id or clob_ids[0]
                down_token_id = down_token_id or clob_ids[1]
        except (json.JSONDecodeError, ValueError):
            pass

    # Fallback: outcomePrices
    if up_price == 0.5 and down_price == 0.5:
        prices_raw = market.get("outcomePrices", "[]")
        try:
            prices = json.loads(prices_raw) if isinstance(prices_raw, str) else prices_raw
            if len(prices) >= 2:
                up_price = _safe_float(prices[0], 0.5)
                down_price = _safe_float(prices[1], 0.5)
        except (json.JSONDecodeError, ValueError):
            pass

    if not up_token_id or not down_token_id:
        return None

    return MarketWindow(
        market_id=str(market.get("id", event.get("id", ""))),
        slug=slug,
        question=title,
        start_time=start_time,
        end_time=end_time,
        up_token_id=up_token_id,
        down_token_id=down_token_id,
        up_price=up_price,
        down_price=down_price,
    )


def parse_btc_event(event: dict) -> MarketWindow | None:
    """Backward-compatible wrapper for BTC events."""
    return parse_coin_event(event, SLUG_PREFIX_5M)


# Backward compatibility
parse_btc_market = parse_btc_event


class MarketWindowScanner:
    """Finds active Up/Down 5-minute markets by generating slug candidates."""

    def __init__(
        self,
        gamma_url: str = "https://gamma-api.polymarket.com",
        coin: str = "btc",
    ) -> None:
        self._gamma_url = gamma_url
        self._coin = coin.lower()
        pattern = SLUG_PATTERNS.get(self._coin, SLUG_PATTERNS["btc"])
        self._slug_prefix = pattern.replace("{ts}", "")
        self._http = httpx.AsyncClient(timeout=30.0)

    def _generate_candidate_slugs(self, count: int = 24) -> list[str]:
        """Generate slug candidates for the next N five-minute windows."""
        now = datetime.now(timezone.utc)
        # Round down to nearest 5-minute boundary
        minutes = (now.minute // 5) * 5
        base = now.replace(minute=minutes, second=0, microsecond=0)

        slugs = []
        for i in range(-1, count):
            ts = base + timedelta(minutes=5 * i)
            slugs.append(f"{self._slug_prefix}{int(ts.timestamp())}")
        return slugs

    async def find_active_windows(self) -> list[MarketWindow]:
        """Query Gamma API for each candidate slug, return found windows."""
        slugs = self._generate_candidate_slugs()
        windows = []

        for slug in slugs:
            try:
                resp = await self._http.get(
                    f"{self._gamma_url}/events",
                    params={"slug": slug},
                )
                if resp.status_code != 200:
                    continue
                data = resp.json()
                if not isinstance(data, list) or not data:
                    continue
                window = parse_coin_event(data[0], self._slug_prefix)
                if window:
                    windows.append(window)
                    logger.debug(f"Found window: {window.question}")
            except Exception as e:
                logger.debug(f"Slug query failed for {slug}: {e}")

        return windows

    async def get_current_window(self) -> MarketWindow | None:
        now = datetime.now(timezone.utc)
        windows = await self.find_active_windows()

        for w in windows:
            if w.is_active(now):
                return w

        upcoming = [w for w in windows if w.start_time > now]
        if upcoming:
            upcoming.sort(key=lambda w: w.start_time)
            return upcoming[0]

        return None

    async def close(self) -> None:
        await self._http.aclose()
