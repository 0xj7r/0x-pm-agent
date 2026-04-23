"""Resolve paper trades via Gamma API and calculate P&L.

Extracted from btc_engine.py to keep files focused and under 400 lines.
"""
from __future__ import annotations

import json
import logging

import httpx

from core.btc_resolution import PaperTradeRecord, resolve_paper_trade, ResolutionResult

logger = logging.getLogger(__name__)

GAMMA_URL = "https://gamma-api.polymarket.com"


class PaperTradeResolver:
    """Checks Gamma API for resolved markets and calculates paper P&L."""

    def __init__(self) -> None:
        self._http = httpx.AsyncClient(timeout=30.0)
        self.resolved_ids: set[str] = set()

    async def check_resolution(self, market_id: str) -> dict | None:
        """Check if a single market has resolved via Gamma API.

        When resolved, includes the on-chain `conditionId` so the caller
        can redeem winning positions via the CTF contract.
        """
        try:
            resp = await self._http.get(f"{GAMMA_URL}/markets/{market_id}")
            if resp.status_code != 200:
                return None

            data = resp.json()
            if isinstance(data, list):
                data = data[0] if data else None
            if not data or not data.get("closed", False):
                return None

            outcome_prices_raw = data.get("outcomePrices", "[]")
            prices = json.loads(outcome_prices_raw) if isinstance(outcome_prices_raw, str) else outcome_prices_raw
            if len(prices) < 2:
                return None

            condition_id = data.get("conditionId")

            yes_price = float(prices[0])
            if yes_price >= 0.99:
                return {
                    "resolved": True,
                    "winning_outcome": "Yes",
                    "condition_id": condition_id,
                }
            elif float(prices[1]) >= 0.99:
                return {
                    "resolved": True,
                    "winning_outcome": "No",
                    "condition_id": condition_id,
                }
            return None
        except Exception as e:
            logger.debug(f"Resolution check failed for {market_id}: {e}")
            return None

    async def resolve_trades(self, paper_trades: list[dict]) -> list[tuple[dict, ResolutionResult, str]]:
        """Check all unresolved trades, return list of (trade, result, resolved_direction)."""
        resolved = []
        for trade in paper_trades:
            market_id = trade["market_id"]
            if market_id in self.resolved_ids:
                continue

            result = await self.check_resolution(market_id)
            if not result:
                continue

            winning = result["winning_outcome"]
            resolved_dir = "UP" if winning == "Yes" else "DOWN"

            # Stamp the on-chain conditionId onto the trade so the live
            # redemption path can call redeemPositions without a re-lookup.
            if result.get("condition_id") and not trade.get("condition_id"):
                trade["condition_id"] = result["condition_id"]

            record = PaperTradeRecord(
                trade_id=trade.get("timestamp", market_id),
                market_id=market_id,
                direction=trade["direction"],
                token_price=trade["token_price"],
                size_usd=trade["size_usd"],
                shares=trade["shares"],
            )
            res = resolve_paper_trade(record, resolved_dir)
            self.resolved_ids.add(market_id)
            resolved.append((trade, res, resolved_dir))

        return resolved

    def prune_resolved(self, paper_trades: list[dict]) -> list[dict]:
        """Remove resolved trades from the list."""
        return [t for t in paper_trades if t["market_id"] not in self.resolved_ids]

    async def close(self) -> None:
        await self._http.aclose()
