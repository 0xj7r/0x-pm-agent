"""Minimal Supabase REST client for shared market/snapshot storage."""
from __future__ import annotations

import os
from dataclasses import dataclass
from typing import Any

import httpx


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

    def upsert_strategy_result(self, row: dict[str, Any]) -> None:
        self._upsert("strategy_results", [row], "coin")

    def insert_snapshots_batch(self, rows: list[dict[str, Any]]) -> None:
        """Insert snapshots without upsert (faster for collector)."""
        if not rows:
            return
        resp = self._http.post("/snapshots", json=rows)
        resp.raise_for_status()

    def load_markets(self, coin: str) -> list[dict[str, Any]]:
        resp = self._http.get(
            "/markets",
            params={"coin": f"eq.{coin}", "order": "start_time.asc"},
        )
        resp.raise_for_status()
        return resp.json()

    def load_snapshots(self, market_id: str) -> list[dict[str, Any]]:
        resp = self._http.get(
            "/snapshots",
            params={"market_id": f"eq.{market_id}", "order": "time.asc"},
        )
        resp.raise_for_status()
        return resp.json()

    def close(self) -> None:
        self._http.close()
