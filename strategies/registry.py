"""Strategy registry for backtesting and replaying saved params.

Each strategy is a filter function that checks additional conditions
beyond the base move threshold. The registry composes these filters
with the shared move/direction/entry logic.
"""
from __future__ import annotations

from typing import Callable


def _base_check(pm, i: int, move: float, max_entry: float):
    """Shared logic: check move threshold, determine direction and entry.

    Returns (direction, entry) or ("NONE", 0) if below threshold,
    or ("SKIP", 0) if entry too expensive.
    """
    if pm.abs_move[i] < move:
        return "NONE", 0.0
    direction = "Up" if pm.move_pct[i] > 0 else "Down"
    entry = pm.price_up[i] if direction == "Up" else pm.price_down[i]
    if entry <= 0 or entry > max_entry:
        return "SKIP", 0.0
    return direction, entry


# Filter functions: return True to trade, False to SKIP, None to defer to base
FilterFn = Callable  # (pm, i, direction, params) -> bool | None

def _filter_consistency(pm, i, direction, params) -> bool:
    return pm.consistency[i] >= params.get("cons", 0)

def _filter_velocity(pm, i, direction, params) -> bool:
    return abs(pm.velocity[i]) >= params.get("vel", 0)

def _filter_velocity_direction(pm, i, direction, params) -> str:
    """Velocity filter that also overrides direction."""
    if abs(pm.velocity[i]) < params.get("vel", 0):
        return "SKIP"
    return "Up" if pm.velocity[i] > 0 else "Down"

def _filter_skew(pm, i, direction, params) -> bool:
    return pm.token_skew[i] <= params.get("skew", float("inf"))

def _filter_timing(pm, i, direction, params) -> bool:
    return pm.elapsed_pct[i] <= params.get("elapsed", 1.0)

def _filter_volatility(pm, i, direction, params) -> bool:
    return pm.volatility[i] >= params.get("vol", 0)

def _filter_acceleration(pm, i, direction, params) -> bool:
    accel = params.get("accel", 0)
    if direction == "Up":
        return pm.acceleration[i] >= accel
    return pm.acceleration[i] <= -accel


STRATEGY_FILTERS: dict[str, list[str]] = {
    "threshold": [],
    "consistency": ["consistency"],
    "velocity": ["velocity_dir"],
    "skew": ["skew"],
    "timing": ["timing"],
    "volatility": ["volatility"],
    "acceleration": ["acceleration"],
    "combo": ["consistency", "skew", "timing"],
    "vel+cons": ["velocity_dir", "consistency"],
    "accel+time": ["timing", "acceleration"],
}

FILTER_MAP: dict[str, FilterFn] = {
    "consistency": _filter_consistency,
    "velocity": _filter_velocity,
    "velocity_dir": _filter_velocity_direction,
    "skew": _filter_skew,
    "timing": _filter_timing,
    "volatility": _filter_volatility,
    "acceleration": _filter_acceleration,
}


def build_check_fn(strategy_name: str, params: dict) -> Callable:
    """Build a check function for the given strategy and params.

    Returns fn(pm, i) -> "Up" | "Down" | "SKIP" | None
    """
    if strategy_name not in STRATEGY_FILTERS:
        raise ValueError(f"Unknown strategy: {strategy_name}")

    move = params.get("move", 0.08)
    max_entry = params.get("max_entry", 0.55)
    filters = [FILTER_MAP[f] for f in STRATEGY_FILTERS[strategy_name]]

    def fn(pm, i: int) -> str | None:
        direction, entry = _base_check(pm, i, move, max_entry)
        if direction == "NONE":
            return None
        if direction == "SKIP":
            return "SKIP"

        for filt in filters:
            result = filt(pm, i, direction, params)
            if isinstance(result, str):
                if result == "SKIP":
                    return "SKIP"
                direction = result
                entry = pm.price_up[i] if direction == "Up" else pm.price_down[i]
                if entry <= 0 or entry > max_entry:
                    return "SKIP"
            elif not result:
                return "SKIP"

        return direction

    return fn
