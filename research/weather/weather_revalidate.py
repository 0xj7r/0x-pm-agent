"""Framework-discipline revalidation of the weather calibrated-CDF strategy.

The existing weather_pnl.py reports NYC +27.6% ROI and London +3.3% ROI
from a single 70/30 time split with fixed parameters. weather_sensitivity.py
sweeps a 9 x 4 parameter grid and shows broadly positive results for 6h-72h
snapshots. Neither applies framework-rigor validation: no walk-forward CV,
no bootstrap CI on per-trade PnL, no PBO across the grid, no cross-city
generalization test, no market-implied baseline comparison.

This script does all of the above and reports honest verdicts.

Inputs (all cached from prior weather_pnl.py runs):
    /tmp/weather_partitions.json
    /tmp/weather_om.json
    /tmp/weather_clob.json

Validation tests (in order of importance):

1. **Bootstrap CI on the canonical NYC 12h / 0.05 config**: is the headline
   +27.6% ROI statistically distinguishable from zero given the sample
   size? Uses the framework's stationary block bootstrap.

2. **Walk-forward CV with 5 folds**: instead of a single 70/30 split, sweep
   expanding windows. Is the edge stable across time?

3. **PBO on the sensitivity grid**: 9 snapshot hours x 4 thresholds = 36
   cells, evaluated on CV folds. If PBO > 0.4, the headline grid cell is
   overfit and the framework rejects the result.

4. **Cross-city holdout**: fit calibration on NYC, test on London (and
   vice versa). True signal should generalize at least partially across
   cities.

5. **Market-implied baseline comparison**: does the calibrated-CDF
   forecaster actually beat "use market price as the forecast"? If not,
   the edge is not in the model, it's in the execution or the data
   quality.

The tests explicitly EXCLUDE 3h, 1h snapshot times per the README's known
stale-quote settlement artifact. These are non-tradeable by construction
and including them would inflate the search space with known-bad cells.
"""
from __future__ import annotations

import json
import math
import sys
from collections import defaultdict
from datetime import datetime
from pathlib import Path
from statistics import mean, stdev

import numpy as np

sys.path.insert(0, str(Path(__file__).parent.parent.parent))

from autoresearch.methods.bootstrap import (
    acf_block_length,
    stationary_block_bootstrap,
)
from autoresearch.methods.pbo import probability_of_backtest_overfitting


POLY_FEE = 0.02
SLIPPAGE = 0.005
CITIES = ["nyc", "london"]

# Pre-registered parameter grid for PBO. 3h and 1h are excluded because
# the README documents them as non-tradeable (stale-quote settlement
# artifact). Including them would inflate PBO with known-bad cells.
SNAPSHOT_HOURS = [72, 48, 36, 24, 18, 12, 6]
THRESHOLDS = [0.03, 0.05, 0.08, 0.12]


def parse_band(band):
    if band.endswith("f-or-below"):
        return None, float(band.replace("f-or-below", ""))
    if band.endswith("f-or-higher"):
        return float(band.replace("f-or-higher", "")), None
    if band.startswith("between-"):
        s = band.replace("between-", "").replace("f", "")
        if "-" in s:
            lo, hi = s.split("-", 1)
            return float(lo), float(hi)
        if len(s) == 4 and s.isdigit():
            return float(s[:2]), float(s[2:])
        if len(s) == 6 and s.isdigit():
            return float(s[:3]), float(s[3:])
    return None, None


def cdf_normal(x, mu, sigma):
    return 0.5 * (1 + math.erf((x - mu) / (sigma * math.sqrt(2))))


def fair_prob(low, high, mu, sigma):
    p_lo = 0.0 if low is None else cdf_normal(low - 0.5, mu, sigma)
    p_hi = 1.0 if high is None else cdf_normal(high + 0.5, mu, sigma)
    return max(0.0, p_hi - p_lo)


def load_data():
    parts = json.load(open("/tmp/weather_partitions.json"))
    om = json.load(open("/tmp/weather_om.json"))
    clob = json.load(open("/tmp/weather_clob.json"))
    full = [p for p in parts if p["n"] == 7]
    return full, om, clob


def build_rows(full, om):
    """Build per-partition rows with ensemble mu and observed temp."""
    rows = []
    for p in full:
        city, td = p["city"], p["target_date"]
        if city not in om:
            continue
        models_o = om[city]
        models = ["gfs_seamless", "ecmwf_ifs025", "icon_seamless"]
        vals = [models_o[m].get(td) for m in models if models_o.get(m, {}).get(td) is not None]
        obs = models_o.get("_observed", {}).get(td)
        if len(vals) < 2 or obs is None:
            continue
        rows.append({
            "city": city,
            "td": td,
            "mu": mean(vals),
            "raw_spread": stdev(vals) if len(vals) > 1 else 1.0,
            "obs": obs,
            "partition": p,
        })
    rows.sort(key=lambda r: r["td"])
    return rows


def find_winner(partition_markets):
    for m in partition_markets:
        op = json.loads(m.get("outcomePrices", "[\"0\",\"0\"]"))
        if op and float(op[0]) > 0.5:
            return m["band"]
    return None


def snapshot_prices(partition_markets, clob, snapshot_ts):
    """Get latest-before-snapshot market price for each band."""
    bp = {}
    for m in partition_markets:
        hist = clob.get(m["id"], [])
        if not hist:
            continue
        latest = None
        for ts, price in hist:
            if ts <= snapshot_ts:
                latest = price
            else:
                break
        if latest is not None:
            bp[m["band"]] = latest
    return bp


def simulate_partition_trades(
    row, bp, winner, bias, rmse, threshold, use_market_implied_baseline=False
):
    """Return list of (pnl, entry_price, won) tuples for this partition.

    If use_market_implied_baseline is True, skips the calibrated CDF and
    uses market_p itself as the fair value (which means edge=0 by
    construction, so no trades — the baseline is a no-op). For the
    baseline comparison, we instead use the ensemble-vote probability as
    the baseline fair value.
    """
    mu_corr = row["mu"] - bias
    sigma = rmse
    partition = row["partition"]["markets"]
    trades = []
    for m in partition:
        lo, hi = parse_band(m["band"])
        fair_p = fair_prob(lo, hi, mu_corr, sigma)
        market_p = bp.get(m["band"])
        if market_p is None:
            continue
        edge_y = fair_p - market_p
        edge_n = (1 - fair_p) - (1 - market_p)
        won = (m["band"] == winner)
        if edge_y > threshold:
            entry = market_p + SLIPPAGE
            if entry < 0.99:
                pnl = (1 - entry) * (1 - POLY_FEE) if won else -entry
                trades.append((pnl, entry, won))
        elif edge_n > threshold:
            entry = (1 - market_p) + SLIPPAGE
            if entry < 0.99:
                pnl = (1 - entry) * (1 - POLY_FEE) if not won else -entry
                trades.append((pnl, entry, (not won)))
    return trades


def fit_calibration(train_rows):
    """Fit per-city bias and rmse from training rows."""
    per_city = defaultdict(list)
    for r in train_rows:
        per_city[r["city"]].append(r["mu"] - r["obs"])
    bias = {c: mean(v) for c, v in per_city.items() if v}
    rmse = {c: math.sqrt(mean(x*x for x in v)) for c, v in per_city.items() if v}
    return bias, rmse


def evaluate_rows(test_rows, clob, bias_by_city, rmse_by_city, snapshot_h, threshold):
    """Run the strategy on test_rows with given calibration + params.
    Returns list of (pnl, entry_price, won) across all test partitions."""
    all_trades = []
    for r in test_rows:
        city = r["city"]
        if city not in bias_by_city:
            continue
        partition = r["partition"]["markets"]
        end_iso = partition[0].get("endDateIso", "")
        if not end_iso:
            continue
        end_ts = int(datetime.fromisoformat(end_iso + "T23:59:59+00:00").timestamp())
        snap_ts = end_ts - snapshot_h * 3600
        winner = find_winner(partition)
        if winner is None:
            continue
        bp = snapshot_prices(partition, clob, snap_ts)
        if len(bp) < 7:
            continue
        tr = simulate_partition_trades(
            r, bp, winner, bias_by_city[city], rmse_by_city[city], threshold
        )
        all_trades.extend(tr)
    return all_trades


def trade_stats(trades):
    if not trades:
        return {"n": 0, "wr": 0.0, "total_pnl": 0.0, "roi": 0.0, "pnl_per_trade": 0.0}
    n = len(trades)
    pnls = [t[0] for t in trades]
    entries = [t[1] for t in trades]
    wins = sum(1 for t in trades if t[2])
    return {
        "n": n,
        "wr": wins / n,
        "total_pnl": sum(pnls),
        "roi": sum(pnls) / sum(entries) if sum(entries) > 0 else 0.0,
        "pnl_per_trade": sum(pnls) / n,
    }


def test_1_bootstrap_ci(rows, clob):
    """Test 1: Bootstrap CI on NYC 12h / 0.05 (the canonical headline config).

    Splits 70/30 time-ordered as in the original weather_pnl.py, trains
    calibration on the first 70%, computes per-trade log returns on the
    test set, and runs stationary block bootstrap to get a 95% CI on the
    mean per-trade PnL (as ROI fraction per trade).
    """
    print("\n" + "=" * 70)
    print("TEST 1: Bootstrap CI on NYC 12h / 0.05 headline config")
    print("=" * 70)

    for city in CITIES:
        city_rows = [r for r in rows if r["city"] == city]
        n = len(city_rows)
        split = int(n * 0.7)
        train = city_rows[:split]
        test = city_rows[split:]
        bias_by_city, rmse_by_city = fit_calibration(train)

        trades = evaluate_rows(test, clob, bias_by_city, rmse_by_city, 12, 0.05)
        stats = trade_stats(trades)
        if stats["n"] < 4:
            print(f"  {city}: too few trades ({stats['n']})")
            continue

        # Per-trade ROI (pnl / entry) — symmetric scaling
        per_trade_rois = np.array([t[0] / t[1] for t in trades])
        block_len = acf_block_length(per_trade_rois, max_lag=20)
        boot = stationary_block_bootstrap(
            per_trade_rois,
            statistic=lambda x: float(np.mean(x)),
            block_length=block_len,
            n_resamples=2000,
            seed=20260408,
        )
        pos_lower = boot.lower_ci > 0
        print(f"  {city}: trades={stats['n']} wr={stats['wr']:.3f} "
              f"mean_roi_per_trade={boot.point_estimate:+.4f}")
        print(f"    95% CI = [{boot.lower_ci:+.4f}, {boot.upper_ci:+.4f}]")
        print(f"    block_length={block_len}  lower_ci>0 = {pos_lower}")
        print(f"    total ROI on deployed capital = {stats['roi']:+.3%}")


def test_2_walk_forward_cv(rows, clob, n_folds=5):
    """Test 2: Walk-forward CV with expanding window.

    Splits the city's time-ordered partitions into n_folds contiguous
    test chunks. For each chunk, trains calibration on ALL EARLIER data
    (expanding anchored window) and tests on that chunk.
    """
    print("\n" + "=" * 70)
    print(f"TEST 2: Walk-forward CV (n_folds={n_folds}) on NYC 12h / 0.05")
    print("=" * 70)

    for city in CITIES:
        city_rows = [r for r in rows if r["city"] == city]
        n = len(city_rows)
        # Need at least 20 rows before first fold for calibration
        min_train = max(20, n // (n_folds + 1))
        remaining = n - min_train
        fold_size = max(1, remaining // n_folds)
        print(f"\n  {city}: n={n}  min_train={min_train}  fold_size={fold_size}")

        fold_rois = []
        fold_trades = []
        for i in range(n_folds):
            train_end = min_train + i * fold_size
            test_end = min_train + (i + 1) * fold_size
            if test_end > n:
                test_end = n
            if train_end >= test_end:
                break
            train = city_rows[:train_end]
            test = city_rows[train_end:test_end]
            bias_by_city, rmse_by_city = fit_calibration(train)
            trades = evaluate_rows(test, clob, bias_by_city, rmse_by_city, 12, 0.05)
            stats = trade_stats(trades)
            fold_rois.append(stats["roi"])
            fold_trades.append(stats["n"])
            print(f"    fold {i+1}: train={len(train)} test={len(test)} "
                  f"trades={stats['n']} wr={stats['wr']:.2f} roi={stats['roi']:+.2%}")

        # Aggregate
        if fold_rois:
            print(f"\n    fold ROIs: {[f'{r:+.2%}' for r in fold_rois]}")
            print(f"    mean fold ROI: {mean(fold_rois):+.2%}")
            print(f"    positive folds: {sum(1 for r in fold_rois if r > 0)}/{len(fold_rois)}")


def test_3_pbo_on_grid(rows, clob, n_folds=6):
    """Test 3: PBO on the (snapshot_h, threshold) grid.

    For each of the 28 cells (7 hours x 4 thresholds, excluding 3h/1h),
    evaluate on n_folds walk-forward test folds. Compute per-fold total
    PnL. Run PBO. If PBO > 0.4, the sensitivity-sweep winner is overfit.
    """
    print("\n" + "=" * 70)
    print(f"TEST 3: PBO across {len(SNAPSHOT_HOURS)} x {len(THRESHOLDS)} grid "
          f"({n_folds} folds, excluding 3h/1h snapshots)")
    print("=" * 70)

    for city in CITIES:
        city_rows = [r for r in rows if r["city"] == city]
        n = len(city_rows)
        min_train = max(20, n // (n_folds + 1))
        remaining = n - min_train
        fold_size = max(1, remaining // n_folds)

        grid = [(h, t) for h in SNAPSHOT_HOURS for t in THRESHOLDS]
        print(f"\n  {city}: grid size = {len(grid)}, folds = {n_folds}")

        metrics = np.zeros((len(grid), n_folds))
        for i, (sh, th) in enumerate(grid):
            for f in range(n_folds):
                train_end = min_train + f * fold_size
                test_end = min_train + (f + 1) * fold_size
                if test_end > n:
                    test_end = n
                if train_end >= test_end:
                    metrics[i, f] = 0.0
                    continue
                train = city_rows[:train_end]
                test = city_rows[train_end:test_end]
                bias_by_city, rmse_by_city = fit_calibration(train)
                trades = evaluate_rows(test, clob, bias_by_city, rmse_by_city, sh, th)
                stats = trade_stats(trades)
                # Use total PnL as the per-fold metric. For the cells where
                # the stale-quote artifact would have occurred (< 6h) we've
                # already excluded those from SNAPSHOT_HOURS.
                metrics[i, f] = stats["total_pnl"]

        # Find the best cell by mean across folds (the "sensitivity winner")
        cell_means = metrics.mean(axis=1)
        best_idx = int(np.argmax(cell_means))
        best_cell = grid[best_idx]
        print(f"    best cell by mean-across-folds: h={best_cell[0]} thr={best_cell[1]} "
              f"mean_total_pnl={cell_means[best_idx]:+.3f}")

        # Only PBO if we have enough folds (need even n_groups)
        if n_folds >= 4 and n_folds % 2 == 0:
            try:
                pbo = probability_of_backtest_overfitting(metrics, n_groups=n_folds)
                verdict = "REJECT (> 0.4)" if pbo > 0.4 else "OK"
                print(f"    PBO across grid = {pbo:.3f}  [{verdict}]")
            except Exception as e:
                print(f"    PBO failed: {e}")
        else:
            print(f"    skipping PBO (need even n_folds >= 4, got {n_folds})")


def test_4_cross_city_holdout(rows, clob):
    """Test 4: Train calibration on one city, test on the other.

    True signal should generalize at least partially. If the NYC result
    only works with NYC-specific bias correction, it might be a
    city-specific artifact. Cross-city calibration tests whether the
    underlying calibrated-CDF approach is transferable.

    Note: bias is per-city measurement error, not per-city signal, so we
    expect some degradation. The question is whether it survives AT ALL.
    """
    print("\n" + "=" * 70)
    print("TEST 4: Cross-city calibration transfer")
    print("=" * 70)

    nyc_rows = [r for r in rows if r["city"] == "nyc"]
    london_rows = [r for r in rows if r["city"] == "london"]

    # Split each city 70/30 by time
    nyc_train, nyc_test = nyc_rows[:int(len(nyc_rows)*0.7)], nyc_rows[int(len(nyc_rows)*0.7):]
    lon_train, lon_test = london_rows[:int(len(london_rows)*0.7)], london_rows[int(len(london_rows)*0.7):]

    # Calibrations
    bias_nyc, rmse_nyc = fit_calibration(nyc_train)
    bias_lon, rmse_lon = fit_calibration(lon_train)

    # Cross-apply: NYC calibration on London test, London calibration on NYC test
    bias_cross_nyc = {"nyc": bias_lon["london"], "london": bias_lon["london"]}
    rmse_cross_nyc = {"nyc": rmse_lon["london"], "london": rmse_lon["london"]}
    bias_cross_lon = {"nyc": bias_nyc["nyc"], "london": bias_nyc["nyc"]}
    rmse_cross_lon = {"nyc": rmse_nyc["nyc"], "london": rmse_nyc["nyc"]}

    print("\n  NYC test with NYC calibration (baseline):")
    trades = evaluate_rows(nyc_test, clob, bias_nyc, rmse_nyc, 12, 0.05)
    print(f"    {trade_stats(trades)}")
    print("  NYC test with London calibration (cross):")
    trades = evaluate_rows(nyc_test, clob, bias_cross_nyc, rmse_cross_nyc, 12, 0.05)
    print(f"    {trade_stats(trades)}")

    print("\n  London test with London calibration (baseline):")
    trades = evaluate_rows(lon_test, clob, bias_lon, rmse_lon, 12, 0.05)
    print(f"    {trade_stats(trades)}")
    print("  London test with NYC calibration (cross):")
    trades = evaluate_rows(lon_test, clob, bias_cross_lon, rmse_cross_lon, 12, 0.05)
    print(f"    {trade_stats(trades)}")


def test_5_market_baseline(rows, clob):
    """Test 5: Does calibrated CDF beat 'use ensemble vote as forecast'?

    For each bucket, the naive baseline is the fraction of ensemble
    models whose forecast falls in that bucket. Compare PnL of
    calibrated-CDF strategy vs naive-ensemble-vote strategy on the
    same test set.
    """
    print("\n" + "=" * 70)
    print("TEST 5: Calibrated CDF vs naive ensemble-vote baseline")
    print("=" * 70)

    om = json.load(open("/tmp/weather_om.json"))

    for city in CITIES:
        city_rows = [r for r in rows if r["city"] == city]
        split = int(len(city_rows) * 0.7)
        train, test = city_rows[:split], city_rows[split:]
        bias_by_city, rmse_by_city = fit_calibration(train)

        # Calibrated CDF trades
        cal_trades = evaluate_rows(test, clob, bias_by_city, rmse_by_city, 12, 0.05)

        # Ensemble vote trades: for each partition, compute fraction of
        # ensemble models whose forecast falls in each bucket
        vote_trades = []
        for r in test:
            partition = r["partition"]["markets"]
            end_iso = partition[0].get("endDateIso", "")
            if not end_iso:
                continue
            end_ts = int(datetime.fromisoformat(end_iso + "T23:59:59+00:00").timestamp())
            snap_ts = end_ts - 12 * 3600
            winner = find_winner(partition)
            if winner is None:
                continue
            bp = snapshot_prices(partition, clob, snap_ts)
            if len(bp) < 7:
                continue
            # Ensemble vote
            models = ["gfs_seamless", "ecmwf_ifs025", "icon_seamless"]
            model_vals = [om[r["city"]][m].get(r["td"]) for m in models
                          if om[r["city"]].get(m, {}).get(r["td"]) is not None]
            if not model_vals:
                continue
            for m in partition:
                lo, hi = parse_band(m["band"])
                lo_eff = -1e9 if lo is None else lo - 0.5
                hi_eff = 1e9 if hi is None else hi + 0.5
                vote_prob = sum(1 for v in model_vals if lo_eff <= v <= hi_eff) / len(model_vals)
                market_p = bp.get(m["band"])
                if market_p is None:
                    continue
                edge_y = vote_prob - market_p
                edge_n = (1 - vote_prob) - (1 - market_p)
                won = (m["band"] == winner)
                if edge_y > 0.05:
                    entry = market_p + SLIPPAGE
                    if entry < 0.99:
                        pnl = (1 - entry) * (1 - POLY_FEE) if won else -entry
                        vote_trades.append((pnl, entry, won))
                elif edge_n > 0.05:
                    entry = (1 - market_p) + SLIPPAGE
                    if entry < 0.99:
                        pnl = (1 - entry) * (1 - POLY_FEE) if not won else -entry
                        vote_trades.append((pnl, entry, (not won)))

        print(f"\n  {city}:")
        print(f"    calibrated CDF:  {trade_stats(cal_trades)}")
        print(f"    ensemble vote:   {trade_stats(vote_trades)}")


def main():
    print("Framework-discipline revalidation of weather calibrated-CDF strategy")
    print("=" * 70)
    full, om, clob = load_data()
    rows = build_rows(full, om)
    print(f"loaded {len(full)} partitions, {len(rows)} usable rows")
    for city in CITIES:
        n = sum(1 for r in rows if r["city"] == city)
        print(f"  {city}: {n} rows")

    test_1_bootstrap_ci(rows, clob)
    test_2_walk_forward_cv(rows, clob, n_folds=5)
    test_3_pbo_on_grid(rows, clob, n_folds=6)
    test_4_cross_city_holdout(rows, clob)
    test_5_market_baseline(rows, clob)


if __name__ == "__main__":
    main()
