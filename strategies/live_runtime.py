"""Runtime adapter for executing research strategies on live window data."""
from __future__ import annotations

from dataclasses import dataclass

from backtesting.precompute import precompute_market
from strategies.registry import build_check_fn


@dataclass
class LiveRuntimeStrategy:
    coin: str
    name: str
    params: dict

    @classmethod
    def from_config(cls, coin: str, config: dict) -> "LiveRuntimeStrategy":
        strategy = config.get("strategy", "threshold")
        params = dict(config.get("params", {}))
        if strategy == "threshold" and not params:
            params = {
                "move": config.get("move_threshold", 0.08),
                "max_entry": config.get("max_entry", 0.55),
            }
        return cls(coin=coin, name=strategy, params=params)

    def check_signal(
        self,
        market_id: str,
        btc_open: float,
        snaps: list[tuple[float, float, float]],
    ) -> str | None:
        if self.name == "disabled" or not snaps or btc_open <= 0:
            return None
        pm = precompute_market(market_id, "Up", btc_open, snaps)
        if pm.num_snaps <= 0:
            return None
        check_fn = build_check_fn(self.name, self.params)
        return check_fn(pm, pm.num_snaps - 1)
