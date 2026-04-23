"""Strategy registry for backtesting and replaying saved params.

Each strategy is a filter function that checks additional conditions
beyond the base move threshold. The registry composes these filters
with the shared move/direction/entry logic.
"""
from __future__ import annotations

import itertools
from dataclasses import dataclass
from typing import Callable


def _base_check(pm, i: int, move: float, max_entry: float, min_entry: float = 0.0):
    """Shared logic: check move threshold, determine direction and entry.

    Returns (direction, entry) or ("NONE", 0) if below threshold,
    or ("SKIP", 0) if entry too expensive or too cheap.
    """
    if pm.abs_move[i] < move:
        return "NONE", 0.0
    direction = "Up" if pm.move_pct[i] > 0 else "Down"
    entry = pm.price_up[i] if direction == "Up" else pm.price_down[i]
    if entry <= 0 or entry > max_entry:
        return "SKIP", 0.0
    if entry < min_entry:
        return "SKIP", 0.0
    return direction, entry


def _in_trading_hours(current_hour: int, hour_start: int, hour_end: int) -> bool:
    if hour_start <= hour_end:
        return hour_start <= current_hour < hour_end
    return current_hour >= hour_start or current_hour < hour_end


# Filter functions: return True to trade, False to SKIP, None to defer to base
FilterFn = Callable  # (pm, i, direction, params) -> bool | None


@dataclass(frozen=True)
class StrategyDefinition:
    name: str
    filters: tuple[str, ...]
    grid: dict[str, tuple[float, ...]]

    def iter_params(self) -> list[dict]:
        keys = tuple(self.grid.keys())
        values = tuple(self.grid[key] for key in keys)
        return [
            dict(zip(keys, combo))
            for combo in itertools.product(*values)
        ]


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


STRATEGY_DEFINITIONS: dict[str, StrategyDefinition] = {
    "threshold": StrategyDefinition(
        name="threshold",
        filters=(),
        grid={
            "move": (0.01, 0.02, 0.03, 0.05, 0.08),
            "max_entry": (0.55, 0.60, 0.65, 0.70, 0.75, 0.85),
        },
    ),
    "consistency": StrategyDefinition(
        name="consistency",
        filters=("consistency",),
        grid={
            "move": (0.01, 0.02, 0.03, 0.05, 0.08),
            "cons": (0.55, 0.60, 0.65, 0.70, 0.75),
            "max_entry": (0.55, 0.60, 0.65, 0.70, 0.75, 0.85),
        },
    ),
    "velocity": StrategyDefinition(
        name="velocity",
        filters=("velocity_dir",),
        grid={
            "move": (0.01, 0.02, 0.03, 0.05, 0.08),
            "vel": (0.005, 0.01, 0.02, 0.03),
            "max_entry": (0.55, 0.60, 0.65, 0.70, 0.75, 0.85),
        },
    ),
    "skew": StrategyDefinition(
        name="skew",
        filters=("skew",),
        grid={
            "move": (0.01, 0.02, 0.03, 0.05, 0.08),
            "skew": (0.02, 0.05, 0.10, 0.15, 0.20),
            "max_entry": (0.55, 0.60, 0.65, 0.70, 0.75, 0.85),
        },
    ),
    "timing": StrategyDefinition(
        name="timing",
        filters=("timing",),
        grid={
            "move": (0.01, 0.02, 0.03, 0.05, 0.08),
            "elapsed": (0.05, 0.10, 0.20, 0.30, 0.50),
            "max_entry": (0.55, 0.60, 0.65, 0.70, 0.75, 0.85),
        },
    ),
    "volatility": StrategyDefinition(
        name="volatility",
        filters=("volatility",),
        grid={
            "move": (0.01, 0.02, 0.03, 0.05, 0.08),
            "vol": (0.001, 0.002, 0.005, 0.01),
            "max_entry": (0.55, 0.60, 0.65, 0.70, 0.75, 0.85),
        },
    ),
    "acceleration": StrategyDefinition(
        name="acceleration",
        filters=("acceleration",),
        grid={
            "move": (0.01, 0.02, 0.03, 0.05, 0.08),
            "accel": (0.005, 0.01, 0.02, 0.03),
            "max_entry": (0.55, 0.60, 0.65, 0.70, 0.75, 0.85),
        },
    ),
    "combo": StrategyDefinition(
        name="combo",
        filters=("consistency", "skew", "timing"),
        grid={
            "move": (0.01, 0.02, 0.03, 0.05),
            "cons": (0.55, 0.60, 0.65, 0.70, 0.75),
            "skew": (0.02, 0.05, 0.10, 0.15),
            "elapsed": (0.10, 0.20, 0.30, 0.50),
            "max_entry": (0.55, 0.60, 0.65, 0.70, 0.75),
        },
    ),
    "vel+cons": StrategyDefinition(
        name="vel+cons",
        filters=("velocity_dir", "consistency"),
        grid={
            "move": (0.01, 0.02, 0.03, 0.05),
            "vel": (0.005, 0.01, 0.02),
            "cons": (0.55, 0.65, 0.75),
            "max_entry": (0.55, 0.65, 0.75),
        },
    ),
    "accel+time": StrategyDefinition(
        name="accel+time",
        filters=("timing", "acceleration"),
        grid={
            "move": (0.01, 0.02, 0.03, 0.05),
            "accel": (0.005, 0.01, 0.02),
            "elapsed": (0.10, 0.20, 0.30),
            "max_entry": (0.55, 0.65, 0.75),
        },
    ),
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

    Returns fn(pm, i, current_hour=None) -> "Up" | "Down" | "SKIP" | None
    """
    definition = STRATEGY_DEFINITIONS.get(strategy_name)
    if definition is None:
        raise ValueError(f"Unknown strategy: {strategy_name}")

    move = params.get("move", 0.08)
    max_entry = params.get("max_entry", 0.55)
    min_entry = params.get("min_entry", 0.0)
    hour_start = params.get("hour_start")
    hour_end = params.get("hour_end")
    # Non-contiguous allow-list of UTC hours. Complements hour_start/end
    # (which is a contiguous range). If both are set, both must pass.
    _allowed_hours = params.get("allowed_hours")
    allowed_hours = set(int(h) for h in _allowed_hours) if _allowed_hours is not None else None
    # Optional deny-list of UTC hours (convenience; cleaner than listing
    # 21 allowed hours when we want to block 3).
    _skip_hours = params.get("skip_hours")
    skip_hours = set(int(h) for h in _skip_hours) if _skip_hours is not None else None
    filters = [FILTER_MAP[name] for name in definition.filters]

    def _to_float(value: object) -> float | None:
        try:
            return float(value)
        except (TypeError, ValueError):
            return None

    def fn(
        pm,
        i: int,
        current_hour: int | None = None,
        **_signal_context,
    ) -> str | None:
        if hour_start is not None and hour_end is not None and current_hour is not None:
            if not _in_trading_hours(current_hour, hour_start, hour_end):
                return "SKIP"
        if allowed_hours is not None and current_hour is not None:
            if current_hour not in allowed_hours:
                return "SKIP"
        if skip_hours is not None and current_hour is not None:
            if current_hour in skip_hours:
                return "SKIP"

        direction, entry = _base_check(pm, i, move, max_entry, min_entry)
        if direction == "NONE":
            return None
        if direction == "SKIP":
            return "SKIP"

        signal_seconds_from_start = _to_float(_signal_context.get("seconds_from_start"))
        if signal_seconds_from_start is not None:
            min_elapsed_seconds = _to_float(params.get("min_elapsed_seconds"))
            if min_elapsed_seconds is not None and signal_seconds_from_start < min_elapsed_seconds:
                return "SKIP"
            max_elapsed_seconds = _to_float(params.get("max_elapsed_seconds"))
            if max_elapsed_seconds is not None and signal_seconds_from_start > max_elapsed_seconds:
                return "SKIP"

        for filt in filters:
            result = filt(pm, i, direction, params)
            if isinstance(result, str):
                if result == "SKIP":
                    return "SKIP"
                direction = result
                entry = pm.price_up[i] if direction == "Up" else pm.price_down[i]
                if entry <= 0 or entry > max_entry or entry < min_entry:
                    return "SKIP"
            elif not result:
                return "SKIP"

        return direction

    return fn


def build_strategy_grid(strategy_names: list[str] | None = None) -> list[tuple[str, dict, Callable]]:
    selected = strategy_names or list(STRATEGY_DEFINITIONS.keys())
    grid: list[tuple[str, dict, Callable]] = []
    for name in selected:
        definition = STRATEGY_DEFINITIONS[name]
        for params in definition.iter_params():
            grid.append((name, params, build_check_fn(name, params)))
    return grid
