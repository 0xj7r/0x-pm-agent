"""One-shot research backfill for Polymarket weather markets.

Pulls every closed/resolved daily-temperature market for NYC and London
in a configurable date window, fetches per-market price history from CLOB,
fetches the Open-Meteo historical forecast (deterministic + ensemble spread),
and stores everything in research/weather/backfill.db.

This is research code. Not meant for production. Throwaway.
"""

from __future__ import annotations

import argparse
import json
import logging
import re
import sqlite3
import sys
import time
from concurrent.futures import ThreadPoolExecutor, as_completed
from datetime import date, datetime, timezone
from pathlib import Path

import urllib.parse
import urllib.request

LOG = logging.getLogger("backfill")

GAMMA = "https://gamma-api.polymarket.com/markets"
CLOB_HISTORY = "https://clob.polymarket.com/prices-history"
OM_HISTFORECAST = "https://historical-forecast-api.open-meteo.com/v1/forecast"
OM_ENSEMBLE = "https://ensemble-api.open-meteo.com/v1/ensemble"
OM_ARCHIVE = "https://archive-api.open-meteo.com/v1/archive"

# settlement-station coordinates (per Polymarket market descriptions)
CITY_COORDS = {
    "nyc": {"lat": 40.7773, "lon": -73.8726, "tz": "America/New_York", "label": "NYC (KLGA)"},
    "new-york": {"lat": 40.7773, "lon": -73.8726, "tz": "America/New_York", "label": "NYC (KLGA)"},
    "new-york-city": {"lat": 40.7773, "lon": -73.8726, "tz": "America/New_York", "label": "NYC (KLGA)"},
    "london": {"lat": 51.4700, "lon": -0.4543, "tz": "Europe/London", "label": "London (Heathrow)"},
    "miami": {"lat": 25.7959, "lon": -80.2870, "tz": "America/New_York", "label": "Miami (KMIA)"},
    "los-angeles": {"lat": 33.9416, "lon": -118.4085, "tz": "America/Los_Angeles", "label": "LA (KLAX)"},
    "dubai": {"lat": 25.2528, "lon": 55.3644, "tz": "Asia/Dubai", "label": "Dubai (OMDB)"},
    "tokyo": {"lat": 35.5494, "lon": 139.7798, "tz": "Asia/Tokyo", "label": "Tokyo (RJTT)"},
    "berlin": {"lat": 52.5597, "lon": 13.2877, "tz": "Europe/Berlin", "label": "Berlin (EDDB)"},
    "paris": {"lat": 48.7253, "lon": 2.3653, "tz": "Europe/Paris", "label": "Paris (LFPO)"},
    "buenos-aires": {"lat": -34.5592, "lon": -58.4156, "tz": "America/Argentina/Buenos_Aires", "label": "BA (SAEZ)"},
}

# normalize new-york -> nyc for downstream grouping
CITY_ALIAS = {"new-york": "nyc", "new-york-city": "nyc"}

UA = {"User-Agent": "Mozilla/5.0 (research-backfill)"}


def http_get_json(url: str, params: dict | None = None, retries: int = 3, sleep: float = 1.0):
    if params:
        url = f"{url}?{urllib.parse.urlencode(params)}"
    for attempt in range(retries):
        try:
            req = urllib.request.Request(url, headers=UA)
            with urllib.request.urlopen(req, timeout=60) as r:
                return json.loads(r.read())
        except Exception as e:
            if attempt == retries - 1:
                raise
            LOG.warning("retry %s for %s: %s", attempt + 1, url[:80], e)
            time.sleep(sleep * (attempt + 1))


SLUG_RE = re.compile(
    r"^will-the-highest-temperature-in-(?P<city>[a-z\-]+?)-be-(?P<band>.+?)-on-(?P<month>[a-z]+)-(?P<day>\d+)$"
)
BAND_RE_BETWEEN = re.compile(r"^between-(?P<low>-?\d+)-(?P<high>-?\d+)f$")
BAND_RE_SINGLE_LOW = re.compile(r"^(?P<v>-?\d+)f-or-below$")
BAND_RE_SINGLE_HIGH = re.compile(r"^(?P<v>-?\d+)f-or-higher$")


def parse_slug(slug: str) -> dict | None:
    m = SLUG_RE.match(slug)
    if not m:
        return None
    g = m.groupdict()
    band = g["band"]
    low_f = high_f = None
    if (b := BAND_RE_BETWEEN.match(band)):
        low_f = float(b.group("low"))
        high_f = float(b.group("high"))
    elif (b := BAND_RE_SINGLE_LOW.match(band)):
        high_f = float(b.group("v"))
    elif (b := BAND_RE_SINGLE_HIGH.match(band)):
        low_f = float(b.group("v"))
    else:
        return None
    return {
        "city": g["city"],
        "low_f": low_f,
        "high_f": high_f,
        "month": g["month"],
        "day": int(g["day"]),
    }


def init_db(db_path: Path) -> sqlite3.Connection:
    conn = sqlite3.connect(db_path)
    conn.executescript(
        """
        CREATE TABLE IF NOT EXISTS markets (
            market_id TEXT PRIMARY KEY,
            slug TEXT,
            question TEXT,
            condition_id TEXT,
            yes_token_id TEXT,
            no_token_id TEXT,
            city TEXT,
            target_date TEXT,
            low_f REAL,
            high_f REAL,
            start_ts INTEGER,
            end_ts INTEGER,
            volume REAL,
            outcome_yes_won INTEGER,
            raw_json TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_markets_target ON markets(city, target_date);

        CREATE TABLE IF NOT EXISTS prices (
            market_id TEXT,
            side TEXT,
            ts INTEGER,
            price REAL,
            PRIMARY KEY (market_id, side, ts)
        );
        CREATE INDEX IF NOT EXISTS idx_prices_market ON prices(market_id);

        CREATE TABLE IF NOT EXISTS forecasts (
            city TEXT,
            target_date TEXT,
            model TEXT,
            value_f REAL,
            ensemble_min_f REAL,
            ensemble_max_f REAL,
            ensemble_std_f REAL,
            ensemble_n INTEGER,
            PRIMARY KEY (city, target_date, model)
        );

        CREATE TABLE IF NOT EXISTS observed (
            city TEXT,
            target_date TEXT,
            observed_f REAL,
            PRIMARY KEY (city, target_date)
        );
        """
    )
    conn.commit()
    return conn


def enumerate_markets(start: str, end: str) -> list[dict]:
    out = []
    seen = set()
    offset = 0
    while True:
        rows = http_get_json(
            GAMMA,
            {
                "limit": 500,
                "offset": offset,
                "closed": "true",
                "active": "false",
                "end_date_min": start,
                "end_date_max": end,
            },
        )
        if not rows:
            break
        any_new = False
        for m in rows:
            mid = m.get("id")
            if mid in seen:
                continue
            seen.add(mid)
            slug = m.get("slug", "")
            parsed = parse_slug(slug)
            if not parsed:
                continue
            # accept any city that maps to coords; track novel cities anyway
            if parsed["city"] not in CITY_COORDS:
                continue
            parsed["city"] = CITY_ALIAS.get(parsed["city"], parsed["city"])
            out.append({"raw": m, "parsed": parsed})
            any_new = True
        offset += 500
        if offset % 5000 == 0:
            LOG.info("enumerated offset=%d total_seen=%d weather=%d", offset, len(seen), len(out))
        # only stop when Gamma returns an empty page
    return out


def normalize_market_row(rec: dict) -> dict | None:
    raw = rec["raw"]
    parsed = rec["parsed"]
    try:
        clob_ids = json.loads(raw.get("clobTokenIds") or "[]")
    except Exception:
        clob_ids = []
    if len(clob_ids) != 2:
        return None
    yes_token, no_token = clob_ids[0], clob_ids[1]
    try:
        outcome_prices = json.loads(raw.get("outcomePrices") or "[]")
    except Exception:
        outcome_prices = []
    if len(outcome_prices) != 2:
        return None
    yes_won = 1 if float(outcome_prices[0]) > 0.5 else 0

    end_iso = raw.get("endDateIso")
    target_date = end_iso  # market end date IS target date for these
    start_iso = raw.get("startDateIso")
    if not target_date or not start_iso:
        return None

    start_dt = datetime.fromisoformat(start_iso + "T00:00:00+00:00")
    end_dt = datetime.fromisoformat(target_date + "T23:59:59+00:00")

    return {
        "market_id": str(raw.get("id")),
        "slug": raw.get("slug", ""),
        "question": raw.get("question", ""),
        "condition_id": raw.get("conditionId", ""),
        "yes_token_id": str(yes_token),
        "no_token_id": str(no_token),
        "city": parsed["city"],
        "target_date": target_date,
        "low_f": parsed["low_f"],
        "high_f": parsed["high_f"],
        "start_ts": int(start_dt.timestamp()),
        "end_ts": int(end_dt.timestamp()),
        "volume": float(raw.get("volumeNum") or 0),
        "outcome_yes_won": yes_won,
        "raw_json": json.dumps({k: raw.get(k) for k in (
            "id", "slug", "question", "endDateIso", "startDateIso", "volumeNum", "outcomePrices"
        )}),
    }


def fetch_price_history(token_id: str, start_ts: int, end_ts: int) -> list[tuple[int, float]]:
    data = http_get_json(
        CLOB_HISTORY,
        {"market": token_id, "startTs": start_ts, "endTs": end_ts, "fidelity": 1},
    )
    h = data.get("history", [])
    return [(int(p["t"]), float(p["p"])) for p in h]


def fetch_open_meteo(city: str, target_iso: str) -> dict:
    coord = CITY_COORDS[city]
    out = {}
    # deterministic models (verified to support historical-forecast-api)
    for model in ("gfs_seamless", "ecmwf_ifs025", "icon_seamless"):
        try:
            d = http_get_json(
                OM_HISTFORECAST,
                {
                    "latitude": coord["lat"],
                    "longitude": coord["lon"],
                    "start_date": target_iso,
                    "end_date": target_iso,
                    "daily": "temperature_2m_max",
                    "models": model,
                    "temperature_unit": "fahrenheit",
                    "timezone": coord["tz"],
                },
            )
            vals = d.get("daily", {}).get("temperature_2m_max") or [None]
            out[model] = vals[0]
        except Exception as e:
            LOG.warning("om model %s %s %s: %s", city, target_iso, model, e)
            out[model] = None
    # ensemble API only supports rolling ~4 month window per OM, so use the
    # deterministic-model spread as a proxy for "ensemble disagreement"
    vals = [out[m] for m in ("gfs_seamless", "ecmwf_ifs025", "icon_seamless") if out.get(m) is not None]
    if vals:
        out["_ens_min"] = min(vals)
        out["_ens_max"] = max(vals)
        mean = sum(vals) / len(vals)
        out["_ens_std"] = (sum((v - mean) ** 2 for v in vals) / len(vals)) ** 0.5
        out["_ens_n"] = len(vals)
    else:
        out["_ens_min"] = out["_ens_max"] = out["_ens_std"] = None
        out["_ens_n"] = 0
    # observed
    try:
        d = http_get_json(
            OM_ARCHIVE,
            {
                "latitude": coord["lat"],
                "longitude": coord["lon"],
                "start_date": target_iso,
                "end_date": target_iso,
                "daily": "temperature_2m_max",
                "temperature_unit": "fahrenheit",
                "timezone": coord["tz"],
            },
        )
        vals = d.get("daily", {}).get("temperature_2m_max") or [None]
        out["_observed"] = vals[0]
    except Exception as e:
        LOG.warning("archive %s %s: %s", city, target_iso, e)
        out["_observed"] = None
    return out


def main():
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
    p = argparse.ArgumentParser()
    p.add_argument("--db", type=Path, default=Path("research/weather/backfill.db"))
    p.add_argument("--start", default="2025-04-01")
    p.add_argument("--end", default="2026-04-07")
    p.add_argument("--max-markets", type=int, default=0, help="0 = no cap")
    p.add_argument("--skip-prices", action="store_true")
    p.add_argument("--skip-forecasts", action="store_true")
    p.add_argument("--workers", type=int, default=8)
    args = p.parse_args()

    args.db.parent.mkdir(parents=True, exist_ok=True)
    conn = init_db(args.db)

    LOG.info("enumerating markets %s..%s", args.start, args.end)
    recs = enumerate_markets(args.start, args.end)
    LOG.info("raw weather candidates: %d", len(recs))

    rows = []
    for rec in recs:
        norm = normalize_market_row(rec)
        if norm:
            rows.append(norm)
    if args.max_markets:
        rows = rows[: args.max_markets]
    LOG.info("normalized markets: %d", len(rows))

    conn.executemany(
        """
        INSERT OR REPLACE INTO markets (
            market_id, slug, question, condition_id, yes_token_id, no_token_id,
            city, target_date, low_f, high_f, start_ts, end_ts, volume,
            outcome_yes_won, raw_json
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        """,
        [
            (
                r["market_id"], r["slug"], r["question"], r["condition_id"],
                r["yes_token_id"], r["no_token_id"], r["city"], r["target_date"],
                r["low_f"], r["high_f"], r["start_ts"], r["end_ts"], r["volume"],
                r["outcome_yes_won"], r["raw_json"],
            )
            for r in rows
        ],
    )
    conn.commit()

    if not args.skip_prices:
        LOG.info("fetching CLOB price history for %d markets...", len(rows))
        # check what's already done
        done = {row[0] for row in conn.execute("SELECT DISTINCT market_id FROM prices")}
        todo = [r for r in rows if r["market_id"] not in done]
        LOG.info("todo prices: %d (skipped %d)", len(todo), len(rows) - len(todo))

        def pull(r):
            try:
                yes_h = fetch_price_history(r["yes_token_id"], r["start_ts"], r["end_ts"])
                no_h = fetch_price_history(r["no_token_id"], r["start_ts"], r["end_ts"])
                return r["market_id"], yes_h, no_h, None
            except Exception as e:
                return r["market_id"], None, None, str(e)

        with ThreadPoolExecutor(max_workers=args.workers) as ex:
            futures = [ex.submit(pull, r) for r in todo]
            done_n = 0
            for fut in as_completed(futures):
                mid, yh, nh, err = fut.result()
                done_n += 1
                if err:
                    LOG.warning("price fail %s: %s", mid, err)
                    continue
                rows_to_insert = (
                    [(mid, "yes", t, p) for (t, p) in yh]
                    + [(mid, "no", t, p) for (t, p) in nh]
                )
                conn.executemany(
                    "INSERT OR REPLACE INTO prices (market_id, side, ts, price) VALUES (?,?,?,?)",
                    rows_to_insert,
                )
                if done_n % 50 == 0:
                    conn.commit()
                    LOG.info("prices progress: %d/%d", done_n, len(todo))
            conn.commit()

    if not args.skip_forecasts:
        # one fetch per (city, target_date), not per market
        cd_pairs = sorted({(r["city"], r["target_date"]) for r in rows})
        done = {(c, d) for (c, d) in conn.execute("SELECT city, target_date FROM observed")}
        todo = [(c, d) for (c, d) in cd_pairs if (c, d) not in done]
        LOG.info("forecasts todo: %d city-days (skipped %d)", len(todo), len(cd_pairs) - len(todo))

        def pull(cd):
            c, d = cd
            try:
                return c, d, fetch_open_meteo(c, d), None
            except Exception as e:
                return c, d, None, str(e)

        with ThreadPoolExecutor(max_workers=args.workers) as ex:
            futures = [ex.submit(pull, cd) for cd in todo]
            done_n = 0
            for fut in as_completed(futures):
                c, d, out, err = fut.result()
                done_n += 1
                if err or out is None:
                    LOG.warning("om fail %s %s: %s", c, d, err)
                    continue
                conn.executemany(
                    "INSERT OR REPLACE INTO forecasts (city, target_date, model, value_f, ensemble_min_f, ensemble_max_f, ensemble_std_f, ensemble_n) VALUES (?,?,?,?,?,?,?,?)",
                    [
                        (c, d, model, out.get(model), out.get("_ens_min"), out.get("_ens_max"), out.get("_ens_std"), out.get("_ens_n"))
                        for model in ("gfs_seamless", "ecmwf_ifs04", "icon_seamless")
                    ],
                )
                if out.get("_observed") is not None:
                    conn.execute(
                        "INSERT OR REPLACE INTO observed (city, target_date, observed_f) VALUES (?,?,?)",
                        (c, d, out["_observed"]),
                    )
                if done_n % 25 == 0:
                    conn.commit()
                    LOG.info("forecasts progress: %d/%d", done_n, len(todo))
            conn.commit()

    # final stats
    n_markets = conn.execute("SELECT COUNT(*) FROM markets").fetchone()[0]
    n_prices = conn.execute("SELECT COUNT(*) FROM prices").fetchone()[0]
    n_fc = conn.execute("SELECT COUNT(*) FROM forecasts").fetchone()[0]
    n_obs = conn.execute("SELECT COUNT(*) FROM observed").fetchone()[0]
    LOG.info("DONE markets=%d prices=%d forecasts=%d observed=%d", n_markets, n_prices, n_fc, n_obs)


if __name__ == "__main__":
    sys.exit(main())
