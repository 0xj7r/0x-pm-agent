from __future__ import annotations

"""Helpers for whale-copy activity ingestion.

This module intentionally keeps ingestion pure and lightweight: it polls the Polymarket
Data API activity endpoint for configured wallets, normalises candidate BUY events, and
returns deduplicated trade signals that can be consumed by the execution engine.
"""

from __future__ import annotations

from dataclasses import dataclass
import time
from typing import Any

import httpx


DATA_API_DEFAULT = "https://data-api.polymarket.com"
DEFAULT_ACTIVITY_LIMIT = 500


@dataclass(frozen=True)
class WhaleCopySignal:
    event_id: str
    wallet: str
    slug: str
    condition_id: str
    event_ts: float
    direction: str
    usdc_size: float
    size: float
    price: float
    raw: dict[str, Any]

    @property
    def event_ts_ms(self) -> int:
        return int(self.event_ts * 1000.0)


def _safe_float(value: Any, default: float = 0.0) -> float:
    try:
        return float(value)
    except (TypeError, ValueError):
        return default


def _safe_int(value: Any, default: int = 0) -> int:
    try:
        return int(value)
    except (TypeError, ValueError):
        return default


def _event_direction(row: dict[str, Any]) -> str | None:
    value = str(row.get("outcome") or row.get("side") or "").strip().lower()
    if value in {"up", "yes", "true", "1", "long"}:
        return "UP"
    if value in {"down", "no", "false", "0", "short"}:
        return "DOWN"
    return None


def _compose_event_id(wallet: str, row: dict[str, Any]) -> str:
    tx = row.get("transactionHash")
    if tx:
        return f"{wallet}:{tx}"
    slug = str(row.get("slug") or row.get("eventSlug") or "")
    return "|".join(
        [
            wallet,
            str(row.get("type") or ""),
            str(row.get("timestamp") or ""),
            slug,
            str(row.get("conditionId") or ""),
            str(row.get("size") or ""),
            str(row.get("price") or ""),
        ]
    )


class WhaleCopySignalIngestor:
    """Polling ingestion for whale activity signals."""

    def __init__(
        self,
        *,
        wallets: list[str],
        api_base: str = DATA_API_DEFAULT,
        poll_interval_seconds: float = 45.0,
        event_ttl_seconds: float = 900.0,
        lookback_seconds: float = 3600.0,
        slug_prefix: str = "btc-updown-5m-",
        limit: int = DEFAULT_ACTIVITY_LIMIT,
        timeout_seconds: float = 15.0,
    ) -> None:
        self.wallets = [w.lower() for w in wallets if w]
        self.api_base = api_base.rstrip("/")
        self.poll_interval_seconds = max(1.0, float(poll_interval_seconds))
        self.event_ttl_seconds = max(10.0, float(event_ttl_seconds))
        self.lookback_seconds = max(60.0, float(lookback_seconds))
        self.slug_prefix = slug_prefix
        self.limit = int(limit) if limit > 0 else DEFAULT_ACTIVITY_LIMIT
        self.timeout_seconds = max(1.0, float(timeout_seconds))
        self._last_poll_ts = 0.0
        self._seen_event_ids: set[str] = set()

    @property
    def enabled(self) -> bool:
        return bool(self.wallets)

    async def poll(self) -> list[WhaleCopySignal]:
        now = time.time()
        if not self.enabled:
            return []
        if now - self._last_poll_ts < self.poll_interval_seconds:
            return []
        self._last_poll_ts = now

        cutoff_ts = int(now - max(self.lookback_seconds, self.event_ttl_seconds))
        seen_local: list[WhaleCopySignal] = []

        async with httpx.AsyncClient(timeout=self.timeout_seconds) as client:
            for wallet in self.wallets:
                try:
                    resp = await client.get(
                        f"{self.api_base}/activity",
                        params={
                            "user": wallet,
                            "limit": str(self.limit),
                            "sortBy": "TIMESTAMP",
                            "sortDirection": "DESC",
                            "start": str(cutoff_ts),
                        },
                    )
                    resp.raise_for_status()
                except Exception:
                    continue

                payload = resp.json()
                if not isinstance(payload, list):
                    continue
                for row in payload:
                    if not isinstance(row, dict):
                        continue
                    signal = self._normalize_row(wallet, row)
                    if signal is None:
                        continue
                    if now - signal.event_ts > self.event_ttl_seconds:
                        continue
                    if signal.event_id in self._seen_event_ids:
                        continue
                    self._seen_event_ids.add(signal.event_id)
                    seen_local.append(signal)

        return seen_local

    def _normalize_row(self, wallet: str, row: dict[str, Any]) -> WhaleCopySignal | None:
        typ = str(row.get("type") or "").upper()
        if typ != "TRADE":
            return None
        direction = _event_direction(row)
        if direction not in {"UP", "DOWN"}:
            return None

        side = str(row.get("side") or "").upper()
        if side and side != "BUY":
            return None

        slug = str(row.get("slug") or row.get("eventSlug") or "").strip()
        if not slug:
            return None
        if self.slug_prefix and not slug.startswith(self.slug_prefix):
            return None

        event_ts = _safe_int(row.get("timestamp"), 0)
        if event_ts <= 0:
            return None

        price = _safe_float(row.get("price"), 0.0)
        if price <= 0:
            return None

        usdc_size = _safe_float(row.get("usdcSize"), 0.0)
        size = _safe_float(row.get("size"), 0.0)
        if usdc_size <= 0 and size <= 0:
            return None

        condition_id = str(row.get("conditionId") or row.get("conditionID") or "").strip()
        event_id = _compose_event_id(wallet, row)

        return WhaleCopySignal(
            event_id=event_id,
            wallet=wallet,
            slug=slug,
            condition_id=condition_id,
            event_ts=float(event_ts),
            direction=direction,
            usdc_size=usdc_size,
            size=size,
            price=price,
            raw=row,
        )
