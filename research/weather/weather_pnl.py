"""Extended weather edge test: compute realized PnL using calibrated CDF vs market prices over time.

Uses partitions from /tmp/weather_partitions.json. Pulls CLOB history for ALL
markets (cached to /tmp/weather_clob.json). Pulls OpenMeteo. Runs out-of-sample
PnL backtest with fees.
"""
from __future__ import annotations

import json
import math
import os
import sys
import time
import urllib.parse
import urllib.request
from collections import defaultdict
from concurrent.futures import ThreadPoolExecutor, as_completed
from datetime import datetime
from statistics import mean, stdev

UA = {"User-Agent": "Mozilla/5.0 (research)"}

CITY_COORDS = {
    "nyc": {"lat": 40.7773, "lon": -73.8726, "tz": "America/New_York"},
    "london": {"lat": 51.4700, "lon": -0.4543, "tz": "Europe/London"},
    "dubai": {"lat": 25.2528, "lon": 55.3644, "tz": "Asia/Dubai"},
}

OM_HIST = "https://historical-forecast-api.open-meteo.com/v1/forecast"
OM_ARCHIVE = "https://archive-api.open-meteo.com/v1/archive"
CLOB_HISTORY = "https://clob.polymarket.com/prices-history"

POLY_FEE = 0.02  # on winnings
SLIPPAGE = 0.005  # half-cent each side


def http_get(url, params, retries=3):
    full = f"{url}?{urllib.parse.urlencode(params)}"
    for i in range(retries):
        try:
            with urllib.request.urlopen(urllib.request.Request(full, headers=UA), timeout=60) as r:
                return json.loads(r.read())
        except Exception as e:
            if i == retries - 1:
                raise
            time.sleep(1 + i)


def parse_band(band: str):
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


def fetch_om_range(city, start_iso, end_iso):
    coord = CITY_COORDS[city]
    out = {}
    for model in ("gfs_seamless", "ecmwf_ifs025", "icon_seamless"):
        try:
            d = http_get(OM_HIST, {
                "latitude": coord["lat"], "longitude": coord["lon"],
                "start_date": start_iso, "end_date": end_iso,
                "daily": "temperature_2m_max", "models": model,
                "temperature_unit": "fahrenheit", "timezone": coord["tz"],
            })
            dates = d.get("daily", {}).get("time", [])
            temps = d.get("daily", {}).get("temperature_2m_max", [])
            out[model] = {dt: t for dt, t in zip(dates, temps) if t is not None}
        except Exception:
            out[model] = {}
    try:
        d = http_get(OM_ARCHIVE, {
            "latitude": coord["lat"], "longitude": coord["lon"],
            "start_date": start_iso, "end_date": end_iso,
            "daily": "temperature_2m_max",
            "temperature_unit": "fahrenheit", "timezone": coord["tz"],
        })
        dates = d.get("daily", {}).get("time", [])
        temps = d.get("daily", {}).get("temperature_2m_max", [])
        out["_observed"] = {dt: t for dt, t in zip(dates, temps) if t is not None}
    except Exception:
        out["_observed"] = {}
    return out


def fetch_clob_history(token_id, start_ts, end_ts):
    try:
        d = http_get(CLOB_HISTORY, {
            "market": token_id, "startTs": start_ts, "endTs": end_ts, "fidelity": 1,
        })
        return [(int(p["t"]), float(p["p"])) for p in d.get("history", [])]
    except Exception:
        return []


def main():
    parts = json.load(open("/tmp/weather_partitions.json"))
    full = [p for p in parts if p["n"] == 7]
    print(f"loaded {len(full)} 7-bucket partitions")

    # OM cache
    om_cache_path = "/tmp/weather_om.json"
    if os.path.exists(om_cache_path):
        om = json.load(open(om_cache_path))
        print("loaded OM cache")
    else:
        by_city = defaultdict(list)
        for p in full:
            by_city[p["city"]].append(p["target_date"])
        om = {}
        for city, dates in by_city.items():
            if city not in CITY_COORDS:
                continue
            sd, ed = min(dates), max(dates)
            print(f"OM fetch {city}: {sd}..{ed}")
            om[city] = fetch_om_range(city, sd, ed)
        json.dump(om, open(om_cache_path, "w"))
        print("OM cached")

    # CLOB cache
    clob_cache_path = "/tmp/weather_clob.json"
    if os.path.exists(clob_cache_path):
        clob = json.load(open(clob_cache_path))
        print(f"loaded CLOB cache: {len(clob)} markets")
    else:
        clob = {}
        # Pull all markets in parallel
        tasks = []
        for p in full:
            for m in p["markets"]:
                tokens = json.loads(m.get("clobTokenIds", "[]"))
                if len(tokens) != 2:
                    continue
                end_iso = m.get("endDateIso", "")
                start_iso = m.get("startDateIso", "")
                if not end_iso or not start_iso:
                    continue
                start_ts = int(datetime.fromisoformat(start_iso + "T00:00:00+00:00").timestamp())
                end_ts = int(datetime.fromisoformat(end_iso + "T23:59:59+00:00").timestamp())
                tasks.append((m["id"], str(tokens[0]), start_ts, end_ts))
        print(f"CLOB fetch {len(tasks)} markets...")

        def pull(t):
            mid, tok, st, et = t
            return mid, fetch_clob_history(tok, st, et)

        with ThreadPoolExecutor(max_workers=8) as ex:
            futures = [ex.submit(pull, t) for t in tasks]
            done = 0
            for fut in as_completed(futures):
                mid, h = fut.result()
                clob[mid] = h
                done += 1
                if done % 100 == 0:
                    print(f"  CLOB progress: {done}/{len(tasks)}")
        json.dump(clob, open(clob_cache_path, "w"))
        print("CLOB cached")

    # ===== Build per-(city, target_date) snapshot table =====
    # for each partition, find timestamps where all 7 buckets have a recent price
    # use a 1-hour bin granularity

    # First: per-city forecast bias / RMSE for calibration
    residuals_by_city = defaultdict(list)
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
        mu = mean(vals)
        residuals_by_city[city].append(mu - obs)
        rows.append({
            "city": city, "td": td, "mu": mu,
            "raw_spread": stdev(vals) if len(vals) > 1 else 1.0,
            "obs": obs, "partition": p,
        })

    bias_by_city = {c: mean(r) for c, r in residuals_by_city.items() if r}
    rmse_by_city = {c: math.sqrt(mean(x*x for x in r)) for c, r in residuals_by_city.items() if r}
    print("\nbias / rmse per city:")
    for c in bias_by_city:
        print(f"  {c}: bias={bias_by_city[c]:+.2f}F  rmse={rmse_by_city[c]:.2f}F")

    # ===== Out-of-sample backtest =====
    # Time-based split: first 70% train (computes bias + rmse), last 30% test
    # For honest OOS, recompute calibration using only train data
    print("\n=== OOS PnL backtest ===")
    rows.sort(key=lambda r: r["td"])

    # We do per-city OOS to be honest
    results = {}
    for city in ["nyc", "london"]:
        city_rows = [r for r in rows if r["city"] == city]
        n = len(city_rows)
        split = int(n * 0.7)
        train = city_rows[:split]
        test = city_rows[split:]
        if not test:
            continue

        train_resids = [r["mu"] - r["obs"] for r in train]
        bias = mean(train_resids)
        rmse = math.sqrt(mean(x*x for x in train_resids))
        print(f"\n{city}: train={len(train)}, test={len(test)}, bias={bias:+.2f}F, rmse={rmse:.2f}F")

        # For each test partition, score trades
        # snapshot strategy: take the LAST snapshot before market resolution
        # (mimics "trade once, late in market life, with current ensemble forecast")
        # This is simplest to test. Real impl might trade multiple times.
        trades = []
        for r in test:
            mu_corr = r["mu"] - bias
            sigma = rmse
            partition = r["partition"]["markets"]
            # winner
            winner = None
            for m in partition:
                op = json.loads(m.get("outcomePrices", "[\"0\",\"0\"]"))
                if op and float(op[0]) > 0.5:
                    winner = m["band"]
                    break
            if not winner:
                continue
            # find a snapshot time: median of last-trade timestamps minus 6 hours from end
            # for simplicity: take a single snapshot ~24h before market end
            # (need to determine the timestamp from CLOB data)
            # Use: snapshot_ts = end_ts - 24*3600
            # Get the latest trade BEFORE that timestamp for each bucket
            end_iso = partition[0].get("endDateIso", "")
            if not end_iso:
                continue
            end_ts = int(datetime.fromisoformat(end_iso + "T23:59:59+00:00").timestamp())
            snapshot_ts = end_ts - 12*3600  # 12h before resolution
            bucket_market_p = {}
            for m in partition:
                mid = m["id"]
                hist = clob.get(mid, [])
                if not hist:
                    continue
                # find latest <= snapshot_ts
                latest = None
                for ts, price in hist:
                    if ts <= snapshot_ts:
                        latest = price
                    else:
                        break
                if latest is not None:
                    bucket_market_p[m["band"]] = latest
            if len(bucket_market_p) < 7:
                continue
            # compute fair P for each bucket
            for m in partition:
                lo, hi = parse_band(m["band"])
                fair_p = fair_prob(lo, hi, mu_corr, sigma)
                market_p = bucket_market_p.get(m["band"])
                if market_p is None:
                    continue
                # YES edge
                edge_yes = fair_p - market_p
                # NO edge
                fair_no = 1 - fair_p
                market_no = 1 - market_p
                edge_no = fair_no - market_no
                # decide trade
                # threshold = 0.05 (5pt) over fees + slippage
                threshold = 0.05
                won = (m["band"] == winner)
                # YES trade
                if edge_yes > threshold:
                    entry = market_p + SLIPPAGE
                    if entry < 0.99:
                        if won:
                            payoff = (1 - entry) * (1 - POLY_FEE)
                        else:
                            payoff = -entry
                        trades.append({"side": "YES", "city": city, "td": r["td"],
                                       "band": m["band"], "fair": fair_p, "market": market_p,
                                       "entry": entry, "won": won, "pnl": payoff})
                # NO trade
                elif edge_no > threshold:
                    entry = market_no + SLIPPAGE
                    if entry < 0.99:
                        if not won:
                            payoff = (1 - entry) * (1 - POLY_FEE)
                        else:
                            payoff = -entry
                        trades.append({"side": "NO", "city": city, "td": r["td"],
                                       "band": m["band"], "fair": fair_p, "market": market_p,
                                       "entry": entry, "won": (not won), "pnl": payoff})
        if not trades:
            print(f"  no trades")
            continue
        wins = sum(1 for t in trades if t["won"])
        total_pnl = sum(t["pnl"] for t in trades)
        avg_entry = mean(t["entry"] for t in trades)
        print(f"  trades: {len(trades)}, wins: {wins} ({wins/len(trades):.1%}), total PnL: {total_pnl:+.2f}, avg entry: {avg_entry:.3f}")
        print(f"  PnL per trade: {total_pnl/len(trades):+.4f}")
        # ROI: total PnL / total capital deployed
        capital = sum(t["entry"] for t in trades)
        print(f"  ROI on deployed capital: {total_pnl/capital:+.1%}")
        results[city] = {
            "n_trades": len(trades),
            "wins": wins,
            "total_pnl": total_pnl,
            "pnl_per_trade": total_pnl / len(trades),
            "roi": total_pnl / capital,
        }

    print("\n=== SUMMARY ===")
    print(json.dumps(results, indent=2))


if __name__ == "__main__":
    main()
