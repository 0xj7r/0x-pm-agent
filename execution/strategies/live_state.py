"""Incremental live feature tracking for runtime strategy evaluation."""
from __future__ import annotations

import math
from types import SimpleNamespace


class IncrementalMarketState:
    """Maintain rolling strategy features in O(1) amortized time per snapshot."""

    def __init__(self, market_id: str, btc_open: float) -> None:
        self.market_id = market_id
        self.btc_open = btc_open
        self.prices: list[float] = []
        self.move_pct: list[float] = []
        self.abs_move: list[float] = []
        self.velocity: list[float] = []
        self.consistency: list[float] = []
        self.volatility: list[float] = []
        self.token_skew: list[float] = []
        self.elapsed_pct: list[float] = []
        self.acceleration: list[float] = []
        self.price_up: list[float] = []
        self.price_down: list[float] = []
        self._tick_dirs: list[int] = []
        self._tick_changes: list[float] = []

    @property
    def num_snaps(self) -> int:
        return len(self.prices)

    def append(self, underlying_price: float, price_up: float, price_down: float) -> None:
        price = float(underlying_price or self.btc_open or 0.0)
        up = float(price_up if price_up is not None else 0.5)
        down = float(price_down if price_down is not None else 0.5)
        idx = self.num_snaps

        self.prices.append(price)
        self.price_up.append(up)
        self.price_down.append(down)
        self.elapsed_pct.append(idx / 2500.0)

        move_pct = ((price - self.btc_open) / self.btc_open * 100.0) if self.btc_open else 0.0
        self.move_pct.append(move_pct)
        self.abs_move.append(abs(move_pct))

        prev_tick_price = self.prices[idx - 1] if idx > 0 else price
        if idx == 0:
            tick_dir = 0
            tick_change = 0.0
        else:
            delta = price - prev_tick_price
            tick_dir = 1 if delta > 0 else -1 if delta < 0 else 0
            tick_change = ((price - prev_tick_price) / prev_tick_price * 100.0) if prev_tick_price else 0.0
        self._tick_dirs.append(tick_dir)
        self._tick_changes.append(tick_change)

        velocity_idx = max(idx - 20, 0)
        velocity_base = self.prices[velocity_idx]
        velocity = 0.0
        if idx > 0 and velocity_base:
            velocity = (price - velocity_base) / velocity_base * 100.0
        self.velocity.append(velocity)

        self.consistency.append(self._compute_consistency(idx, move_pct))
        self.volatility.append(self._compute_volatility(idx))
        self.token_skew.append(self._compute_token_skew(move_pct, up, down))
        self.acceleration.append(self._compute_acceleration(idx))

    def as_precomputed_market(self) -> SimpleNamespace:
        return SimpleNamespace(
            market_id=self.market_id,
            winner="Up",
            num_snaps=self.num_snaps,
            move_pct=self.move_pct,
            abs_move=self.abs_move,
            velocity=self.velocity,
            consistency=self.consistency,
            volatility=self.volatility,
            token_skew=self.token_skew,
            elapsed_pct=self.elapsed_pct,
            acceleration=self.acceleration,
            price_up=self.price_up,
            price_down=self.price_down,
        )

    def _compute_consistency(self, idx: int, move_pct: float) -> float:
        cw = min(idx, 30)
        if cw <= 1 or move_pct == 0:
            return 0.5
        recent = self._tick_dirs[idx - cw + 1:idx + 1]
        target_dir = 1 if move_pct > 0 else -1
        same = sum(1 for tick_dir in recent if tick_dir == target_dir)
        return same / cw

    def _compute_volatility(self, idx: int) -> float:
        vw = min(idx, 50)
        if vw <= 2:
            return 0.0
        recent = self._tick_changes[idx - vw + 1:idx + 1]
        count = len(recent)
        if count <= 1:
            return 0.0
        mean = sum(recent) / count
        variance = sum((value - mean) ** 2 for value in recent) / (count - 1)
        return math.sqrt(max(variance, 0.0))

    @staticmethod
    def _compute_token_skew(move_pct: float, price_up: float, price_down: float) -> float:
        if move_pct > 0:
            return price_up - 0.5
        if move_pct < 0:
            return price_down - 0.5
        return 0.0

    def _compute_acceleration(self, idx: int) -> float:
        al = min(idx, 10)
        if al <= 2:
            return 0.0
        mid_idx = idx - (al // 2)
        early_idx = idx - al
        early_price = self.prices[early_idx]
        mid_price = self.prices[mid_idx]
        current_price = self.prices[idx]
        v1 = ((mid_price - early_price) / early_price * 100.0) if early_price else 0.0
        v2 = ((current_price - mid_price) / mid_price * 100.0) if mid_price else 0.0
        return v2 - v1
