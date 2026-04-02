"""Health + dashboard HTTP server for remote monitoring.

Runs alongside the trading engine on port 8080. Exposes:
  /health      - JSON status for monitoring agents
  /api/dashboard - JSON trade data for the dashboard
  /             - Trading dashboard UI
"""
from __future__ import annotations

import asyncio
import json
import logging
import sqlite3
import time
from pathlib import Path
from aiohttp import web

logger = logging.getLogger(__name__)

DASHBOARD_HTML = Path(__file__).parent.parent / "dashboard.html"


class HealthServer:
    """Serves health status and trading dashboard over HTTP."""

    def __init__(self, port: int = 8080, db_path: str = "") -> None:
        self._port = port
        self._db_path = db_path
        self._status: dict = {
            "started_at": time.time(),
            "container": "running",
            "coin": "",
            "balance": 0.0,
            "starting_balance": 100.0,
            "trades_total": 0,
            "trades_resolved": 0,
            "wins": 0,
            "losses": 0,
            "win_rate": 0.0,
            "total_pnl": 0.0,
            "last_signal_time": None,
            "last_trade_time": None,
            "binance_connected": False,
            "binance_last_msg_age_s": None,
            "current_window": None,
            "errors_last_hour": 0,
            "last_error": None,
        }
        self._error_timestamps: list[float] = []
        self._app = web.Application()
        self._app.router.add_get("/health", self._handle_health)
        self._app.router.add_get("/api/dashboard", self._handle_dashboard)
        self._app.router.add_get("/", self._handle_ui)
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

    async def _handle_dashboard(self, request: web.Request) -> web.Response:
        """Return full dashboard data: status + trades + events."""
        self._status["uptime_seconds"] = round(time.time() - self._status["started_at"], 1)

        data = {
            "status": dict(self._status),
            "trades": [],
            "events": [],
            "balance_history": [],
        }

        if self._db_path:
            try:
                conn = sqlite3.connect(self._db_path)
                conn.row_factory = sqlite3.Row

                # Recent trades with results
                trades = conn.execute("""
                    SELECT t.id, t.market_id, t.market_question, t.outcome, t.side,
                           t.size_usd, t.price, t.paper, t.created_at,
                           r.won, r.pnl_usd, r.resolved_at
                    FROM trades t
                    LEFT JOIN results r ON t.id = r.trade_id
                    ORDER BY t.created_at DESC LIMIT 100
                """).fetchall()
                data["trades"] = [dict(t) for t in trades]

                # Recent events
                events = conn.execute("""
                    SELECT window_id, timestamp, event_type, btc_price, details
                    FROM event_log
                    ORDER BY id DESC LIMIT 200
                """).fetchall()
                data["events"] = [dict(e) for e in events]

                # Balance history from portfolio snapshots
                snapshots = conn.execute("""
                    SELECT balance_usd, realized_pnl, num_trades, win_rate, created_at
                    FROM portfolio_snapshots
                    ORDER BY created_at DESC LIMIT 500
                """).fetchall()
                data["balance_history"] = [dict(s) for s in snapshots]

                conn.close()
            except Exception as e:
                data["db_error"] = str(e)

        return web.json_response(data, headers={"Access-Control-Allow-Origin": "*"})

    async def _handle_ui(self, request: web.Request) -> web.Response:
        if DASHBOARD_HTML.exists():
            return web.Response(
                text=DASHBOARD_HTML.read_text(),
                content_type="text/html",
            )
        return web.Response(text="Dashboard not found. Place dashboard.html in project root.")

    async def start(self) -> None:
        self._runner = web.AppRunner(self._app)
        await self._runner.setup()
        site = web.TCPSite(self._runner, "0.0.0.0", self._port)
        await site.start()
        logger.info(f"Health endpoint running on port {self._port}")

    async def stop(self) -> None:
        if self._runner:
            await self._runner.cleanup()
