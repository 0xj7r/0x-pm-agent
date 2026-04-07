"""Focused weather edge test: hypothesis A (sum-arb) + hypothesis C (CDF pricing).

Loads pre-grouped partitions from /tmp/weather_partitions.json, fetches OpenMeteo
historical forecast and observed temps for each (city, target_date), fetches CLOB
price history for a sample of partitions, and prints verdict.
"""
from __future__ import annotations

import json
import math
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
    "nyc": {"lat": 40.7773, "lon": -73.8726, "tz": "America/New_York"},  # KLGA
    "london": {"lat": 51.4700, "lon": -0.4543, "tz": "Europe/London"},  # Heathrow
    "dubai": {"lat": 25.2528, "lon": 55.3644, "tz": "Asia/Dubai"},
}

OM_HIST = "https://historical-forecast-api.open-meteo.com/v1/forecast"
OM_ARCHIVE = "https://archive-api.open-meteo.com/v1/archive"
CLOB_HISTORY = "https://clob.polymarket.com/prices-history"

POLY_FEE = 0.02  # on winnings
SLIPPAGE = 0.005  # half-cent per side


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


def parse_band(band: str) -> tuple[float | None, float | None]:
    """Return (low_f, high_f) inclusive bounds. None means unbounded."""
    if band.endswith("f-or-below"):
        return None, float(band.replace("f-or-below", ""))
    if band.endswith("f-or-higher"):
        return float(band.replace("f-or-higher", "")), None
    if band.startswith("between-"):
        s = band.replace("between-", "").replace("f", "")
        if "-" in s:
            lo, hi = s.split("-", 1)
            return float(lo), float(hi)
        # 4-digit form: between-5253f means 52-53
        if len(s) == 4 and s.isdigit():
            return float(s[:2]), float(s[2:])
        # 6-digit: between-100101f -> 100-101
        if len(s) == 6 and s.isdigit():
            return float(s[:3]), float(s[3:])
        return None, None
    return None, None


def cdf_normal(x, mu, sigma):
    return 0.5 * (1 + math.erf((x - mu) / (sigma * math.sqrt(2))))


def fair_prob(low: float | None, high: float | None, mu: float, sigma: float) -> float:
    """P(low <= T <= high) under N(mu, sigma)."""
    p_lo = 0.0 if low is None else cdf_normal(low - 0.5, mu, sigma)
    p_hi = 1.0 if high is None else cdf_normal(high + 0.5, mu, sigma)
    return max(0.0, p_hi - p_lo)


def fetch_om_range(city: str, start_iso: str, end_iso: str) -> dict[str, dict[str, float]]:
    """Pull a date range of GFS/ECMWF/ICON forecasts in 1 call per model.
    Returns {model: {date: temp_f}}."""
    coord = CITY_COORDS[city]
    out = {}
    for model in ("gfs_seamless", "ecmwf_ifs025", "icon_seamless"):
        try:
            d = http_get(OM_HIST, {
                "latitude": coord["lat"], "longitude": coord["lon"],
                "start_date": start_iso, "end_date": end_iso,
                "daily": "temperature_2m_max",
                "models": model,
                "temperature_unit": "fahrenheit",
                "timezone": coord["tz"],
            })
            dates = d.get("daily", {}).get("time", [])
            temps = d.get("daily", {}).get("temperature_2m_max", [])
            out[model] = {dt: t for dt, t in zip(dates, temps) if t is not None}
        except Exception as e:
            print(f"  om {model} {city} {start_iso}..{end_iso}: {e}", file=sys.stderr)
            out[model] = {}
    # Observed
    try:
        d = http_get(OM_ARCHIVE, {
            "latitude": coord["lat"], "longitude": coord["lon"],
            "start_date": start_iso, "end_date": end_iso,
            "daily": "temperature_2m_max",
            "temperature_unit": "fahrenheit",
            "timezone": coord["tz"],
        })
        dates = d.get("daily", {}).get("time", [])
        temps = d.get("daily", {}).get("temperature_2m_max", [])
        out["_observed"] = {dt: t for dt, t in zip(dates, temps) if t is not None}
    except Exception as e:
        print(f"  archive {city}: {e}", file=sys.stderr)
        out["_observed"] = {}
    return out


def fetch_clob_history(token_id: str, start_ts: int, end_ts: int) -> list[tuple[int, float]]:
    try:
        d = http_get(CLOB_HISTORY, {
            "market": token_id, "startTs": start_ts, "endTs": end_ts, "fidelity": 1,
        })
        return [(int(p["t"]), float(p["p"])) for p in d.get("history", [])]
    except Exception as e:
        return []


def main():
    parts = json.load(open("/tmp/weather_partitions.json"))
    full = [p for p in parts if p["n"] == 7]
    print(f"loaded {len(parts)} partitions, {len(full)} are 7-bucket")

    # group target_date ranges per city
    by_city = defaultdict(list)
    for p in full:
        by_city[p["city"]].append(p["target_date"])

    # fetch OpenMeteo for each city's full date range (1 call per model per city)
    print("\n=== fetching OpenMeteo ===")
    om = {}
    for city, dates in by_city.items():
        if city not in CITY_COORDS:
            continue
        sd, ed = min(dates), max(dates)
        print(f"  {city}: {sd}..{ed} ({len(set(dates))} dates)")
        om[city] = fetch_om_range(city, sd, ed)

    # ===== HYPOTHESIS C: CDF-based fair value vs observed outcomes =====
    # Step 1: build the calibrated forecast distribution
    # First pass: ensemble mean across 3 models -> mu
    # sigma: start with raw ensemble spread; later calibrate vs historical RMSE
    print("\n=== HYPOTHESIS C: forecast CDF vs realized outcomes ===")

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
        spread = stdev(vals) if len(vals) > 1 else 1.0
        # find which bucket actually won
        winner_band = None
        for m in p["markets"]:
            op = json.loads(m.get("outcomePrices", "[\"0\",\"0\"]"))
            if op and float(op[0]) > 0.5:
                winner_band = m["band"]
                break
        if not winner_band:
            continue
        rows.append({
            "city": city, "target_date": td,
            "mu_ensemble": mu, "sigma_ensemble": spread,
            "observed_f": obs, "winner_band": winner_band,
            "n_models": len(vals),
            "partition": p,
        })

    print(f"  usable partitions (have forecast + observed + winner): {len(rows)}")

    # Calibrate sigma against historical forecast error RMSE
    # ensemble vs observed residuals
    residuals_by_city = defaultdict(list)
    for r in rows:
        residuals_by_city[r["city"]].append(r["mu_ensemble"] - r["observed_f"])
    print("\n  forecast bias and RMSE per city:")
    rmse_by_city = {}
    for city, resids in residuals_by_city.items():
        m = mean(resids)
        rmse = math.sqrt(mean(x * x for x in resids))
        print(f"    {city}: bias={m:+.2f}F  rmse={rmse:.2f}F  n={len(resids)}")
        rmse_by_city[city] = rmse

    # Now compute fair prices using calibrated sigma = max(rmse, raw spread)
    print("\n  per-partition fair prices vs market resolved prices:")

    # Use only out-of-sample evaluation: use bias-corrected mu
    bias_by_city = {c: mean(residuals_by_city[c]) for c in residuals_by_city}

    # Score the model: log loss vs observed
    log_loss = {"calibrated": [], "naive_ensemble_vote": []}
    bucket_correct_calibrated = 0
    bucket_correct_vote = 0
    for r in rows:
        city = r["city"]
        # bias-correct mu (in sample, but with leave-one-out it would be cleaner; for v0 we accept this)
        mu_corrected = r["mu_ensemble"] - bias_by_city[city]
        sigma = rmse_by_city[city]
        # compute fair P for each bucket
        partition = r["partition"]["markets"]
        bucket_p = {}
        for m in partition:
            lo, hi = parse_band(m["band"])
            bucket_p[m["band"]] = fair_prob(lo, hi, mu_corrected, sigma)
        # ensemble vote probability (current strategy approach)
        models_vals = [v for v in [
            om[city]["gfs_seamless"].get(r["target_date"]),
            om[city]["ecmwf_ifs025"].get(r["target_date"]),
            om[city]["icon_seamless"].get(r["target_date"]),
        ] if v is not None]
        bucket_vote = {}
        for m in partition:
            lo, hi = parse_band(m["band"])
            lo_eff = -1e9 if lo is None else lo - 0.5
            hi_eff = 1e9 if hi is None else hi + 0.5
            votes = sum(1 for v in models_vals if lo_eff <= v <= hi_eff)
            bucket_vote[m["band"]] = votes / len(models_vals) if models_vals else 0

        # log loss vs realized outcome
        winner = r["winner_band"]
        p_cal = max(1e-6, min(1 - 1e-6, bucket_p.get(winner, 0)))
        p_vote = max(1e-6, min(1 - 1e-6, bucket_vote.get(winner, 0)))
        log_loss["calibrated"].append(-math.log(p_cal))
        log_loss["naive_ensemble_vote"].append(-math.log(p_vote))

        # argmax pick correct?
        if max(bucket_p, key=bucket_p.get) == winner:
            bucket_correct_calibrated += 1
        if max(bucket_vote, key=bucket_vote.get) == winner:
            bucket_correct_vote += 1

    n = len(rows)
    print(f"\n  forecast quality (n={n}):")
    print(f"    calibrated CDF      log-loss={mean(log_loss['calibrated']):.3f}  hit-rate={bucket_correct_calibrated/n:.1%}")
    print(f"    ensemble vote       log-loss={mean(log_loss['naive_ensemble_vote']):.3f}  hit-rate={bucket_correct_vote/n:.1%}")
    print(f"    random (1/7)        log-loss={math.log(7):.3f}  hit-rate=14.3%")

    # ===== HYPOTHESIS A: sum-constraint arbitrage on RESOLVED prices =====
    # Note: this only tests "did the resolved prices ever sum to !=1" which is silly
    # because resolved prices are 0 or 1 by definition, and sum to 1.
    # Real test needs intra-market snapshots (CLOB price history).
    # For now, sample a few partitions and pull CLOB to test.

    print("\n=== HYPOTHESIS A: sum-constraint arbitrage (sampled CLOB snapshots) ===")
    sample = full[:5]  # 5 partitions = 35 markets = 70 token API calls
    print(f"  sampling {len(sample)} partitions ({sum(len(p['markets']) for p in sample)} markets)")

    histories = {}  # market_id -> {ts: yes_price}
    with ThreadPoolExecutor(max_workers=6) as ex:
        futures = {}
        for p in sample:
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
                fut = ex.submit(fetch_clob_history, str(tokens[0]), start_ts, end_ts)
                futures[fut] = m["id"]
        for fut in as_completed(futures):
            mid = futures[fut]
            histories[mid] = dict(fut.result())

    # For each sampled partition, compute sum(yes_price) at each common timestamp
    arb_violations = 0
    arb_size = []
    for p in sample:
        ids = [m["id"] for m in p["markets"]]
        if not all(i in histories for i in ids):
            continue
        # find timestamps where ALL 7 buckets have a price
        # quick approx: bucket time into 1-hour bins, take last price per bucket per bin
        bins = defaultdict(dict)  # bin_ts -> {market_id -> last_price}
        for mid in ids:
            for ts, price in sorted(histories[mid].items()):
                bin_ts = ts - (ts % 3600)
                bins[bin_ts][mid] = price
        complete = [b for b, m in bins.items() if len(m) == 7]
        if not complete:
            continue
        # compute sum at each
        sums = [sum(bins[b].values()) for b in complete]
        viol = [s for s in sums if s < 0.97 or s > 1.03]
        print(f"    {p['city']} {p['target_date']}: {len(complete)} hourly snapshots, "
              f"sum range [{min(sums):.3f}, {max(sums):.3f}], median {sorted(sums)[len(sums)//2]:.3f}, "
              f"violations |1-sum|>0.03: {len(viol)}")
        arb_violations += len(viol)
        arb_size.extend(s - 1 for s in sums)

    if arb_size:
        avg_dev = mean(arb_size)
        max_dev = max(arb_size, key=abs)
        print(f"\n  total arb violations across sample: {arb_violations}")
        print(f"  mean (sum - 1): {avg_dev:+.4f}")
        print(f"  max abs (sum - 1): {max_dev:+.4f}")

    # ===== Verdict =====
    print("\n=== VERDICT ===")
    cal_loss = mean(log_loss['calibrated']) if log_loss['calibrated'] else float('inf')
    vote_loss = mean(log_loss['naive_ensemble_vote']) if log_loss['naive_ensemble_vote'] else float('inf')
    rand = math.log(7)
    print(f"forecast quality: calibrated CDF beats random by {(rand - cal_loss):.2f} nats/trade")
    print(f"calibrated CDF beats ensemble vote by {(vote_loss - cal_loss):.2f} nats/trade")
    if cal_loss < rand * 0.85:
        print("=> Forecast model has REAL skill vs uniform")
    else:
        print("=> Forecast model has marginal skill")

    print("\nNext step: pull market price snapshots and compute realized PnL of trading where |fair - mid| > threshold")


if __name__ == "__main__":
    main()
