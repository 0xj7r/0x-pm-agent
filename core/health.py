"""Lightweight health HTTP endpoint for remote monitoring.

Runs alongside the trading engine on port 8080. Exposes /health
as JSON for the scheduled monitoring agent to curl.
"""
from __future__ import annotations

import asyncio
import json
import logging
import time
from aiohttp import web

logger = logging.getLogger(__name__)


class HealthServer:
    """Serves health status over HTTP for remote monitoring."""

    def __init__(self, port: int = 8080) -> None:
        self._port = port
        self._status: dict = {
            "started_at": time.time(),
            "container": "running",
            "balance": 0.0,
            "trades_total": 0,
            "trades_resolved": 0,
            "win_rate": 0.0,
            "last_signal_time": None,
            "last_trade_time": None,
            "p_up": 0.5,
            "binance_connected": False,
            "binance_last_msg_age_s": None,
            "current_window": None,
            "errors_last_hour": 0,
            "last_error": None,
        }
        self._error_timestamps: list[float] = []
        self._app = web.Application()
        self._app.router.add_get("/health", self._handle_health)
        self._runner: web.AppRunner | None = None

    def update(self, **kwargs: object) -> None:
        for k, v in kwargs.items():
            if k in self._status:
                self._status[k] = v

    def record_error(self, error_msg: str) -> None:
        now = time.time()
        self._error_timestamps.append(now)
        cutoff = now - 3600
        self._error_timestamps = [t for t in self._error_timestamps if t > cutoff]
        self._status["errors_last_hour"] = len(self._error_timestamps)
        self._status["last_error"] = error_msg

    async def _handle_health(self, request: web.Request) -> web.Response:
        self._status["uptime_seconds"] = round(time.time() - self._status["started_at"], 1)
        return web.json_response(self._status)

    async def start(self) -> None:
        self._runner = web.AppRunner(self._app)
        await self._runner.setup()
        site = web.TCPSite(self._runner, "0.0.0.0", self._port)
        await site.start()
        logger.info(f"Health endpoint running on port {self._port}")

    async def stop(self) -> None:
        if self._runner:
            await self._runner.cleanup()
