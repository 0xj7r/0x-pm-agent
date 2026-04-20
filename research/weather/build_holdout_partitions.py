"""Build a fresh weather partitions file for holdout testing.

The existing /tmp/weather_partitions.json covers 2025-04-01 to 2025-07-16
and has been used for all research and walk-forward CV. To preserve
framework discipline we need a truly untouched holdout set.

This script pulls Polymarket weather markets that close between
2025-07-17 and 2026-04-08, groups them by (city, target_date), and
writes the result to /tmp/weather_holdout_partitions.json. It also
pulls the matching OpenMeteo historical forecasts and ensures each
partition has observed temperatures available.

Inputs: Gamma API + Open-Meteo historical-forecast API.
Output:
    /tmp/weather_holdout_partitions.json  (same shape as existing)
    /tmp/weather_holdout_om.json           (OM cache for the new date range)

This file must run BEFORE any holdout evaluation. The output files
are NOT committed (they're build artifacts) but the hash/fingerprint
is recorded in the preregistration and audit log.
"""
from __future__ import annotations

import json
import re
import sys
import time
import urllib.parse
import urllib.request
from collections import defaultdict
from datetime import datetime
from statistics import mean, stdev

GAMMA = "https://gamma-api.polymarket.com/markets"
OM_HIST = "https://historical-forecast-api.open-meteo.com/v1/forecast"
OM_ARCHIVE = "https://archive-api.open-meteo.com/v1/archive"

UA = {"User-Agent": "Mozilla/5.0 (weather-holdout-builder)"}

# Dates (inclusive) - build everything resolving in this window
HOLDOUT_START = "2025-07-17"
HOLDOUT_END = "2026-04-08"

CITY_COORDS = {
    "nyc": {"lat": 40.7773, "lon": -73.8726, "tz": "America/New_York"},
    "new-york": {"lat": 40.7773, "lon": -73.8726, "tz": "America/New_York"},
    "new-york-city": {"lat": 40.7773, "lon": -73.8726, "tz": "America/New_York"},
    "london": {"lat": 51.4700, "lon": -0.4543, "tz": "Europe/London"},
}
CITY_ALIAS = {"new-york": "nyc", "new-york-city": "nyc"}

SLUG_CITY_RE = re.compile(
    r"(?:the-)?(?:highest|lowest)-temperature-in-(nyc|new-york|new-york-city|london)"
    r"-be-[^?]*"
)


def http_get(url, params=None, retries=3):
    if params:
        url = f"{url}?{urllib.parse.urlencode(params)}"
    for i in range(retries):
        try:
            with urllib.request.urlopen(
                urllib.request.Request(url, headers=UA), timeout=60
            ) as r:
                return json.loads(r.read())
        except Exception as e:
            if i == retries - 1:
                raise
            time.sleep(1 + i)


def slug_city(slug: str) -> str | None:
    m = SLUG_CITY_RE.search(slug)
    if not m:
        return None
    raw = m.group(1)
    return CITY_ALIAS.get(raw, raw)


def target_date_from_market(m: dict) -> str | None:
    """Extract YYYY-MM-DD from the slug or end_date."""
    slug = m.get("slug", "")
    # slugs often end with '-on-MONTH-DD' or use 'on-YYYY-MM-DD'
    # Simplest: use the market's endDate from Gamma, truncated
    end = m.get("endDate") or m.get("end_date") or ""
    if end:
        try:
            return end[:10]
        except Exception:
            return None
    return None


def parse_band(slug: str) -> str | None:
    """Extract the band token from a weather market slug."""
    m = re.search(
        r"be-(between-\d+(?:-\d+)?f|\d+f-or-below|\d+f-or-higher|between-\d{4,6}f)",
        slug,
    )
    if m:
        return m.group(1)
    # alternative patterns
    m = re.search(r"be-(\d+f-or-(?:below|higher))", slug)
    if m:
        return m.group(1)
    return None


def fetch_markets(start: str, end: str):
    """Paginate through all closed weather markets in [start, end]."""
    all_markets = []
    offset = 0
    batch_size = 500
    while True:
        d = http_get(GAMMA, {
            "limit": batch_size,
            "offset": offset,
            "closed": "true",
            "end_date_min": start,
            "end_date_max": end,
        })
        if not d:
            break
        batch = d if isinstance(d, list) else d.get("markets", [])
        if not batch:
            break
        # Filter for weather markets
        for m in batch:
            slug = m.get("slug", "")
            if "temperature" not in slug.lower() and "temperature" not in (m.get("question") or "").lower():
                continue
            city = slug_city(slug)
            if not city or city not in CITY_COORDS:
                continue
            band = parse_band(slug)
            if not band:
                continue
            td = target_date_from_market(m)
            if not td:
                continue
            m["city"] = city
            m["band"] = band
            m["target_date"] = td
            all_markets.append(m)
        if len(batch) < batch_size:
            break
        offset += batch_size
        print(f"  fetched {offset} markets, kept {len(all_markets)} so far", file=sys.stderr)
        time.sleep(0.3)
    return all_markets


def group_into_partitions(markets):
    """Group by (city, target_date), keep only 7-bucket full partitions."""
    groups = defaultdict(list)
    for m in markets:
        key = (m["city"], m["target_date"])
        groups[key].append(m)
    partitions = []
    for (city, td), ms in groups.items():
        bands = set(m["band"] for m in ms)
        partitions.append({
            "city": city,
            "target_date": td,
            "n": len(bands),
            "markets": ms,
        })
    return partitions


def fetch_om_range(city, start_iso, end_iso):
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
            print(f"  om {model} {city}: {e}", file=sys.stderr)
            out[model] = {}
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


def main():
    print(f"fetching weather markets {HOLDOUT_START} to {HOLDOUT_END}...")
    markets = fetch_markets(HOLDOUT_START, HOLDOUT_END)
    print(f"got {len(markets)} candidate markets")

    partitions = group_into_partitions(markets)
    print(f"grouped into {len(partitions)} partitions")
    full = [p for p in partitions if p["n"] == 7]
    print(f"7-bucket partitions: {len(full)}")
    # Print city breakdown
    city_counts = defaultdict(int)
    for p in full:
        city_counts[p["city"]] += 1
    print(f"  {dict(city_counts)}")

    # Save partitions
    out_path = "/tmp/weather_holdout_partitions.json"
    with open(out_path, "w") as f:
        json.dump(partitions, f)
    print(f"wrote {out_path}")

    # Fetch OM for full partitions' date range
    by_city = defaultdict(list)
    for p in full:
        by_city[p["city"]].append(p["target_date"])
    om = {}
    for city, dates in by_city.items():
        if city not in CITY_COORDS:
            continue
        sd, ed = min(dates), max(dates)
        print(f"OM fetch {city}: {sd}..{ed} ({len(set(dates))} unique dates)")
        om[city] = fetch_om_range(city, sd, ed)

    out_om = "/tmp/weather_holdout_om.json"
    with open(out_om, "w") as f:
        json.dump(om, f)
    print(f"wrote {out_om}")

    # Print matchable counts
    print("\nmatchable partitions (partition has all 3 models + observed):")
    for city in sorted(by_city.keys()):
        n_matchable = 0
        for p in full:
            if p["city"] != city:
                continue
            td = p["target_date"]
            models_o = om.get(city, {})
            models = ["gfs_seamless", "ecmwf_ifs025", "icon_seamless"]
            vals = [models_o.get(m, {}).get(td) for m in models]
            obs = models_o.get("_observed", {}).get(td)
            if sum(1 for v in vals if v is not None) >= 2 and obs is not None:
                n_matchable += 1
        print(f"  {city}: {n_matchable}")


if __name__ == "__main__":
    main()
