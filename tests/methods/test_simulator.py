"""Tests for autoresearch.methods.simulator.

Test strategy:
- Synthetic markets with known best_ask, winner, and a forecaster
  whose q is constructed to be either obviously profitable or
  obviously unprofitable.
- Property: a perfect oracle forecaster should produce strictly
  positive log returns on every trade it takes.
- Property: a baseline (market-implied) forecaster should never
  produce a positive edge by construction (q == break_even minus fee).
- Property: rows with NaN best_ask should be skipped, not crash.
"""
from __future__ import annotations

import math

import numpy as np
import pytest

from autoresearch.methods.simulator import (
    MarketSlice,
    SimulatorReport,
    TradeRecord,
    market_implied_baseline_forecaster,
    simulate_market,
    simulate_partition,
)
from autoresearch.methods.sizing import SizingPolicy
from autoresearch.methods.edge import EdgePolicy


def make_synthetic_slice(
    market_id: str = "test",
    winner: str = "Up",
    n: int = 50,
    ask_up: float = 0.45,
    ask_down: float = 0.55,
    book_nan_indices: tuple = (),
) -> MarketSlice:
    bid_up = max(0.01, ask_up - 0.01)
    bid_down = max(0.01, ask_down - 0.01)
    ask_up_arr = np.full(n, ask_up, dtype=np.float64)
    ask_down_arr = np.full(n, ask_down, dtype=np.float64)
    bid_up_arr = np.full(n, bid_up, dtype=np.float64)
    bid_down_arr = np.full(n, bid_down, dtype=np.float64)
    for i in book_nan_indices:
        ask_up_arr[i] = np.nan
        ask_down_arr[i] = np.nan
        bid_up_arr[i] = np.nan
        bid_down_arr[i] = np.nan
    return MarketSlice(
        market_id=market_id,
        winner=winner,
        best_ask_up=ask_up_arr,
        best_ask_down=ask_down_arr,
        best_bid_up=bid_up_arr,
        best_bid_down=bid_down_arr,
        features={},
    )


class TestSimulateMarket:
    def test_oracle_forecaster_takes_winning_trade(self):
        # Market resolves Up. Oracle says q=0.99. Ask_up is 0.45.
        # Edge = 0.99 - (0.45 + 0.009) = 0.531 — well above any threshold.
        slice_ = make_synthetic_slice(winner="Up", ask_up=0.45, ask_down=0.56)
        oracle = lambda s, i: 0.99
        # Use shrink_alpha=1.0 so the simulator sees q=0.99 unchanged.
        policy = SizingPolicy(shrink_alpha=1.0)
        trade = simulate_market(slice_, oracle, sizing_policy=policy)
        assert trade is not None
        assert trade.direction == "Up"
        assert trade.won is True
        assert trade.log_return > 0

    def test_oracle_wrong_direction_loses(self):
        # Market resolves Down. Oracle wrongly says q=0.99 (YES).
        slice_ = make_synthetic_slice(winner="Down", ask_up=0.45, ask_down=0.56)
        oracle = lambda s, i: 0.99
        policy = SizingPolicy(shrink_alpha=1.0)
        trade = simulate_market(slice_, oracle, sizing_policy=policy)
        assert trade is not None
        assert trade.direction == "Up"
        assert trade.won is False
        assert trade.log_return < 0

    def test_no_edge_no_trade(self):
        # Forecaster says exactly market-implied. No edge after fees.
        slice_ = make_synthetic_slice(winner="Up", ask_up=0.45, ask_down=0.56)
        # market-implied: 0.45 / (0.45 + 0.56) = 0.4455
        flat = lambda s, i: 0.4455
        policy = SizingPolicy(shrink_alpha=1.0)
        trade = simulate_market(slice_, flat, sizing_policy=policy)
        assert trade is None

    def test_skips_nan_book_rows(self):
        # First 5 rows have no book, then later rows do
        slice_ = make_synthetic_slice(
            winner="Up", n=20, book_nan_indices=(10, 11, 12, 13)
        )
        oracle = lambda s, i: 0.99
        policy = SizingPolicy(shrink_alpha=1.0)
        trade = simulate_market(slice_, oracle, sizing_policy=policy)
        assert trade is not None
        # Should NOT have entered at index 10-13 (NaN book)
        assert trade.entry_index not in (10, 11, 12, 13)

    def test_takes_no_side_when_yes_unprofitable(self):
        # YES is too expensive but NO has edge
        # ask_up = 0.85, ask_down = 0.20, q = 0.20 (so NO probability is 0.80)
        # Fee for NO = 0.02 * 0.20 = 0.004
        # NO break-even = 0.204; NO probability believed = 0.80
        # NO edge = 0.596 - well above threshold
        slice_ = make_synthetic_slice(winner="Down", ask_up=0.85, ask_down=0.20)
        forecaster = lambda s, i: 0.20  # YES prob 20%
        policy = SizingPolicy(shrink_alpha=1.0)
        trade = simulate_market(slice_, forecaster, sizing_policy=policy)
        assert trade is not None
        assert trade.direction == "Down"
        assert trade.won is True

    def test_no_book_at_all_returns_none(self):
        slice_ = make_synthetic_slice(
            n=20, book_nan_indices=tuple(range(20))
        )
        oracle = lambda s, i: 0.99
        trade = simulate_market(slice_, oracle, sizing_policy=SizingPolicy(shrink_alpha=1.0))
        assert trade is None


class TestSimulatePartition:
    def test_aggregates_correctly(self):
        # 5 markets, oracle wins all of them
        slices = [
            make_synthetic_slice(market_id=f"m{i}", winner="Up", ask_up=0.40)
            for i in range(5)
        ]
        oracle = lambda s, i: 0.99
        policy = SizingPolicy(shrink_alpha=1.0)
        report = simulate_partition(slices, oracle, sizing_policy=policy)
        assert isinstance(report, SimulatorReport)
        assert report.n_markets == 5
        assert report.n_trades == 5
        assert report.win_rate == 1.0
        assert report.log_growth_per_trade > 0
        assert report.bankroll_final > 1.0

    def test_market_implied_baseline_no_edge(self):
        # The market-implied baseline forecaster always returns the
        # market's implied q. After fees, this should produce no
        # trades because edge = -fee.
        slices = [
            make_synthetic_slice(market_id=f"m{i}", winner="Up", ask_up=0.45, ask_down=0.56)
            for i in range(10)
        ]
        report = simulate_partition(
            slices, market_implied_baseline_forecaster, sizing_policy=SizingPolicy(shrink_alpha=1.0)
        )
        assert report.n_trades == 0
        assert report.log_growth_per_trade == 0.0

    def test_skipped_markets_counted(self):
        # All-NaN markets should be skipped, not enter the trades list
        slices = [
            make_synthetic_slice(
                market_id=f"m{i}",
                n=20,
                book_nan_indices=tuple(range(20)),
            )
            for i in range(3)
        ]
        oracle = lambda s, i: 0.99
        report = simulate_partition(
            slices, oracle, sizing_policy=SizingPolicy(shrink_alpha=1.0)
        )
        assert report.n_skipped_no_book == 3
        assert report.n_trades == 0
