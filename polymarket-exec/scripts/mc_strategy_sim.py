#!/usr/bin/env python3
"""Monte Carlo simulator for paired-MM strategy P&L distributions.

Replaces the over-pessimistic paper env with a parameterized synthetic
market. Generates order books + taker arrival streams under different
regime assumptions, runs the strategy logic through them, and outputs
distributions of fill rate and P&L.

Why this exists: Paper env shows 0 fills across all variants but is
known too pessimistic. Live ground truth is geo-blocked. MC sim gives
us distributional confidence ("in 80% of plausible regimes, this
strategy makes X-Y/day at $C capital") without needing real fills.

Strategy modeled (matches polymarket-exec btc_5m_mm pricing):
- Pure book-mid normalized fair value (no spot/momentum)
- max_bid = fair - edge_bps/10000 - inventory_skew
- maker_bid_price = min(best_bid, max_bid, best_ask - safety_ticks*tick)
- Paired bids: posts on YES + NO simultaneously
- Fills via taker arrival at our queue position

Regimes swept (each parameter has a distribution, sampled per trial):
- vol: realized volatility of mid price (low/medium/high)
- taker_arrival_rate: takers per second hitting top of book
- queue_depth_ratio: total queue size at our level (we're at back)
- spread: best_ask - best_bid in cents
- adverse_selection: probability that fills lose vs gain on average

Usage:
  ./mc_strategy_sim.py --trials 1000 --capital 1000 --hours 1 \\
      --output data/mc/results.json
  ./mc_strategy_sim.py --plot   # render distribution charts (matplotlib)
"""

from __future__ import annotations

import argparse
import json
import math
import random
import statistics
import sys
from dataclasses import dataclass, field, asdict
from pathlib import Path
from typing import Any


@dataclass
class Regime:
    """A single set of market regime parameters."""
    name: str
    vol_bps_per_minute: float       # realized vol of mid price
    taker_per_sec: float             # rate of taker hits at top of book
    queue_depth_usd: float           # USD of resting orders ahead of us
    spread_cents: int                # best_ask - best_bid in 1-cent ticks
    adverse_selection_prob: float    # P(fill is adversely selected)
    maker_rebate_pct: float          # rebate earned per fill (e.g., 0.001 = 10bps)


@dataclass
class StrategyConfig:
    capital_usd: float
    base_clip_usd: float
    min_edge_bps: float              # required edge over fair (e.g., 25)
    maker_safety_ticks: float        # how far below ask we cap
    quote_age_ms: int                # min age before reprice
    fee_per_fill_pct: float          # taker fee if we cross


@dataclass
class TrialResult:
    regime_name: str
    quotes_posted: int
    fills: int
    fill_rate: float
    gross_pnl_usd: float
    rebate_pnl_usd: float
    fee_pnl_usd: float
    net_pnl_usd: float
    final_capital: float


# Standard regimes: cover the realistic parameter space for Polymarket
# 5-min crypto markets. These ranges are chosen as plausible bands;
# replace with measured values once we have live data.
REGIMES = [
    Regime(name="flat_low_vol",
           vol_bps_per_minute=5,    taker_per_sec=0.3,  queue_depth_usd=200,
           spread_cents=1, adverse_selection_prob=0.55, maker_rebate_pct=0.001),
    Regime(name="normal",
           vol_bps_per_minute=20,   taker_per_sec=1.0,  queue_depth_usd=150,
           spread_cents=1, adverse_selection_prob=0.50, maker_rebate_pct=0.001),
    Regime(name="active_trend",
           vol_bps_per_minute=60,   taker_per_sec=3.0,  queue_depth_usd=100,
           spread_cents=2, adverse_selection_prob=0.55, maker_rebate_pct=0.001),
    Regime(name="high_vol_burst",
           vol_bps_per_minute=200,  taker_per_sec=8.0,  queue_depth_usd=80,
           spread_cents=3, adverse_selection_prob=0.40, maker_rebate_pct=0.001),
    Regime(name="quiet_overnight",
           vol_bps_per_minute=2,    taker_per_sec=0.05, queue_depth_usd=300,
           spread_cents=1, adverse_selection_prob=0.50, maker_rebate_pct=0.001),
]


def simulate_trial(regime: Regime, strategy: StrategyConfig, hours: float,
                   rng: random.Random) -> TrialResult:
    """Simulate one (regime, strategy, duration) trial.

    Mid price evolves as a random walk. Each second we have:
    - Some probability that taker(s) hit top of book
    - Some probability that mid moves enough to trigger our reprice
    - Our standing quote may get filled (with probability proportional
      to our queue position vs takers cleared)
    """
    seconds = int(hours * 3600)
    capital = strategy.capital_usd
    mid = 0.50  # symmetric pair starts here

    # Vol per second from per-minute bps
    vol_per_sec = (regime.vol_bps_per_minute / 10_000) / math.sqrt(60)

    # Our quote: posted at fair - edge below mid (one tick below best_bid effectively)
    # Quote refresh interval (ms → seconds for sim)
    quote_age_sec = strategy.quote_age_ms / 1000.0
    last_quote_time = -1e9
    our_bid_yes = 0.0
    our_bid_no = 0.0
    quote_size = 0.0

    quotes_posted = 0
    fills = 0
    gross_pnl = 0.0
    rebate_pnl = 0.0
    fee_pnl = 0.0

    for t in range(seconds):
        # Mid evolves
        shock = rng.gauss(0, vol_per_sec)
        mid = max(0.01, min(0.99, mid + shock))

        # Refresh quote if aged out
        if (t - last_quote_time) >= quote_age_sec:
            # btc_5m_mm logic: fair = book mid normalized; max_bid = fair - edge
            edge_decimal = strategy.min_edge_bps / 10_000
            our_bid_yes = max(0.01, mid - edge_decimal -
                              strategy.maker_safety_ticks * 0.01)
            our_bid_no = max(0.01, (1.0 - mid) - edge_decimal -
                             strategy.maker_safety_ticks * 0.01)
            # Round to 1-cent tick
            our_bid_yes = round(our_bid_yes * 100) / 100
            our_bid_no = round(our_bid_no * 100) / 100
            # Clip size limited by capital
            quote_size = min(strategy.base_clip_usd / max(0.01, our_bid_yes),
                             capital * 0.1 / max(0.01, our_bid_yes))
            if quote_size > 0:
                quotes_posted += 1
            last_quote_time = t

        # Taker arrivals this second (Poisson)
        n_takers = sum(1 for _ in range(int(regime.taker_per_sec) + 1)
                       if rng.random() < (regime.taker_per_sec / max(1, int(regime.taker_per_sec) + 1)))

        for _ in range(n_takers):
            # Where did the taker hit? Equal chance YES/NO side.
            our_bid = our_bid_yes if rng.random() < 0.5 else our_bid_no
            our_side_label = "yes" if our_bid == our_bid_yes else "no"
            taker_size_usd = max(1.0, rng.expovariate(1 / 5.0))  # avg $5 taker

            # Probability we get filled: our queue position
            # (we're at back, queue_depth_usd is ahead of us)
            # Taker has to clear queue_depth before reaching us
            cleared = max(0.0, taker_size_usd - regime.queue_depth_usd)
            our_fill_usd = min(quote_size * our_bid, cleared)

            if our_fill_usd > 0.01 and quote_size > 0:
                fills += 1
                # Adverse selection: fill happens where the market is moving away
                # On average we lose `adverse_selection_prob - 0.5` per fill (in pp)
                adverse_loss_pct = (regime.adverse_selection_prob - 0.5) * 0.02
                gross_pnl -= our_fill_usd * adverse_loss_pct
                # Maker rebate (% of fill notional, scaled by p(1-p))
                rebate_pnl += our_fill_usd * regime.maker_rebate_pct * our_bid * (1 - our_bid)
                # Capital adjusts
                capital -= our_fill_usd
                # Reduce remaining quote
                quote_size = max(0, quote_size - (our_fill_usd / our_bid))

        # Inventory aging: assume positions resolve via merge/redeem with some lag.
        # Crude: every minute, recover collateral at fair * size.
        if t % 60 == 0 and t > 0:
            # Not modeled in detail; treat as "mark to mid" for capital recovery
            pass

    fill_rate = fills / max(1, quotes_posted)
    net_pnl = gross_pnl + rebate_pnl + fee_pnl
    final_capital = strategy.capital_usd + net_pnl
    return TrialResult(
        regime_name=regime.name,
        quotes_posted=quotes_posted,
        fills=fills,
        fill_rate=fill_rate,
        gross_pnl_usd=gross_pnl,
        rebate_pnl_usd=rebate_pnl,
        fee_pnl_usd=fee_pnl,
        net_pnl_usd=net_pnl,
        final_capital=final_capital,
    )


def summarize(results: list[TrialResult]) -> dict[str, Any]:
    """Aggregate results into per-regime distributions."""
    by_regime: dict[str, list[TrialResult]] = {}
    for r in results:
        by_regime.setdefault(r.regime_name, []).append(r)

    summary = {}
    for regime_name, rs in by_regime.items():
        net_pnls = [r.net_pnl_usd for r in rs]
        fill_rates = [r.fill_rate for r in rs]
        fills = [r.fills for r in rs]
        net_pnls_sorted = sorted(net_pnls)
        n = len(net_pnls)
        summary[regime_name] = {
            "trials": n,
            "fills_median": statistics.median(fills),
            "fills_mean": statistics.mean(fills),
            "fill_rate_median": statistics.median(fill_rates),
            "net_pnl_p10": net_pnls_sorted[int(n * 0.10)] if n else 0,
            "net_pnl_p25": net_pnls_sorted[int(n * 0.25)] if n else 0,
            "net_pnl_p50": net_pnls_sorted[int(n * 0.50)] if n else 0,
            "net_pnl_p75": net_pnls_sorted[int(n * 0.75)] if n else 0,
            "net_pnl_p90": net_pnls_sorted[int(n * 0.90)] if n else 0,
            "net_pnl_mean": statistics.mean(net_pnls),
            "net_pnl_stdev": statistics.stdev(net_pnls) if n > 1 else 0,
            "win_rate": sum(1 for p in net_pnls if p > 0) / n if n else 0,
        }
    return summary


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--trials", type=int, default=200,
                    help="trials per regime (default 200)")
    ap.add_argument("--capital", type=float, default=100,
                    help="starting capital USD (default 100)")
    ap.add_argument("--base-clip", type=float, default=1.10,
                    help="base clip USD per quote (default 1.10)")
    ap.add_argument("--edge-bps", type=float, default=25,
                    help="MIN_EDGE_BPS for strategy (default 25)")
    ap.add_argument("--safety-ticks", type=float, default=2,
                    help="maker_safety_ticks (default 2)")
    ap.add_argument("--quote-age-ms", type=int, default=5000,
                    help="quote refresh interval in ms (default 5000)")
    ap.add_argument("--hours", type=float, default=1.0,
                    help="hours per trial (default 1.0)")
    ap.add_argument("--seed", type=int, default=42, help="RNG seed")
    ap.add_argument("--output", type=str, help="write JSON results to path")
    args = ap.parse_args()

    strategy = StrategyConfig(
        capital_usd=args.capital,
        base_clip_usd=args.base_clip,
        min_edge_bps=args.edge_bps,
        maker_safety_ticks=args.safety_ticks,
        quote_age_ms=args.quote_age_ms,
        fee_per_fill_pct=0.0,
    )

    rng = random.Random(args.seed)
    print(f"=== Monte Carlo strategy simulation ===")
    print(f"Capital: ${strategy.capital_usd}, Clip: ${strategy.base_clip_usd}")
    print(f"Edge: {strategy.min_edge_bps}bps, Safety ticks: {strategy.maker_safety_ticks}")
    print(f"Quote age: {strategy.quote_age_ms}ms, Trial duration: {args.hours}h")
    print(f"Trials per regime: {args.trials}, Regimes: {len(REGIMES)}")
    print()

    all_results = []
    for regime in REGIMES:
        for _ in range(args.trials):
            all_results.append(simulate_trial(regime, strategy, args.hours, rng))

    summary = summarize(all_results)

    print(f"{'regime':<18} {'fills':>6} {'rate':>6} {'p10':>10} {'p50':>10} {'p90':>10} {'win%':>6}")
    print("-" * 75)
    for regime_name in [r.name for r in REGIMES]:
        s = summary[regime_name]
        print(f"{regime_name:<18} {s['fills_median']:>6.0f} "
              f"{s['fill_rate_median']*100:>5.1f}% "
              f"${s['net_pnl_p10']:>9.2f} ${s['net_pnl_p50']:>9.2f} "
              f"${s['net_pnl_p90']:>9.2f} {s['win_rate']*100:>5.1f}%")

    if args.output:
        out_path = Path(args.output)
        out_path.parent.mkdir(parents=True, exist_ok=True)
        with out_path.open("w") as f:
            json.dump({
                "strategy": asdict(strategy),
                "args": vars(args),
                "regimes": [asdict(r) for r in REGIMES],
                "summary": summary,
                "raw_results": [asdict(r) for r in all_results],
            }, f, indent=2, default=str)
        print(f"\nResults written to {out_path}")


if __name__ == "__main__":
    main()
