"""Rolling-window regime features for the direction-asymmetric forecaster.

These features encode the recent state of the underlying market so the
forecaster can learn when continuation bets work versus when they don't.
The Apr 4-7 ETH paper-trade data showed a clear pattern: continuation
strategies' win rate decayed asymmetrically by direction (UP went from
85% to 49% over 4 days while DOWN held at 69%). The forecaster needs
features that let it detect "we are in a regime where UP continuation
no longer pays" and downweight UP bets accordingly.

The features here are MARKET-LEVEL: each market gets one scalar value
per feature, computed from a chronologically-ordered window of strictly
preceding markets. The build loop in feature_store.py broadcasts these
to per-snapshot arrays so the forecaster sees them at every snapshot
within the market.

Look-ahead safety: the value at market i depends only on markets with
strictly earlier start_time. The current market's own outcome and price
end are NEVER read by these functions. This is enforced by passing the
window as `past_markets` and the current as a separate `current_market`,
with no way to confuse the two.

Each function takes a tuple of past market summaries:

    PastMarket = (start_time_iso, price_start, price_end, winner)

and the lag window size N (default 20).
"""
from __future__ import annotations

from collections import deque
from dataclasses import dataclass
from typing import Iterable

import numpy as np


@dataclass(frozen=True)
class PastMarket:
    """Minimal market summary needed for regime feature computation.

    Only includes resolved markets with valid price_start and price_end.
    """
    start_time: str  # ISO 8601
    price_start: float
    price_end: float
    winner: str  # "Up" or "Down"

    @property
    def signed_move_pct(self) -> float:
        if self.price_start <= 0:
            return 0.0
        return (self.price_end - self.price_start) / self.price_start * 100.0

    @property
    def abs_move_pct(self) -> float:
        return abs(self.signed_move_pct)


@dataclass(frozen=True)
class RegimeFeatures:
    """Feature scalars for the regime layer at one market.

    All names are stable; adding/removing requires updating
    REGIME_FEATURE_NAMES below and any consumers.
    """
    lag_n_up_frac: float          # fraction of past N markets that resolved Up
    lag_n_mean_abs_move: float    # mean |move_pct| across past N markets
    lag_n_mean_signed_move: float # signed mean move_pct across past N markets
    lag_n_up_magnitude: float     # mean |move_pct| of past N Up markets
    lag_n_down_magnitude: float   # mean |move_pct| of past N Down markets
    hour_of_day: float            # 0-23, from current market start_time
    day_of_week: float            # 0-6, Monday=0
    n_past_markets: float         # actual sample size in the lag window

    def to_dict(self) -> dict[str, float]:
        return {
            "lag_n_up_frac": self.lag_n_up_frac,
            "lag_n_mean_abs_move": self.lag_n_mean_abs_move,
            "lag_n_mean_signed_move": self.lag_n_mean_signed_move,
            "lag_n_up_magnitude": self.lag_n_up_magnitude,
            "lag_n_down_magnitude": self.lag_n_down_magnitude,
            "hour_of_day": self.hour_of_day,
            "day_of_week": self.day_of_week,
            "n_past_markets": self.n_past_markets,
        }


REGIME_FEATURE_NAMES = list(RegimeFeatures(0, 0, 0, 0, 0, 0, 0, 0).to_dict().keys())


def compute_regime_features(
    past_markets: Iterable[PastMarket],
    current_start_time: str,
    lag_n: int = 20,
) -> RegimeFeatures:
    """Compute regime feature scalars for the current market.

    Parameters:
    - past_markets: an iterable of PastMarket entries STRICTLY PRECEDING
      the current market in time. The function takes only the most
      recent `lag_n` of them. The caller is responsible for the
      strict-precedence guarantee; this function does not re-check.
    - current_start_time: ISO 8601 timestamp of the current market;
      used only to extract hour-of-day and day-of-week features.
    - lag_n: window size, default 20 (~1.5 hours of 5-min markets).

    Returns RegimeFeatures with NaN for ratios when the relevant
    sample is empty (e.g., no past Up markets in the window). NaN
    propagates through downstream model features and is handled by
    the forecaster's missing-feature policy.
    """
    window = list(past_markets)[-lag_n:] if lag_n > 0 else []
    n_past = len(window)

    if n_past == 0:
        up_frac = float("nan")
        mean_abs = float("nan")
        mean_signed = float("nan")
        up_mag = float("nan")
        dn_mag = float("nan")
    else:
        ups = [m for m in window if m.winner == "Up"]
        downs = [m for m in window if m.winner == "Down"]
        up_frac = len(ups) / n_past
        abs_moves = [m.abs_move_pct for m in window]
        signed_moves = [m.signed_move_pct for m in window]
        mean_abs = float(np.mean(abs_moves)) if abs_moves else float("nan")
        mean_signed = float(np.mean(signed_moves)) if signed_moves else float("nan")
        up_mag = float(np.mean([m.abs_move_pct for m in ups])) if ups else float("nan")
        dn_mag = float(np.mean([m.abs_move_pct for m in downs])) if downs else float("nan")

    # Time-of-day and day-of-week from the current market's start_time
    # Robust to ISO 8601 with optional Z suffix and microseconds
    from datetime import datetime, timezone
    ts = current_start_time
    if ts.endswith("Z"):
        ts = ts[:-1] + "+00:00"
    try:
        dt = datetime.fromisoformat(ts)
        if dt.tzinfo is None:
            dt = dt.replace(tzinfo=timezone.utc)
        else:
            dt = dt.astimezone(timezone.utc)
        hour = float(dt.hour)
        dow = float(dt.weekday())
    except (ValueError, TypeError):
        hour = float("nan")
        dow = float("nan")

    return RegimeFeatures(
        lag_n_up_frac=up_frac,
        lag_n_mean_abs_move=mean_abs,
        lag_n_mean_signed_move=mean_signed,
        lag_n_up_magnitude=up_mag,
        lag_n_down_magnitude=dn_mag,
        hour_of_day=hour,
        day_of_week=dow,
        n_past_markets=float(n_past),
    )


class RollingMarketWindow:
    """Maintains a rolling window of past markets for incremental
    regime feature computation across a chronologically-ordered build.

    Usage:
        window = RollingMarketWindow(lag_n=20)
        for market_row in markets_sorted_by_start_time:
            features = window.features_for_current(market_row.start_time)
            # ... use features for the current market ...
            window.append(PastMarket(market_row.start_time,
                                     market_row.price_start,
                                     market_row.price_end,
                                     market_row.winner))

    The order matters: features_for_current is called BEFORE append,
    which guarantees the window contains only strictly preceding markets
    when the features are computed. This is the look-ahead safety
    invariant.
    """

    def __init__(self, lag_n: int = 20) -> None:
        self.lag_n = lag_n
        self._buffer: deque[PastMarket] = deque(maxlen=lag_n)

    def features_for_current(self, current_start_time: str) -> RegimeFeatures:
        return compute_regime_features(
            self._buffer, current_start_time, lag_n=self.lag_n
        )

    def append(self, market: PastMarket) -> None:
        self._buffer.append(market)

    def __len__(self) -> int:
        return len(self._buffer)
