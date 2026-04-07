"""Shared trade extraction and scoring helpers for backtesting."""
from __future__ import annotations

from dataclasses import dataclass

from shared.fees import taker_fee


@dataclass(frozen=True)
class EvaluatedTrade:
    market_id: str
    direction: str
    winner: str
    won: bool
    entry_price: float
    entry_index: int
    pnl: float


def find_first_trade(
    pm,
    check_fn,
    start_index: int = 10,
    entry_delay: int = 0,
    entry_slippage: float = 0.0,
    fee_multiplier: float = 1.0,
) -> EvaluatedTrade | None:
    for index in range(start_index, pm.num_snaps):
        direction = check_fn(pm, index)
        if direction is None:
            continue
        if direction == "SKIP":
            break

        entry_index = index + entry_delay
        if entry_index >= pm.num_snaps:
            break

        entry = pm.price_up[entry_index] if direction == "Up" else pm.price_down[entry_index]
        entry = min(float(entry) + entry_slippage, 0.999)
        if entry <= 0 or entry >= 0.99:
            break

        won = direction == pm.winner
        fee = taker_fee(entry) * entry * fee_multiplier
        pnl = (1.0 - entry - fee) if won else -(entry + fee)
        return EvaluatedTrade(
            market_id=pm.market_id,
            direction=direction,
            winner=pm.winner,
            won=won,
            entry_price=entry,
            entry_index=entry_index,
            pnl=pnl,
        )
    return None
