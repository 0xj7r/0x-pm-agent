"""Forward simulator with honest execution: best_ask entry, fees, slippage.

The simulator is where all the other methods come together. Given a
forecaster (q estimator), a market, and a sizing policy, it walks
through the snapshot timeline, decides whether to enter, simulates
the fill against the orderbook depth, applies fees, tracks PnL, and
returns per-trade log returns plus aggregate statistics.

This is the spec's section 4 made into code. The headline metric it
produces is `log_growth_per_trade`. The framework's bootstrap module
takes the per-trade log return array and produces the CI that gates
promotion.

Critical correctness invariants:

- Entry price is best_ask, NEVER midpoint. Rows where best_ask is
  NaN (PolyBackTest collection gaps) are SKIPPED, not filled with
  midpoint. The honest answer to "what is my edge on a row I have
  no execution data for" is "I don't know."
- Fee is applied to the cost basis (price + fee), not subtracted
  from PnL afterward. This matters for break_even calculations.
- One trade per market in v1. The simulator returns after the first
  successful entry. Multi-entry is wave 3 in the spec.
- The simulator is a pure function. No I/O. No state across markets.
  Pass it arrays in, get a result out. This makes it testable with
  synthetic data and trivially parallelizable.
"""
from __future__ import annotations

from dataclasses import dataclass
from typing import Callable, Optional

import numpy as np

from autoresearch.methods.edge import (
    EdgePolicy,
    edge as edge_after_costs,
    market_implied_q,
    min_edge_required,
)
from autoresearch.methods.sizing import SizingPolicy, stake_fraction


@dataclass(frozen=True)
class TradeRecord:
    market_id: str
    direction: str  # "Up" or "Down"
    entry_index: int
    entry_price: float
    stake_fraction: float
    fee_paid: float
    won: bool
    pnl: float
    log_return: float


@dataclass(frozen=True)
class MarketSlice:
    """Per-market arrays needed by the simulator.

    All arrays must be the same length. NaN values in best_ask_up
    or best_ask_down indicate snapshot rows where PolyBackTest
    returned no orderbook; the simulator skips those rows for
    entry purposes but does not return early (later rows in the
    same market may have book data).
    """
    market_id: str
    winner: str  # "Up" or "Down"
    best_ask_up: np.ndarray
    best_ask_down: np.ndarray
    best_bid_up: np.ndarray
    best_bid_down: np.ndarray
    # additional features the forecaster needs - opaque to the simulator
    features: dict


# A forecaster is a callable that takes (MarketSlice, index) and returns q
# in [0, 1]. The simulator does not care how it computes q; that is the
# Researcher's job.
ForecasterFn = Callable[[MarketSlice, int], float]


def fee_model_default(price: float) -> float:
    """Polymarket-style taker fee.

    Default is a 2% taker fee on the entry price (matches the
    long-run shared/fees.py value as of this writing). The
    simulator can be passed a different fee_fn for stress tests.
    """
    return 0.02 * price


def simulate_market(
    slice_: MarketSlice,
    forecaster: ForecasterFn,
    sizing_policy: SizingPolicy = SizingPolicy(),
    edge_policy: EdgePolicy = EdgePolicy(),
    fee_fn: Callable[[float], float] = fee_model_default,
    start_index: int = 10,
    model_uncertainty: float = 0.0,
) -> Optional[TradeRecord]:
    """Walk a single market and return at most one trade.

    Returns None if no entry decision was triggered (no row had
    sufficient edge after costs and uncertainty).

    The trade pipeline:
      1. For each index from start_index forward, ask the forecaster
         for q.
      2. Compute edge on YES side: q - (best_ask_up + fee). If above
         min_edge_required, enter YES at best_ask_up, return.
      3. Else compute edge on NO side: (1-q) - (best_ask_down + fee).
         If above min_edge_required, enter NO at best_ask_down, return.
      4. Else continue to next index.

    Skips rows where the relevant best_ask is NaN.
    """
    n = len(slice_.best_ask_up)
    if n == 0 or start_index >= n:
        return None

    for i in range(start_index, n):
        ask_yes = float(slice_.best_ask_up[i])
        ask_no = float(slice_.best_ask_down[i])
        if not (np.isfinite(ask_yes) and np.isfinite(ask_no)):
            continue
        if ask_yes <= 0 or ask_yes >= 1 or ask_no <= 0 or ask_no >= 1:
            continue

        q_hat = float(forecaster(slice_, i))
        if not 0 <= q_hat <= 1:
            continue

        # YES side
        fee_yes = fee_fn(ask_yes)
        e_yes = edge_after_costs(q_hat, ask_yes, fee_yes)
        bid_yes = float(slice_.best_bid_up[i]) if np.isfinite(slice_.best_bid_up[i]) else ask_yes
        spread_yes = max(0.0, ask_yes - bid_yes)
        threshold_yes = min_edge_required(spread_yes, model_uncertainty, edge_policy)
        if e_yes > threshold_yes:
            f = stake_fraction(q_hat, ask_yes, sizing_policy)
            if f > 0:
                won = (slice_.winner == "Up")
                # PnL: paid (ask_yes + fee), received 1 if won else 0
                cost = ask_yes + fee_yes
                payout = 1.0 if won else 0.0
                pnl_per_unit = payout - cost
                # Per-bankroll-unit return: stake fraction is the
                # fraction of bankroll deployed; the per-unit-stake
                # return is pnl / cost (like an ROI), so the
                # bankroll-relative return is f * (pnl_per_unit / cost)
                # equivalently f * (payout/cost - 1).
                ret = f * (pnl_per_unit / cost)
                return TradeRecord(
                    market_id=slice_.market_id,
                    direction="Up",
                    entry_index=i,
                    entry_price=ask_yes,
                    stake_fraction=f,
                    fee_paid=fee_yes,
                    won=won,
                    pnl=pnl_per_unit,
                    log_return=float(np.log1p(ret)),
                )

        # NO side
        fee_no = fee_fn(ask_no)
        e_no = edge_after_costs(1 - q_hat, ask_no, fee_no)
        bid_no = float(slice_.best_bid_down[i]) if np.isfinite(slice_.best_bid_down[i]) else ask_no
        spread_no = max(0.0, ask_no - bid_no)
        threshold_no = min_edge_required(spread_no, model_uncertainty, edge_policy)
        if e_no > threshold_no:
            f = stake_fraction(1 - q_hat, ask_no, sizing_policy)
            if f > 0:
                won = (slice_.winner == "Down")
                cost = ask_no + fee_no
                payout = 1.0 if won else 0.0
                pnl_per_unit = payout - cost
                ret = f * (pnl_per_unit / cost)
                return TradeRecord(
                    market_id=slice_.market_id,
                    direction="Down",
                    entry_index=i,
                    entry_price=ask_no,
                    stake_fraction=f,
                    fee_paid=fee_no,
                    won=won,
                    pnl=pnl_per_unit,
                    log_return=float(np.log1p(ret)),
                )

    return None


@dataclass(frozen=True)
class SimulatorReport:
    n_markets: int
    n_trades: int
    n_skipped_no_book: int
    n_no_entry: int
    log_growth_per_trade: float
    log_growth_total: float
    win_rate: float
    bankroll_final: float
    trades: list[TradeRecord]


def simulate_partition(
    slices: list[MarketSlice],
    forecaster: ForecasterFn,
    sizing_policy: SizingPolicy = SizingPolicy(),
    edge_policy: EdgePolicy = EdgePolicy(),
    fee_fn: Callable[[float], float] = fee_model_default,
    model_uncertainty: float = 0.0,
) -> SimulatorReport:
    """Run the simulator across a list of markets and aggregate.

    Returns a SimulatorReport carrying per-trade records and the
    aggregate metrics. The headline metric is log_growth_per_trade,
    which is the input to the bootstrap module's CI computation.

    The simulator does NOT compound bankroll across markets in v1.
    Each market's trade is sized as a fraction of an abstract
    "1.0" bankroll and the per-trade log returns are reported
    independently. This is correct for the framework's purpose
    (estimating per-trade edge with bootstrap CIs) and avoids
    pathological edge cases where one early loss caps all
    subsequent stake sizes.
    """
    trades: list[TradeRecord] = []
    n_skipped = 0
    n_no_entry = 0

    for s in slices:
        if not np.any(np.isfinite(s.best_ask_up)) and not np.any(np.isfinite(s.best_ask_down)):
            n_skipped += 1
            continue
        t = simulate_market(
            s,
            forecaster,
            sizing_policy=sizing_policy,
            edge_policy=edge_policy,
            fee_fn=fee_fn,
            model_uncertainty=model_uncertainty,
        )
        if t is None:
            n_no_entry += 1
        else:
            trades.append(t)

    n_trades = len(trades)
    if n_trades > 0:
        log_returns = np.array([t.log_return for t in trades], dtype=np.float64)
        log_growth_per_trade = float(np.mean(log_returns))
        log_growth_total = float(np.sum(log_returns))
        win_rate = float(np.mean([t.won for t in trades]))
        bankroll_final = float(np.exp(log_growth_total))
    else:
        log_growth_per_trade = 0.0
        log_growth_total = 0.0
        win_rate = 0.0
        bankroll_final = 1.0

    return SimulatorReport(
        n_markets=len(slices),
        n_trades=n_trades,
        n_skipped_no_book=n_skipped,
        n_no_entry=n_no_entry,
        log_growth_per_trade=log_growth_per_trade,
        log_growth_total=log_growth_total,
        win_rate=win_rate,
        bankroll_final=bankroll_final,
        trades=trades,
    )


def market_implied_baseline_forecaster(slice_: MarketSlice, i: int) -> float:
    """The mandatory baseline forecaster: 'the market is right'.

    Returns market_implied_q(best_ask_up[i], best_ask_down[i]).
    Every research run must compare candidate forecasters against
    this baseline. A candidate that does not beat this baseline on
    log growth (with bootstrap CI > 0 above the baseline) is
    rejected. See spec section 4.7.
    """
    ay = float(slice_.best_ask_up[i])
    an = float(slice_.best_ask_down[i])
    return market_implied_q(ay, an)
