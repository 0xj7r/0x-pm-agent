"""Runtime adapter for executing research strategies on live window data."""
from __future__ import annotations

from dataclasses import dataclass, field

from strategies.registry import build_check_fn
from strategies.live_state import IncrementalMarketState


@dataclass
class LiveRuntimeStrategy:
    coin: str
    name: str
    params: dict
    _check_fn: object = field(init=False, repr=False)
    _market_state: IncrementalMarketState | None = field(default=None, init=False, repr=False)

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

    def __post_init__(self) -> None:
        self._check_fn = build_check_fn(self.name, self.params) if self.name != "disabled" else None

    @property
    def num_snaps(self) -> int:
        return self._market_state.num_snaps if self._market_state else 0

    def start_window(self, market_id: str, btc_open: float) -> None:
        self._market_state = IncrementalMarketState(market_id, btc_open)

    def check_signal(
        self,
        market_id: str,
        btc_open: float,
        snap: tuple[float, float, float],
        current_hour: int | None = None,
        sample_ts: float | None = None,
        seconds_from_start: float | None = None,
        seconds_to_close: float | None = None,
        window_duration_seconds: float | None = None,
        window_open_ts: float | None = None,
        price_to_beat: float | None = None,
        **_signal_context: object,
    ) -> str | None:
        if self.name == "disabled" or btc_open <= 0:
            return None
        if self._market_state is None or self._market_state.market_id != market_id:
            self.start_window(market_id, btc_open)
        elif self._market_state.num_snaps == 0 and self._market_state.btc_open != btc_open:
            self._market_state = IncrementalMarketState(market_id, btc_open)

        self._market_state.append(*snap)
        if self._market_state.num_snaps <= 0:
            return None
        pm = self._market_state.as_precomputed_market()
        idx = pm.num_snaps - 1
        if seconds_from_start is not None and idx < len(pm.elapsed_pct):
            if window_duration_seconds is not None and window_duration_seconds > 0:
                elapsed_pct = min(
                    1.0,
                    max(0.0, float(seconds_from_start) / float(window_duration_seconds)),
                )
                pm.elapsed_pct[idx] = elapsed_pct
            pm.seconds_from_start = float(seconds_from_start)
        if seconds_to_close is not None:
            pm.seconds_to_close = float(seconds_to_close)
        if window_duration_seconds is not None:
            pm.window_duration_seconds = float(window_duration_seconds)
        if window_open_ts is not None:
            pm.window_open_ts = float(window_open_ts)
        if sample_ts is not None:
            pm.sample_ts = float(sample_ts)
        if price_to_beat is not None:
            pm.price_to_beat = float(price_to_beat)
        return self._check_fn(
            pm,
            idx,
            current_hour=current_hour,
            seconds_from_start=seconds_from_start,
            seconds_to_close=seconds_to_close,
            window_duration_seconds=window_duration_seconds,
            window_open_ts=window_open_ts,
            price_to_beat=price_to_beat,
            sample_ts=sample_ts,
        )
