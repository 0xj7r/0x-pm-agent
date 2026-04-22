"""Discover active coin Up/Down markets from the Gamma API.

Supports 5-minute and 15-minute windows. Slug pattern is
{coin}-updown-{5m|15m}-{unix_timestamp} where the timestamp is the
window's start time. We generate candidate slugs for the current and
upcoming windows and query the /events endpoint directly.
"""
from __future__ import annotations

import json
import logging
import re
from datetime import datetime, timedelta, timezone

import httpx

from models.market import MarketWindow
from shared.constants import SLUG_PATTERNS, WINDOW_MINUTES

logger = logging.getLogger(__name__)

SLUG_PREFIX_5M = "btc-updown-5m-"
_PRICE_TO_BEAT_PATTERN_TEMPLATE = (
    r'"ticker":"{slug}","slug":"{slug}".{{0,4000}}?"eventMetadata":{{"priceToBeat":([0-9.]+)'
)


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


def parse_coin_event(
    event: dict, slug_prefix: str, window_minutes: int = 5
) -> MarketWindow | None:
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
        end_time = start_time + timedelta(minutes=window_minutes)
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
        price_to_beat=None,
    )


def parse_btc_event(event: dict) -> MarketWindow | None:
    """Backward-compatible wrapper for BTC events."""
    return parse_coin_event(event, SLUG_PREFIX_5M)


# Backward compatibility
parse_btc_market = parse_btc_event


class MarketWindowScanner:
    """Finds active Up/Down markets by generating slug candidates.

    Supports both 5-minute and 15-minute windows via market_type param.
    """

    def __init__(
        self,
        gamma_url: str = "https://gamma-api.polymarket.com",
        coin: str = "btc",
        market_type: str = "5m",
    ) -> None:
        self._gamma_url = gamma_url
        self._coin = coin.lower()
        self._market_type = market_type
        self._window_minutes = WINDOW_MINUTES[market_type]
        pattern = SLUG_PATTERNS[market_type].get(
            self._coin, SLUG_PATTERNS[market_type]["btc"]
        )
        self._slug_prefix = pattern.replace("{ts}", "")
        self._http = httpx.AsyncClient(timeout=30.0)
        self._polymarket_event_url = "https://polymarket.com/event"
        self._price_to_beat_cache: dict[str, float] = {}

    def _generate_candidate_slugs(self, count: int = 24) -> list[str]:
        """Generate slug candidates for the next N window-aligned windows."""
        now = datetime.now(timezone.utc)
        wm = self._window_minutes
        minutes = (now.minute // wm) * wm
        base = now.replace(minute=minutes, second=0, microsecond=0)

        slugs = []
        for i in range(-1, count):
            ts = base + timedelta(minutes=wm * i)
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
                window = parse_coin_event(
                    data[0], self._slug_prefix, self._window_minutes
                )
                if window:
                    windows.append(window)
                    logger.debug(f"Found window: {window.question}")
            except Exception as e:
                logger.debug(f"Slug query failed for {slug}: {e}")

        return windows

    def _parse_price_to_beat_from_html(self, html: str, slug: str) -> float | None:
        pattern = _PRICE_TO_BEAT_PATTERN_TEMPLATE.format(slug=re.escape(slug))
        match = re.search(pattern, html, flags=re.IGNORECASE | re.DOTALL)
        if not match:
            return None
        return _safe_float(match.group(1), default=0.0) or None

    async def _fetch_price_to_beat(self, slug: str) -> float | None:
        cached = self._price_to_beat_cache.get(slug)
        if cached is not None:
            return cached

        try:
            resp = await self._http.get(f"{self._polymarket_event_url}/{slug}")
            if resp.status_code != 200:
                return None
            price_to_beat = self._parse_price_to_beat_from_html(resp.text, slug)
            if price_to_beat is not None:
                self._price_to_beat_cache[slug] = price_to_beat
            return price_to_beat
        except Exception as e:
            logger.debug("Price To Beat fetch failed for %s: %s", slug, e)
            return None

    async def _enrich_price_to_beat(self, windows: list[MarketWindow]) -> None:
        if not windows:
            return

        now = datetime.now(timezone.utc)
        candidates: list[MarketWindow] = []
        active = [w for w in windows if w.is_active(now)]
        upcoming = sorted(
            [w for w in windows if w.start_time >= now],
            key=lambda w: w.start_time,
        )
        if active:
            candidates.append(active[0])
        if upcoming:
            candidates.append(upcoming[0])

        seen: set[str] = set()
        for window in candidates:
            if not window.slug or window.slug in seen:
                continue
            seen.add(window.slug)
            if window.price_to_beat is None:
                window.price_to_beat = await self._fetch_price_to_beat(window.slug)

    async def get_current_window(self) -> MarketWindow | None:
        now = datetime.now(timezone.utc)
        windows = await self.find_active_windows()
        await self._enrich_price_to_beat(windows)

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
