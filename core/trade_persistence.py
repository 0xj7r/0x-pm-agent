"""Persistence helpers for engine trade lifecycle events."""
from __future__ import annotations

from datetime import datetime, timezone
from uuid import uuid4

from core.memory import MemoryStore


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

    def record_resolution(self, strategy_name: str, trade: dict, resolution, resolved_direction: str) -> None:
        self._memory.save_event(
            window_id=trade["market_id"],
            event_type="resolution",
            p_up=None,
            log_odds=None,
            btc_price=None,
            details={
                "won": resolution.won,
                "pnl_usd": resolution.pnl_usd,
                "resolved_direction": resolved_direction,
                "trade": trade,
            },
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

    def record_status(self, window_id: str, btc_price: float, balance: float, trades_total: int, up_price: float, down_price: float) -> None:
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
            },
        )

    def close(self) -> None:
        self._memory.close()
        if self._supabase:
            self._supabase.close()
