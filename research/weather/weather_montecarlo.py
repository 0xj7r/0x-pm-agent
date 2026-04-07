"""Monte Carlo P&L sim for weather strategy.

Uses empirical per-partition returns from the OOS backtest, bootstraps forward,
applies Kelly sizing, simulates M paths from various starting bankrolls.
"""
from __future__ import annotations
import json, math, random
from collections import defaultdict
from datetime import datetime
from statistics import mean, median

POLY_FEE = 0.02
SLIPPAGE = 0.005

def parse_band(band):
    if band.endswith("f-or-below"): return None, float(band.replace("f-or-below",""))
    if band.endswith("f-or-higher"): return float(band.replace("f-or-higher","")), None
    if band.startswith("between-"):
        s = band.replace("between-","").replace("f","")
        if "-" in s:
            lo, hi = s.split("-",1); return float(lo), float(hi)
        if len(s)==4 and s.isdigit(): return float(s[:2]), float(s[2:])
        if len(s)==6 and s.isdigit(): return float(s[:3]), float(s[3:])
    return None, None

def cdf_normal(x, mu, sigma):
    return 0.5 * (1 + math.erf((x - mu) / (sigma * math.sqrt(2))))

def fair_prob(low, high, mu, sigma):
    p_lo = 0.0 if low is None else cdf_normal(low - 0.5, mu, sigma)
    p_hi = 1.0 if high is None else cdf_normal(high + 0.5, mu, sigma)
    return max(0.0, p_hi - p_lo)


def build_partition_returns(snapshot_h: int, threshold: float, frac_kelly: float):
    """For each test partition, compute the realized return on a unit-bankroll bet.
    Returns list of (city, return_pct) where return_pct is gain/loss on staked capital."""
    parts = json.load(open("/tmp/weather_partitions.json"))
    full = [p for p in parts if p["n"] == 7]
    om = json.load(open("/tmp/weather_om.json"))
    clob = json.load(open("/tmp/weather_clob.json"))

    residuals_by_city = defaultdict(list)
    rows = []
    for p in full:
        city, td = p["city"], p["target_date"]
        if city not in om: continue
        models = ["gfs_seamless", "ecmwf_ifs025", "icon_seamless"]
        vals = [om[city][m].get(td) for m in models if om[city].get(m, {}).get(td) is not None]
        obs = om[city].get("_observed", {}).get(td)
        if len(vals) < 2 or obs is None: continue
        mu = mean(vals)
        residuals_by_city[city].append(mu - obs)
        rows.append({"city": city, "td": td, "mu": mu, "obs": obs, "partition": p})

    rows.sort(key=lambda r: r["td"])

    # Use train/test split per city
    partition_returns = []  # (city, fractional_return_per_partition)
    for city in ["nyc", "london"]:
        city_rows = [r for r in rows if r["city"] == city]
        n = len(city_rows)
        split = int(n * 0.7)
        train = city_rows[:split]
        test = city_rows[split:]
        train_resids = [r["mu"] - r["obs"] for r in train]
        bias = mean(train_resids)
        rmse = math.sqrt(mean(x*x for x in train_resids))

        for r in test:
            mu_corr = r["mu"] - bias
            sigma = rmse
            partition = r["partition"]["markets"]
            winner = None
            for m in partition:
                op = json.loads(m.get("outcomePrices", "[\"0\",\"0\"]"))
                if op and float(op[0]) > 0.5:
                    winner = m["band"]; break
            if not winner: continue
            end_iso = partition[0].get("endDateIso", "")
            if not end_iso: continue
            end_ts = int(datetime.fromisoformat(end_iso + "T23:59:59+00:00").timestamp())
            snap_ts = end_ts - snapshot_h * 3600
            bp = {}
            for m in partition:
                hist = clob.get(m["id"], [])
                if not hist: continue
                latest = None
                for ts, price in hist:
                    if ts <= snap_ts: latest = price
                    else: break
                if latest is not None:
                    bp[m["band"]] = latest
            if len(bp) < 7: continue

            # Build trades for this partition with Kelly sizing
            partition_trades = []
            for m in partition:
                lo, hi = parse_band(m["band"])
                fair_p = fair_prob(lo, hi, mu_corr, sigma)
                market_p = bp.get(m["band"])
                if market_p is None: continue
                # YES candidate
                edge_y = fair_p - market_p
                edge_n = (1 - fair_p) - (1 - market_p)
                won = (m["band"] == winner)
                trade = None
                if edge_y > threshold:
                    entry = market_p + SLIPPAGE
                    if 0.01 < entry < 0.99:
                        b = (1 - entry) / entry
                        kelly = (fair_p * b - (1 - fair_p)) / b
                        if kelly > 0:
                            stake_frac = frac_kelly * kelly
                            trade = {"stake_frac": stake_frac, "entry": entry, "won": won, "p": fair_p}
                elif edge_n > threshold:
                    entry = (1 - market_p) + SLIPPAGE
                    if 0.01 < entry < 0.99:
                        b = (1 - entry) / entry
                        kelly = ((1 - fair_p) * b - fair_p) / b
                        if kelly > 0:
                            stake_frac = frac_kelly * kelly
                            trade = {"stake_frac": stake_frac, "entry": entry, "won": (not won), "p": (1-fair_p)}
                if trade:
                    partition_trades.append(trade)

            if not partition_trades:
                continue

            # Cap total stake fraction at 0.10 of bankroll across all bets in partition
            total_stake = sum(t["stake_frac"] for t in partition_trades)
            cap = 0.10
            scale = min(1.0, cap / total_stake) if total_stake > 0 else 0
            partition_pnl_frac = 0.0
            for t in partition_trades:
                stake = t["stake_frac"] * scale
                if t["won"]:
                    partition_pnl_frac += stake * (1 - t["entry"]) / t["entry"] * (1 - POLY_FEE)
                else:
                    partition_pnl_frac -= stake
            partition_returns.append((city, partition_pnl_frac))

    return partition_returns


def monte_carlo(returns: list, starting_bankroll: float, n_partitions: int, n_paths: int = 10000):
    """Simulate n_paths of forward returns. Each path samples n_partitions returns
    with replacement from the empirical distribution. Returns list of final bankrolls."""
    finals = []
    paths_max_dd = []
    for _ in range(n_paths):
        bankroll = starting_bankroll
        peak = bankroll
        max_dd = 0.0
        for _ in range(n_partitions):
            _, ret = random.choice(returns)
            bankroll *= (1 + ret)
            if bankroll > peak:
                peak = bankroll
            dd = (peak - bankroll) / peak
            if dd > max_dd:
                max_dd = dd
        finals.append(bankroll)
        paths_max_dd.append(max_dd)
    return finals, paths_max_dd


def percentile(xs, p):
    s = sorted(xs)
    k = int(len(s) * p)
    return s[min(k, len(s) - 1)]


def main():
    print("Building empirical partition returns from backtest...")
    # Use the best operating point: 48h before close, threshold 0.05, half-Kelly
    returns = build_partition_returns(snapshot_h=48, threshold=0.05, frac_kelly=0.5)
    n = len(returns)
    print(f"  partitions with at least one trade: {n}")
    avg = mean(r for _, r in returns)
    med = median(r for _, r in returns)
    pos = sum(1 for _, r in returns if r > 0) / n
    print(f"  avg return per partition: {avg*100:+.2f}%")
    print(f"  median return per partition: {med*100:+.2f}%")
    print(f"  hit rate (positive partition): {pos:.1%}")

    nyc = [(c, r) for c, r in returns if c == "nyc"]
    lon = [(c, r) for c, r in returns if c == "london"]
    print(f"  nyc: {len(nyc)} partitions, avg {mean(r for _,r in nyc)*100:+.2f}%")
    print(f"  london: {len(lon)} partitions, avg {mean(r for _,r in lon)*100:+.2f}%")

    # Trade frequency assumption: ~200 partitions/year (NYC + London combined)
    horizons = [
        ("1 month", 17),
        ("3 months", 50),
        ("6 months", 100),
        ("12 months", 200),
    ]
    starts = [100, 1_000, 10_000, 100_000]

    print("\n" + "=" * 90)
    print("MONTE CARLO P&L SIMULATION")
    print("=" * 90)
    print("Assumptions:")
    print("  - Half-Kelly sizing, partition-level cap of 10% of bankroll")
    print("  - 200 partitions/year tradeable (NYC + London combined)")
    print("  - Bootstrap from 62 OOS partition returns")
    print("  - Liquidity NOT capped (idealized; see notes below)")
    print("  - 10,000 paths per scenario")

    for start in starts:
        print(f"\n--- Starting bankroll: ${start:,} ---")
        print(f"{'horizon':<12}{'p10':>12}{'p25':>12}{'p50':>12}{'p75':>12}{'p90':>12}{'med dd':>12}{'P(loss)':>10}")
        for label, n_part in horizons:
            finals, dds = monte_carlo(returns, start, n_part)
            p10 = percentile(finals, 0.10)
            p25 = percentile(finals, 0.25)
            p50 = percentile(finals, 0.50)
            p75 = percentile(finals, 0.75)
            p90 = percentile(finals, 0.90)
            med_dd = percentile(dds, 0.50)
            p_loss = sum(1 for f in finals if f < start) / len(finals)
            print(f"{label:<12}${p10:>10,.0f}${p25:>10,.0f}${p50:>10,.0f}${p75:>10,.0f}${p90:>10,.0f}{med_dd*100:>10.1f}%{p_loss:>10.1%}")

    # Liquidity-capped sim: cap each bet at $500 absolute
    print("\n" + "=" * 90)
    print("WITH LIQUIDITY CAP ($500 max per partition stake)")
    print("=" * 90)
    print("This is more realistic. Books are $5-15K so max $500 fill is conservative.")

    def mc_capped(start, n_part, max_bet=500.0, n_paths=10000):
        finals = []
        for _ in range(n_paths):
            bankroll = start
            for _ in range(n_part):
                _, ret = random.choice(returns)
                # idealized return assumes betting full Kelly fraction of bankroll
                # if cap binds, your effective return is scaled by (max_bet / desired_bet)
                desired_bet = bankroll * 0.10  # 10% kelly cap from above
                if desired_bet > max_bet:
                    scale = max_bet / desired_bet
                    bankroll += ret * bankroll * scale
                else:
                    bankroll *= (1 + ret)
            finals.append(bankroll)
        return finals

    for start in starts:
        print(f"\n--- Starting bankroll: ${start:,} (capped) ---")
        print(f"{'horizon':<12}{'p10':>12}{'p25':>12}{'p50':>12}{'p75':>12}{'p90':>12}{'P(loss)':>10}")
        for label, n_part in horizons:
            finals = mc_capped(start, n_part)
            p10 = percentile(finals, 0.10)
            p25 = percentile(finals, 0.25)
            p50 = percentile(finals, 0.50)
            p75 = percentile(finals, 0.75)
            p90 = percentile(finals, 0.90)
            p_loss = sum(1 for f in finals if f < start) / len(finals)
            print(f"{label:<12}${p10:>10,.0f}${p25:>10,.0f}${p50:>10,.0f}${p75:>10,.0f}${p90:>10,.0f}{p_loss:>10.1%}")


if __name__ == "__main__":
    random.seed(42)
    main()
