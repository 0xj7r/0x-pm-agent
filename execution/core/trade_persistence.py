"""Persistence helpers for engine trade lifecycle events."""
from __future__ import annotations

import logging
from datetime import datetime, timezone
from uuid import uuid4

from core.memory import MemoryStore

logger = logging.getLogger(__name__)


class TradePersistence:
    def __init__(self, coin: str, memory: MemoryStore, supabase) -> None:
        self._coin = coin
        self._memory = memory
        self._supabase = supabase

    def _ensure_trade_id(self, strategy_name: str, trade: dict) -> str:
        trade_id = trade.get("id")
        if trade_id:
            return str(trade_id)

        # Generate one immutable execution ID at entry time and reuse it for resolution.
        trade_id = f"{self._coin}-{strategy_name}-{trade['market_id']}-{uuid4().hex}"
        trade["id"] = trade_id
        return trade_id

    def ensure_trade_id(self, strategy_name: str, trade: dict) -> str:
        """Public wrapper so the engine can pre-assign an ID before order submit."""
        return self._ensure_trade_id(strategy_name, trade)

    def record_order_submit(self, market_id: str, payload: dict) -> None:
        """Record an order submission attempt (live mode telemetry)."""
        self._memory.save_event(
            window_id=market_id,
            event_type="order_submit",
            log_odds=None,
            p_up=None,
            btc_price=payload.get("btc_price"),
            details=payload,
        )

    def record_order_final(self, market_id: str, payload: dict) -> None:
        """Record the final order outcome (filled/partial/rejected/etc)."""
        self._memory.save_event(
            window_id=market_id,
            event_type="order_final",
            log_odds=None,
            p_up=None,
            btc_price=payload.get("btc_price"),
            details=payload,
        )

    def record_entry(self, strategy_name: str, trade: dict) -> None:
        trade_id = self._ensure_trade_id(strategy_name, trade)
        self._memory.save_event(
            window_id=trade["market_id"],
            event_type="entry",
            log_odds=None,
            p_up=None,
            btc_price=trade["btc_price"],
            details=trade,
        )
        if self._supabase:
            self._supabase.upsert_trade_safe({
                "id": trade_id,
                "coin": self._coin,
                "strategy": strategy_name,
                "market_id": trade["market_id"],
                "direction": trade["direction"],
                "token_price": trade["token_price"],
                "size_usd": trade["size_usd"],
                "shares": trade["shares"],
                "paper": True,
                "underlying_price": trade["btc_price"],
                "move_pct": trade["move_pct"],
                "created_at": trade["timestamp"],
            })

    def record_resolution(
        self,
        strategy_name: str,
        trade: dict,
        resolution,
        resolved_direction: str,
        source: str = "poll",
    ) -> None:
        resolution_details = {
            "won": resolution.won,
            "pnl_usd": resolution.pnl_usd,
            "resolved_direction": resolved_direction,
            "source": source,
            "trade": trade,
        }
        self._memory.save_event(
            window_id=trade["market_id"],
            event_type="resolution",
            p_up=None,
            log_odds=None,
            btc_price=None,
            details=resolution_details,
        )
        logger.info(
            "[RESOLUTION_WRITE] market=%s source=%s won=%s pnl=%+.2f direction=%s",
            trade["market_id"],
            source,
            resolution.won,
            resolution.pnl_usd,
            resolved_direction,
        )
        if self._supabase:
            self._supabase.upsert_trade_safe({
                "id": self._ensure_trade_id(strategy_name, trade),
                "coin": self._coin,
                "strategy": trade.get("strategy", strategy_name),
                "market_id": trade["market_id"],
                "direction": trade["direction"],
                "token_price": trade["token_price"],
                "size_usd": trade["size_usd"],
                "shares": trade.get("shares", 0),
                "won": resolution.won,
                "pnl_usd": resolution.pnl_usd,
                "paper": True,
                "underlying_price": trade.get("btc_price"),
                "move_pct": trade.get("move_pct"),
                "resolved_at": datetime.now(timezone.utc).isoformat(),
            })

    def record_paper_shadow_entry(self, strategy_name: str, trade: dict) -> None:
        """Record the paper leg of a paired live decision.

        This is attribution telemetry only. It does not mutate open live
        positions or Supabase trade state; live fills still use `entry`.
        """
        trade_id = self._ensure_trade_id(strategy_name, trade)
        shadow = dict(trade)
        shadow["trade_id"] = trade_id
        shadow["execution_mode"] = "paper_shadow"
        shadow["paper"] = True
        shadow["live_paired"] = True
        self._memory.save_event(
            window_id=trade["market_id"],
            event_type="paper_shadow_entry",
            log_odds=None,
            p_up=None,
            btc_price=trade.get("btc_price"),
            details=shadow,
        )

    def record_exit(self, strategy_name: str, trade: dict, exit_details: dict) -> None:
        self._memory.save_event(
            window_id=trade["market_id"],
            event_type="exit",
            p_up=None,
            log_odds=None,
            btc_price=None,
            details={
                "trade": trade,
                **exit_details,
            },
        )

    def record_status(
        self,
        window_id: str,
        btc_price: float,
        balance: float,
        trades_total: int,
        up_price: float,
        down_price: float,
        entry_blocker: dict | None = None,
    ) -> None:
        self._memory.save_event(
            window_id=window_id,
            event_type="status_tick",
            log_odds=None,
            p_up=None,
            btc_price=btc_price,
            details={
                "balance": balance,
                "trades": trades_total,
                "up_price": up_price,
                "down_price": down_price,
                "entry_blocker": entry_blocker,
            },
        )

    def close(self) -> None:
        self._memory.close()
        if self._supabase:
            self._supabase.close()
