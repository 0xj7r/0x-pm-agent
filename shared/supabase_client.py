"""Minimal Supabase REST client for shared market/snapshot storage."""
from __future__ import annotations

import logging
import os
from dataclasses import dataclass
from typing import Any

import httpx

logger = logging.getLogger(__name__)

DLQ_MAX_SIZE = 500


@dataclass
class SupabaseConfig:
    url: str
    key: str

    @classmethod
    def from_env(cls) -> "SupabaseConfig":
        url = os.getenv("SUPABASE_URL", "").rstrip("/")
        key = os.getenv("SUPABASE_KEY", "")
        if not url or not key:
            raise RuntimeError("SUPABASE_URL and SUPABASE_KEY must be set")
        return cls(url=url, key=key)


class SupabaseClient:
    """Small PostgREST wrapper for the repo's storage schema."""

    def __init__(self, config: SupabaseConfig | None = None) -> None:
        self._config = config or SupabaseConfig.from_env()
        self._http = httpx.Client(
            base_url=f"{self._config.url}/rest/v1",
            headers={
                "apikey": self._config.key,
                "Authorization": f"Bearer {self._config.key}",
                "Content-Type": "application/json",
            },
            timeout=30.0,
        )
        self._dlq: list[dict[str, Any]] = []

    def _upsert(self, table: str, rows: list[dict[str, Any]], on_conflict: str) -> None:
        if not rows:
            return
        resp = self._http.post(
            f"/{table}",
            params={"on_conflict": on_conflict},
            headers={"Prefer": "resolution=merge-duplicates"},
            json=rows,
        )
        resp.raise_for_status()

    def upsert_market(self, row: dict[str, Any]) -> None:
        self._upsert("markets", [row], "market_id")

    def upsert_markets(self, rows: list[dict[str, Any]]) -> None:
        self._upsert("markets", rows, "market_id")

    def upsert_snapshots(self, rows: list[dict[str, Any]]) -> None:
        self._upsert("snapshots", rows, "market_id,time")

    def upsert_trade(self, row: dict[str, Any]) -> None:
        self._upsert("trades", [row], "id")

    def upsert_trade_safe(self, row: dict[str, Any]) -> None:
        """Write a trade row, queuing to DLQ on failure instead of raising."""
        try:
            self._upsert("trades", [row], "id")
        except Exception as e:
            logger.error(f"Supabase write failed, queuing to DLQ: {e}")
            self._dlq.append(row)
            if len(self._dlq) > DLQ_MAX_SIZE:
                self._dlq = self._dlq[-DLQ_MAX_SIZE:]

    def flush_dlq(self) -> int:
        """Retry all queued DLQ writes. Returns count of successfully flushed."""
        if not self._dlq:
            return 0
        still_failed: list[dict[str, Any]] = []
        flushed = 0
        for row in self._dlq:
            try:
                self._upsert("trades", [row], "id")
                flushed += 1
            except Exception:
                still_failed.append(row)
        self._dlq = still_failed
        if flushed:
            logger.info(f"DLQ flush: {flushed} succeeded, {len(still_failed)} remaining")
        return flushed

    def health_check(self) -> bool:
        """Verify Supabase is reachable and schema accepts our trade shape."""
        test_row = {
            "id": "__healthcheck__",
            "coin": "test",
            "strategy": "healthcheck",
            "market_id": "0",
            "direction": "Up",
            "token_price": 0.5,
            "size_usd": 0.0,
            "shares": 0.0,
            "paper": True,
            "underlying_price": 0.0,
            "move_pct": 0.0,
            "created_at": "2000-01-01T00:00:00Z",
        }
        try:
            resp = self._http.post(
                "/trades",
                params={"on_conflict": "id"},
                headers={"Prefer": "resolution=merge-duplicates"},
                json=[test_row],
            )
            resp.raise_for_status()
            self._http.delete("/trades", params={"id": "eq.__healthcheck__"})
            return True
        except Exception as e:
            logger.error(f"Supabase health check failed: {e}")
            return False

    def upsert_strategy_result(self, row: dict[str, Any]) -> None:
        self._upsert("strategy_results", [row], "coin")

    def insert_snapshots_batch(self, rows: list[dict[str, Any]]) -> None:
        """Insert snapshots without upsert (faster for collector)."""
        if not rows:
            return
        resp = self._http.post("/snapshots", json=rows)
        resp.raise_for_status()

    def load_markets(self, coin: str) -> list[dict[str, Any]]:
        rows: list[dict[str, Any]] = []
        offset = 0
        batch_size = 1000
        while True:
            batch = self.load_markets_page(coin, limit=batch_size, offset=offset)
            if not batch:
                break
            rows.extend(batch)
            if len(batch) < batch_size:
                break
            offset += batch_size
        return rows

    def load_markets_page(
        self, coin: str, limit: int = 1000, offset: int = 0
    ) -> list[dict[str, Any]]:
        resp = self._http.get(
            "/markets",
            params={
                "coin": f"eq.{coin}",
                "order": "start_time.asc,market_id.asc",
                "limit": str(limit),
                "offset": str(offset),
            },
        )
        resp.raise_for_status()
        return resp.json()

    def load_trades(self, coin: str | None = None) -> list[dict[str, Any]]:
        """Fetch all trades, optionally filtered by coin."""
        params: dict[str, str] = {"order": "created_at.desc"}
        if coin:
            params["coin"] = f"eq.{coin}"
        resp = self._http.get("/trades", params=params)
        resp.raise_for_status()
        return resp.json()

    def load_trade_stats(self, coin: str | None = None) -> dict[str, Any]:
        """Compute aggregate trade stats from Supabase."""
        trades = self.load_trades(coin)
        total = len(trades)
        resolved = [t for t in trades if t.get("resolved_at")]
        wins = [t for t in resolved if t.get("won")]
        losses = [t for t in resolved if t.get("won") is False]
        total_pnl = sum(t.get("pnl_usd", 0) or 0 for t in resolved)
        total_wagered = sum(t.get("size_usd", 0) or 0 for t in trades)
        win_rate = len(wins) / len(resolved) * 100 if resolved else 0.0
        return {
            "trades_total": total,
            "trades_resolved": len(resolved),
            "wins": len(wins),
            "losses": len(losses),
            "win_rate": round(win_rate, 1),
            "total_pnl": round(total_pnl, 2),
            "total_wagered": round(total_wagered, 2),
            "trades": trades,
        }

    def load_snapshots(self, market_id: str) -> list[dict[str, Any]]:
        rows: list[dict[str, Any]] = []
        offset = 0
        batch_size = 1000
        while True:
            batch = self.load_snapshots_page(market_id, limit=batch_size, offset=offset)
            if not batch:
                break
            rows.extend(batch)
            if len(batch) < batch_size:
                break
            offset += batch_size
        return rows

    def load_snapshots_page(
        self, market_id: str, limit: int = 1000, offset: int = 0
    ) -> list[dict[str, Any]]:
        resp = self._http.get(
            "/snapshots",
            params={
                "market_id": f"eq.{market_id}",
                "order": "time.asc,id.asc",
                "limit": str(limit),
                "offset": str(offset),
            },
        )
        resp.raise_for_status()
        return resp.json()

    def load_coin_snapshots_page(
        self, coin: str, limit: int = 1000, offset: int = 0
    ) -> list[dict[str, Any]]:
        resp = self._http.get(
            "/snapshots",
            params={
                "coin": f"eq.{coin}",
                "order": "time.asc,id.asc",
                "limit": str(limit),
                "offset": str(offset),
            },
        )
        resp.raise_for_status()
        return resp.json()

    def close(self) -> None:
        self._http.close()
