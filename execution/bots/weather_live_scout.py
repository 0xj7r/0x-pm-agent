"""Dry-run weather scout: enumerate trade candidates against live markets.

Fetches all active daily-temperature events on Polymarket, pulls current
ensemble forecasts from Open-Meteo for each city, computes fair
probability per temperature band via bias-corrected normal CDF, compares
to market prices, and prints trade candidates with edge + Kelly size.

DOES NOT SUBMIT ORDERS. This is the discovery + sizing step only.

Usage:
    python3 scripts/weather_live_scout.py
    python3 scripts/weather_live_scout.py --min-edge 0.05 --date 2026-04-22
"""
from __future__ import annotations

import argparse
import json
import math
import re
import sys
import urllib.parse
import urllib.request
from collections import defaultdict
from dataclasses import dataclass
from datetime import date, datetime, timedelta
from typing import Any

UA = {"User-Agent": "weather-scout/1.0"}
GAMMA_EVENTS = "https://gamma-api.polymarket.com/events"
OM_FORECAST = "https://api.open-meteo.com/v1/forecast"
OM_ENSEMBLE = "https://ensemble-api.open-meteo.com/v1/ensemble"
# Models exposed as ensembles via ensemble-api. Each returns N members.
ENSEMBLE_MODELS = ("gfs_seamless", "ecmwf_ifs025", "icon_seamless")

# City coords = ASOS/METAR airport station coordinates. Polymarket
# resolves daily-temperature markets using Weather Underground data,
# which pulls from the station codes below. Using city-center coords
# would put our forecast 5-15 miles off the resolution station and
# introduce systematic 1-3°F bias in narrow-bucket probabilities.
#
# VERIFY each station by checking the "Resolution criteria" on the
# specific Polymarket event before trading. Some cities may use a
# different station than the primary airport.
CITY_COORDS: dict[str, dict[str, Any]] = {
    # station: KLGA LaGuardia (confirmed resolution source for NYC)
    "nyc":          {"lat": 40.7772, "lon": -73.8726, "tz": "America/New_York", "station": "KLGA"},
    "new-york":     {"lat": 40.7772, "lon": -73.8726, "tz": "America/New_York", "station": "KLGA"},
    # station: EGLL Heathrow (standard resolution source for London markets)
    "london":       {"lat": 51.4700, "lon": -0.4543, "tz": "Europe/London",     "station": "EGLL"},
    # station: LFPG Charles de Gaulle
    "paris":        {"lat": 49.0097, "lon": 2.5479,  "tz": "Europe/Paris",      "station": "LFPG"},
    # station: KDFW Dallas-Fort Worth
    "dallas":       {"lat": 32.8998, "lon": -97.0403, "tz": "America/Chicago",  "station": "KDFW"},
    # station: KATL Hartsfield-Jackson
    "atlanta":      {"lat": 33.6407, "lon": -84.4277, "tz": "America/New_York", "station": "KATL"},
    # station: KMIA Miami International
    "miami":        {"lat": 25.7932, "lon": -80.2906, "tz": "America/New_York", "station": "KMIA"},
    # station: KSEA Seattle-Tacoma
    "seattle":      {"lat": 47.4502, "lon": -122.3088, "tz": "America/Los_Angeles", "station": "KSEA"},
    # station: CYYZ Toronto Pearson
    "toronto":      {"lat": 43.6777, "lon": -79.6248, "tz": "America/Toronto",  "station": "CYYZ"},
    # station: SBGR Guarulhos
    "sao-paulo":    {"lat": -23.4356, "lon": -46.4731, "tz": "America/Sao_Paulo", "station": "SBGR"},
    # station: SAEZ Ezeiza
    "buenos-aires": {"lat": -34.8222, "lon": -58.5358, "tz": "America/Argentina/Buenos_Aires", "station": "SAEZ"},
    # station: RKSI Incheon (Seoul's primary international)
    "seoul":        {"lat": 37.4602, "lon": 126.4407, "tz": "Asia/Seoul",       "station": "RKSI"},
    # station: RJTT Tokyo Haneda (domestic; Narita RJAA is alternative)
    "tokyo":        {"lat": 35.5494, "lon": 139.7798, "tz": "Asia/Tokyo",       "station": "RJTT"},
    # station: VHHH Hong Kong International
    "hong-kong":    {"lat": 22.3080, "lon": 113.9185, "tz": "Asia/Hong_Kong",   "station": "VHHH"},
    # station: ZSPD Shanghai Pudong
    "shanghai":     {"lat": 31.1443, "lon": 121.8083, "tz": "Asia/Shanghai",    "station": "ZSPD"},
    # station: WSSS Singapore Changi
    "singapore":    {"lat": 1.3644, "lon": 103.9915, "tz": "Asia/Singapore",    "station": "WSSS"},
    # station: VILK Lucknow
    "lucknow":      {"lat": 26.7606, "lon": 80.8893, "tz": "Asia/Kolkata",      "station": "VILK"},
    # station: LLBG Ben Gurion
    "tel-aviv":     {"lat": 32.0114, "lon": 34.8866, "tz": "Asia/Jerusalem",    "station": "LLBG"},
    # station: EDDM Munich
    "munich":       {"lat": 48.3538, "lon": 11.7861, "tz": "Europe/Berlin",     "station": "EDDM"},
    # station: LTAC Ankara Esenboga
    "ankara":       {"lat": 40.1281, "lon": 32.9951, "tz": "Europe/Istanbul",   "station": "LTAC"},
}

# Per-city bias (F) and RMSE (F) baked in from the 3.5-month training
# corpus (weather_om.json). Cities without training data use a generic
# default; as forecast-vs-observed data accumulates we refit.
DEFAULT_BIAS = 0.0
DEFAULT_RMSE = 2.5
CITY_STATS: dict[str, dict[str, float]] = {
    # PREVIOUSLY had calibrated bias/rmse for nyc + london from
    # weather_om.json. Those were computed against city-center
    # gridpoints (40.7128,-74.006 for nyc; 51.5074,-0.1278 for london).
    # Now that we query the airport stations (KLGA, EGLL), the old
    # calibration doesn't apply — airport microclimate differs.
    # All cities use DEFAULT_BIAS=0.0 / DEFAULT_RMSE=2.5 until we
    # re-backfill forecast-vs-observed against the airport stations.
}


EVENT_SLUG_RE = re.compile(r"^highest-temperature-in-([a-z\-]+?)-on-([a-z]+)-(\d+)-(\d{4})$")


def http_get(url: str, params: dict[str, Any] | None = None, timeout: float = 30.0) -> Any:
    if params:
        url = f"{url}?{urllib.parse.urlencode(params)}"
    with urllib.request.urlopen(urllib.request.Request(url, headers=UA), timeout=timeout) as r:
        return json.loads(r.read())


def parse_event_slug(slug: str) -> tuple[str, date] | None:
    m = EVENT_SLUG_RE.match(slug)
    if not m:
        return None
    city = m.group(1)
    month_name, day, year = m.group(2), int(m.group(3)), int(m.group(4))
    try:
        target = datetime.strptime(f"{month_name} {day} {year}", "%B %d %Y").date()
    except ValueError:
        return None
    return city, target


def parse_band_suffix(submarket_slug: str, event_slug: str) -> tuple[float | None, float | None]:
    """Extract (low_f, high_f) from a sub-market slug, given its event slug.

    Returns (None, high) for `-Xforbelow`, (low, None) for `-Xforhigher`,
    (low, high) for `-X-Yf`. Returns (None, None) if slug doesn't match.
    """
    if not submarket_slug.startswith(event_slug + "-"):
        return None, None
    suffix = submarket_slug[len(event_slug) + 1:]
    if suffix.endswith("forbelow"):
        try:
            return None, float(suffix[:-len("forbelow")])
        except ValueError:
            return None, None
    if suffix.endswith("forhigher"):
        try:
            return float(suffix[:-len("forhigher")]), None
        except ValueError:
            return None, None
    if suffix.endswith("f") and "-" in suffix:
        s = suffix[:-1]
        try:
            lo, hi = s.split("-", 1)
            return float(lo), float(hi)
        except ValueError:
            return None, None
    return None, None


def fetch_events(tag: str = "daily-temperature", limit: int = 200) -> list[dict]:
    return http_get(GAMMA_EVENTS, {
        "tag_slug": tag,
        "closed": "false",
        "active": "true",
        "limit": limit,
    })


def fetch_ensemble_members(city: str, target_date: date) -> list[float]:
    """Return all ensemble member forecasts of daily max temp (F) for the
    target date, combined across GFS + ECMWF + ICON. Each model
    typically exposes 20-50 members; combined we usually get 80-120+.

    Open-Meteo's ensemble API returns hourly member values as
    `temperature_2m_member01`, `_member02`, etc. We aggregate to daily
    max per member manually because `temperature_2m_max` isn't always
    exposed per-member in the ensemble endpoint.
    """
    coord = CITY_COORDS.get(city)
    if not coord:
        return []
    target_iso = target_date.isoformat()
    all_members: list[float] = []
    for model in ENSEMBLE_MODELS:
        try:
            data = http_get(OM_ENSEMBLE, {
                "latitude": coord["lat"],
                "longitude": coord["lon"],
                "models": model,
                "hourly": "temperature_2m",
                "start_date": target_iso,
                "end_date": target_iso,
                "timezone": coord["tz"],
                "temperature_unit": "fahrenheit",
            })
        except Exception as exc:
            print(f"  WARN: ensemble {model} {city}: {exc}", file=sys.stderr)
            continue
        hourly = data.get("hourly", {}) or {}
        times = hourly.get("time", []) or []
        if not times:
            continue
        # Find member keys like "temperature_2m_member01"
        member_keys = [k for k in hourly if k.startswith("temperature_2m_member")]
        if not member_keys:
            # Fallback: single deterministic series under "temperature_2m"
            series = hourly.get("temperature_2m")
            if series:
                day_max = max(v for v in series if v is not None)
                all_members.append(float(day_max))
            continue
        for key in member_keys:
            series = hourly.get(key, []) or []
            vals = [v for v in series if v is not None]
            if not vals:
                continue
            all_members.append(float(max(vals)))
    return all_members


def cdf_normal(x: float, mu: float, sigma: float) -> float:
    if sigma <= 0:
        return 1.0 if x >= mu else 0.0
    return 0.5 * (1.0 + math.erf((x - mu) / (sigma * math.sqrt(2.0))))


def fair_prob(low_f: float | None, high_f: float | None, mu_f: float, sigma_f: float) -> float:
    """P(forecast value falls in [low, high] in Fahrenheit) assuming
    normal error. Uses +/- 0.5F boundary smoothing (markets round to
    nearest integer F)."""
    p_lo = 0.0 if low_f is None else cdf_normal(low_f - 0.5, mu_f, sigma_f)
    p_hi = 1.0 if high_f is None else cdf_normal(high_f + 0.5, mu_f, sigma_f)
    return max(0.0, p_hi - p_lo)


def binary_kelly_fraction(probability: float, price: float) -> float:
    if price <= 0 or price >= 1 or probability <= price:
        return 0.0
    b = (1.0 - price) / price
    q = 1.0 - probability
    edge = b * probability - q
    return max(0.0, edge / b) if edge > 0 else 0.0


@dataclass(frozen=True)
class Candidate:
    event_slug: str
    city: str
    target_date: date
    market_slug: str
    low_f: float | None
    high_f: float | None
    market_price: float
    model_prob: float
    edge: float
    consensus: float
    kelly_fraction: float


def ensemble_bucket_prob(members: list[float], low_f: float | None, high_f: float | None) -> float:
    """Empirical probability that a member's forecast lands in [low, high]
    in Fahrenheit. Uses +/- 0.5F boundary smoothing (markets round to
    nearest integer F)."""
    if not members:
        return 0.0
    lo = -float("inf") if low_f is None else low_f - 0.5
    hi = float("inf") if high_f is None else high_f + 0.5
    in_band = sum(1 for m in members if lo <= m <= hi)
    return in_band / len(members)


def evaluate_event(event: dict, min_edge: float, min_consensus: float) -> list[Candidate]:
    slug = event.get("slug", "")
    parsed = parse_event_slug(slug)
    if not parsed:
        return []
    city, target = parsed
    if city not in CITY_COORDS:
        return []

    members_raw = fetch_ensemble_members(city, target)
    if len(members_raw) < 20:
        return []  # too thin an ensemble to trust

    # Apply city bias correction if we have calibration data.
    stats = CITY_STATS.get(city, {"bias": DEFAULT_BIAS, "rmse": DEFAULT_RMSE})
    bias_f = stats["bias"]
    members = [m - bias_f for m in members_raw]

    # Consensus signal: 1 - (ensemble std / calibrated RMSE), clipped to
    # [0, 1]. Higher = members agree more than typical forecast error.
    mean = sum(members) / len(members)
    variance = sum((m - mean) ** 2 for m in members) / max(1, len(members) - 1)
    ensemble_std = variance ** 0.5
    consensus = max(0.0, min(1.0, 1.0 - ensemble_std / max(stats["rmse"], 0.5)))

    candidates: list[Candidate] = []
    for mkt in event.get("markets", []):
        mslug = mkt.get("slug", "")
        low_f, high_f = parse_band_suffix(mslug, slug)
        if low_f is None and high_f is None:
            continue
        try:
            price = float(mkt.get("lastTradePrice") or 0.0)
        except (TypeError, ValueError):
            continue
        if price <= 0.0 or price >= 0.99:
            continue
        prob = ensemble_bucket_prob(members, low_f, high_f)
        edge = prob - price
        if edge < min_edge:
            continue
        if consensus < min_consensus:
            continue
        kelly = binary_kelly_fraction(prob, price)
        candidates.append(Candidate(
            event_slug=slug, city=city, target_date=target,
            market_slug=mslug, low_f=low_f, high_f=high_f,
            market_price=price, model_prob=prob, edge=edge,
            consensus=consensus, kelly_fraction=kelly,
        ))
    return candidates


def band_label(low_f: float | None, high_f: float | None) -> str:
    if low_f is None:
        return f"≤{high_f:.0f}F"
    if high_f is None:
        return f"≥{low_f:.0f}F"
    return f"{low_f:.0f}-{high_f:.0f}F"


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--min-edge", type=float, default=0.05)
    ap.add_argument("--min-consensus", type=float, default=0.0)
    ap.add_argument("--date", type=str, default="", help="Filter to target_date YYYY-MM-DD")
    ap.add_argument("--bankroll", type=float, default=100.0)
    ap.add_argument("--kelly-mult", type=float, default=0.25)
    ap.add_argument("--max-pct", type=float, default=0.05)
    args = ap.parse_args()

    events = fetch_events()
    print(f"[scout] {len(events)} active daily-temperature events fetched")

    by_city_count: dict[str, int] = defaultdict(int)
    all_candidates: list[Candidate] = []
    unsupported_cities: set[str] = set()

    date_filter: date | None = None
    if args.date:
        date_filter = datetime.strptime(args.date, "%Y-%m-%d").date()

    for ev in events:
        parsed = parse_event_slug(ev.get("slug", ""))
        if not parsed:
            continue
        city, target = parsed
        if date_filter and target != date_filter:
            continue
        by_city_count[city] += 1
        if city not in CITY_COORDS:
            unsupported_cities.add(city)
            continue
        try:
            cands = evaluate_event(ev, args.min_edge, args.min_consensus)
        except Exception as exc:
            print(f"[scout] error on {ev.get('slug')}: {exc}", file=sys.stderr)
            continue
        all_candidates.extend(cands)

    print(f"[scout] cities seen: {dict(by_city_count)}")
    if unsupported_cities:
        print(f"[scout] cities without lat/lon mapping (skipped): {sorted(unsupported_cities)}")
    print(f"[scout] trade candidates (edge >= {args.min_edge}): {len(all_candidates)}")
    print("")

    # Sort by edge descending
    all_candidates.sort(key=lambda c: c.edge, reverse=True)
    print(f"{'city':<14s} {'date':<12s} {'band':<10s} {'price':>6s} {'model_p':>8s} {'edge':>6s} {'cons':>5s} {'kelly':>6s} {'size':>7s}")
    print("-" * 90)
    for c in all_candidates[:40]:
        size = args.bankroll * min(c.kelly_fraction * args.kelly_mult, args.max_pct)
        print(
            f"{c.city:<14s} {c.target_date.isoformat():<12s} {band_label(c.low_f, c.high_f):<10s} "
            f"{c.market_price:>6.3f} {c.model_prob:>8.3f} {c.edge:>+6.3f} {c.consensus:>5.2f} "
            f"{c.kelly_fraction:>6.3f} ${size:>6.2f}"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
