"""Pull fresh Polymarket weather partitions for a holdout evaluation.

The existing /tmp/weather_partitions.json covers 2025-04-01 to 2025-07-16
(105 unique dates, 205 7-bucket partitions, NYC + London primarily).
All of this has been consumed by the walk-forward CV in
weather_revalidate.py, which means there is no untouched holdout for a
proper one-shot promotion test.

This script pulls fresh data from 2025-07-17 onwards and writes three
files to /tmp:

    /tmp/weather_partitions_holdout.json
    /tmp/weather_om_holdout.json
    /tmp/weather_clob_holdout.json

Usage:
    python3 research/weather/build_holdout.py
    python3 research/weather/build_holdout.py --start 2025-07-17 --end 2026-04-07

Deliberately simple and slow. No caching across runs. Re-run produces a
fresh snapshot. Used once for a preregistered holdout burn; the
preregistration commit hash is the receipt.
"""
from __future__ import annotations

import argparse
import json
import re
import sys
import time
import urllib.parse
import urllib.request
from collections import defaultdict
from concurrent.futures import ThreadPoolExecutor, as_completed
from datetime import date, datetime

UA = {"User-Agent": "Mozilla/5.0 (research-holdout)"}
GAMMA = "https://gamma-api.polymarket.com/markets"
CLOB_HISTORY = "https://clob.polymarket.com/prices-history"
OM_HIST = "https://historical-forecast-api.open-meteo.com/v1/forecast"
OM_ARCHIVE = "https://archive-api.open-meteo.com/v1/archive"

CITY_COORDS = {
    "nyc": {"lat": 40.7773, "lon": -73.8726, "tz": "America/New_York"},
    "london": {"lat": 51.4700, "lon": -0.4543, "tz": "Europe/London"},
}
CITY_ALIAS = {"new-york": "nyc", "new-york-city": "nyc"}


def http_get(url, params=None, retries=3):
    if params:
        url = f"{url}?{urllib.parse.urlencode(params)}"
    last_err = None
    for i in range(retries):
        try:
            with urllib.request.urlopen(urllib.request.Request(url, headers=UA), timeout=30) as r:
                return json.loads(r.read())
        except Exception as e:
            last_err = e
            time.sleep(1 + i * 2)
    raise RuntimeError(f"http_get failed: {last_err}")


def parse_slug(slug: str):
    """Extract (city, target_date_iso, band) from a weather market slug.

    Expected patterns:
      will-the-highest-temperature-in-nyc-be-<band>-on-<month>-<day>
      will-the-highest-temperature-in-london-be-<band>-on-<month>-<day>
    Returns None if the slug does not parse as a weather market.
    """
    m = re.search(
        r"will-the-highest-temperature-in-([a-z\-]+?)-be-"
        r"(between-[\d\-]+f|[\d]+f-or-below|[\d]+f-or-higher)-on-"
        r"([a-z]+)-(\d+)",
        slug,
    )
    if not m:
        return None
    city_raw = m.group(1)
    city = CITY_ALIAS.get(city_raw, city_raw)
    if city not in CITY_COORDS:
        return None
    band = m.group(2)
    month_name = m.group(3)
    day = int(m.group(4))
    # Return city + band + partial date; the caller resolves the year from end_time
    return city, band, month_name, day


def fetch_partitions(start_iso: str, end_iso: str) -> list:
    """Paginate Gamma for closed weather markets in the date window and
    group them into (city, target_date) partitions."""
    all_markets = []
    offset = 0
    limit = 500
    print(f"fetching Gamma closed weather markets {start_iso} to {end_iso}")
    while True:
        try:
            batch = http_get(GAMMA, {
                "limit": limit,
                "offset": offset,
                "closed": "true",
                "end_date_min": start_iso,
                "end_date_max": end_iso,
            })
        except Exception as e:
            print(f"  pagination stopped at offset {offset}: {e}")
            break
        if not batch:
            break
        n_before = len(all_markets)
        for m in batch:
            slug = m.get("slug", "")
            parsed = parse_slug(slug)
            if parsed is None:
                continue
            city, band, month_name, day = parsed
            end_time = m.get("endDate") or ""
            if not end_time:
                continue
            try:
                end_dt = datetime.fromisoformat(end_time.replace("Z", "+00:00"))
            except ValueError:
                continue
            target_date = end_dt.date().isoformat()
            all_markets.append({
                "id": m.get("id"),
                "slug": slug,
                "city": city,
                "band": band,
                "target_date": target_date,
                "endDateIso": target_date,
                "startDateIso": m.get("startDate", "")[:10] if m.get("startDate") else "",
                "clobTokenIds": m.get("clobTokenIds", "[]"),
                "outcomePrices": m.get("outcomePrices", "[\"0\",\"0\"]"),
            })
        added = len(all_markets) - n_before
        print(f"  offset {offset}: batch={len(batch)} weather_added={added} total={len(all_markets)}")
        if len(batch) < limit:
            break
        offset += limit
        time.sleep(0.3)

    # Group into partitions
    partitions_dict = defaultdict(list)
    for m in all_markets:
        key = (m["city"], m["target_date"])
        partitions_dict[key].append(m)

    partitions = []
    for (city, td), markets in partitions_dict.items():
        partitions.append({
            "city": city,
            "target_date": td,
            "n": len(markets),
            "markets": markets,
        })
    print(f"grouped into {len(partitions)} partitions, "
          f"{sum(1 for p in partitions if p['n'] == 7)} are full 7-bucket")
    return partitions


def fetch_om_range(city: str, start_iso: str, end_iso: str) -> dict:
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
        except Exception as e:
            print(f"  om {model} {city} failed: {e}")
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
    except Exception as e:
        print(f"  archive {city} failed: {e}")
        out["_observed"] = {}
    return out


def fetch_clob_history(token_id: str, start_ts: int, end_ts: int) -> list:
    try:
        d = http_get(CLOB_HISTORY, {
            "market": token_id, "startTs": start_ts, "endTs": end_ts, "fidelity": 1,
        })
        return [(int(p["t"]), float(p["p"])) for p in d.get("history", [])]
    except Exception:
        return []


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--start", default="2025-07-17")
    ap.add_argument("--end", default=date.today().isoformat())
    args = ap.parse_args()

    # Fetch partitions
    partitions = fetch_partitions(args.start, args.end)
    full = [p for p in partitions if p["n"] == 7]
    print(f"\nsaving {len(partitions)} partitions ({len(full)} 7-bucket)")
    json.dump(partitions, open("/tmp/weather_partitions_holdout.json", "w"))
    print("  /tmp/weather_partitions_holdout.json")

    # Fetch OM per city
    print("\nfetching Open-Meteo historical forecasts and archive...")
    om = {}
    for city in CITY_COORDS:
        dates = sorted(set(p["target_date"] for p in full if p["city"] == city))
        if not dates:
            continue
        sd, ed = dates[0], dates[-1]
        print(f"  {city}: {sd}..{ed} ({len(dates)} dates)")
        om[city] = fetch_om_range(city, sd, ed)
    json.dump(om, open("/tmp/weather_om_holdout.json", "w"))
    print("  /tmp/weather_om_holdout.json")

    # Fetch CLOB history for all 7-bucket partition markets
    print(f"\nfetching CLOB price history for {sum(len(p['markets']) for p in full)} markets...")
    tasks = []
    for p in full:
        for m in p["markets"]:
            tokens = json.loads(m.get("clobTokenIds", "[]"))
            if len(tokens) != 2:
                continue
            start_iso = m.get("startDateIso", "")
            end_iso = m.get("endDateIso", "")
            if not start_iso or not end_iso:
                continue
            try:
                start_ts = int(datetime.fromisoformat(start_iso + "T00:00:00+00:00").timestamp())
                end_ts = int(datetime.fromisoformat(end_iso + "T23:59:59+00:00").timestamp())
            except ValueError:
                continue
            tasks.append((m["id"], str(tokens[0]), start_ts, end_ts))

    print(f"  {len(tasks)} market fetch tasks")
    clob = {}
    with ThreadPoolExecutor(max_workers=6) as ex:
        futures = {
            ex.submit(fetch_clob_history, tok, st, et): mid
            for mid, tok, st, et in tasks
        }
        done = 0
        for fut in as_completed(futures):
            mid = futures[fut]
            try:
                clob[mid] = fut.result()
            except Exception:
                clob[mid] = []
            done += 1
            if done % 100 == 0:
                print(f"    CLOB progress: {done}/{len(tasks)}")

    json.dump(clob, open("/tmp/weather_clob_holdout.json", "w"))
    print("  /tmp/weather_clob_holdout.json")

    # Sanity checks
    n_with_history = sum(1 for v in clob.values() if v)
    print(f"\nmarkets with CLOB history: {n_with_history}/{len(clob)}")
    print("HOLDOUT DATA READY")


if __name__ == "__main__":
    main()
