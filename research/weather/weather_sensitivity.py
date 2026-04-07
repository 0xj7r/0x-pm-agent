"""Sensitivity analysis: vary snapshot time and threshold to test robustness."""
from __future__ import annotations
import json, math, os
from collections import defaultdict
from datetime import datetime
from statistics import mean, stdev

POLY_FEE = 0.02
SLIPPAGE = 0.005


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


def main():
    parts = json.load(open("/tmp/weather_partitions.json"))
    full = [p for p in parts if p["n"] == 7]
    om = json.load(open("/tmp/weather_om.json"))
    clob = json.load(open("/tmp/weather_clob.json"))

    # Build rows
    residuals_by_city = defaultdict(list)
    rows = []
    for p in full:
        city, td = p["city"], p["target_date"]
        if city not in om:
            continue
        models = ["gfs_seamless", "ecmwf_ifs025", "icon_seamless"]
        vals = [om[city][m].get(td) for m in models if om[city].get(m, {}).get(td) is not None]
        obs = om[city].get("_observed", {}).get(td)
        if len(vals) < 2 or obs is None:
            continue
        mu = mean(vals)
        residuals_by_city[city].append(mu - obs)
        rows.append({"city": city, "td": td, "mu": mu, "obs": obs, "partition": p})

    rows.sort(key=lambda r: r["td"])

    # Sweep over snapshot times and thresholds
    snapshot_hours = [72, 48, 36, 24, 18, 12, 6, 3, 1]
    thresholds = [0.03, 0.05, 0.08, 0.12]

    for city in ["nyc", "london"]:
        city_rows = [r for r in rows if r["city"] == city]
        n = len(city_rows)
        split = int(n * 0.7)
        train = city_rows[:split]
        test = city_rows[split:]
        train_resids = [r["mu"] - r["obs"] for r in train]
        bias = mean(train_resids)
        rmse = math.sqrt(mean(x*x for x in train_resids))

        print(f"\n=== {city.upper()} === (test n={len(test)}, bias={bias:+.2f}F, rmse={rmse:.2f}F)")
        print(f"{'snapshot_h':<12}", end="")
        for th in thresholds:
            print(f"thr={th:>4.2f}      ", end="")
        print()

        for sh in snapshot_hours:
            print(f"{sh:>10}h  ", end="")
            for th in thresholds:
                trades = []
                for r in test:
                    mu_corr = r["mu"] - bias
                    sigma = rmse
                    partition = r["partition"]["markets"]
                    winner = None
                    for m in partition:
                        op = json.loads(m.get("outcomePrices", "[\"0\",\"0\"]"))
                        if op and float(op[0]) > 0.5:
                            winner = m["band"]
                            break
                    if not winner:
                        continue
                    end_iso = partition[0].get("endDateIso", "")
                    if not end_iso:
                        continue
                    end_ts = int(datetime.fromisoformat(end_iso + "T23:59:59+00:00").timestamp())
                    snap_ts = end_ts - sh * 3600
                    bp = {}
                    for m in partition:
                        hist = clob.get(m["id"], [])
                        if not hist:
                            continue
                        latest = None
                        for ts, price in hist:
                            if ts <= snap_ts:
                                latest = price
                            else:
                                break
                        if latest is not None:
                            bp[m["band"]] = latest
                    if len(bp) < 7:
                        continue
                    for m in partition:
                        lo, hi = parse_band(m["band"])
                        fair_p = fair_prob(lo, hi, mu_corr, sigma)
                        market_p = bp.get(m["band"])
                        if market_p is None:
                            continue
                        edge_y = fair_p - market_p
                        edge_n = (1 - fair_p) - (1 - market_p)
                        won = (m["band"] == winner)
                        if edge_y > th:
                            entry = market_p + SLIPPAGE
                            if entry < 0.99:
                                pnl = (1 - entry) * (1 - POLY_FEE) if won else -entry
                                trades.append({"won": won, "entry": entry, "pnl": pnl})
                        elif edge_n > th:
                            entry = (1 - market_p) + SLIPPAGE
                            if entry < 0.99:
                                pnl = (1 - entry) * (1 - POLY_FEE) if not won else -entry
                                trades.append({"won": (not won), "entry": entry, "pnl": pnl})
                if trades:
                    cap = sum(t["entry"] for t in trades)
                    roi = sum(t["pnl"] for t in trades) / cap
                    wr = sum(1 for t in trades if t["won"]) / len(trades)
                    print(f"n={len(trades):3} roi={roi:+.1%}   ", end="")
                else:
                    print(f"n=0 roi=  -      ", end="")
            print()


if __name__ == "__main__":
    main()
