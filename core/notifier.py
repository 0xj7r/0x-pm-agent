"""Slack webhook notifications for trading events and health status.

The bot notifies directly — no external monitor needed. Sends to a
Slack webhook URL configured via SLACK_WEBHOOK_URL env var.
"""
from __future__ import annotations

import logging
import os
from datetime import datetime, timezone

import httpx

logger = logging.getLogger(__name__)


class SlackNotifier:
    """Sends trading updates and alerts to Slack via incoming webhook."""

    def __init__(self, webhook_url: str | None = None) -> None:
        self._url = webhook_url or os.getenv("SLACK_WEBHOOK_URL", "")
        self._http = httpx.AsyncClient(timeout=10.0)

    @property
    def enabled(self) -> bool:
        return bool(self._url)

    async def _send(self, text: str) -> None:
        if not self.enabled:
            return
        try:
            resp = await self._http.post(self._url, json={"text": text})
            if resp.status_code != 200:
                logger.warning(f"Slack webhook returned {resp.status_code}")
        except Exception as e:
            logger.warning(f"Slack notification failed: {e}")

    async def notify_trade(self, direction: str, token_price: float,
                           size_usd: float, shares: float, p_win: float | None,
                           btc_price: float, balance: float) -> None:
        p_win_str = f"{p_win:.3f}" if p_win is not None else "n/a"
        await self._send(
            f"*Paper Trade Executed*\n"
            f"Direction: {direction} @ ${token_price:.3f}\n"
            f"Size: ${size_usd:.2f} ({shares:.0f} shares)\n"
            f"P({direction}): {p_win_str} | BTC: ${btc_price:,.2f}\n"
            f"Balance: ${balance:.2f}"
        )

    async def notify_resolution(self, won: bool, pnl: float,
                                direction: str, token_price: float,
                                resolved_direction: str, balance: float) -> None:
        status = "WON" if won else "LOST"
        emoji = ":chart_with_upwards_trend:" if won else ":chart_with_downwards_trend:"
        await self._send(
            f"{emoji} *Trade Resolved: {status}*\n"
            f"PnL: ${pnl:+.2f}\n"
            f"Bought {direction} @ ${token_price:.3f} | Resolved: {resolved_direction}\n"
            f"Balance: ${balance:.2f}"
        )

    async def notify_error(self, error: str) -> None:
        await self._send(f":warning: *Bot Error*\n```{error[:500]}```")

    async def notify_startup(self, balance: float, threshold: float,
                             paper: bool) -> None:
        mode = "PAPER" if paper else "LIVE"
        await self._send(
            f":rocket: *Polymarket Bot Started*\n"
            f"Mode: {mode} | Balance: ${balance:.2f}\n"
            f"Threshold: {threshold}"
        )

    async def notify_status(self, balance: float, trades: int,
                            resolved: int, p_up: float | None,
                            uptime_hours: float) -> None:
        p_up_str = f"{p_up:.3f}" if p_up is not None else "n/a"
        await self._send(
            f"*Hourly Status*\n"
            f"Balance: ${balance:.2f} | Trades: {trades} (resolved: {resolved})\n"
            f"P(UP): {p_up_str} | Uptime: {uptime_hours:.1f}h"
        )

    async def close(self) -> None:
        await self._http.aclose()
