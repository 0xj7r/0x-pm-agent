"""Weather paper bot: run scout daily, log trades, resolve past markets.

Paper-mode companion to the live BTC bot. Does NOT submit orders.

Daily flow:
  1. Fetch active daily-temperature events
  2. For each new event we haven't decided yet, run the scout, record
     any trade candidates with edge >= threshold to SQLite
  3. Check for any paper trades whose target_date is past, fetch the
     observed temperature from Open-Meteo archive, mark resolved
  4. Print summary: open positions, pending resolutions, cumulative PnL

Run once:  python3 scripts/weather_paper_bot.py
Run loop:  python3 scripts/weather_paper_bot.py --loop 3600  (every hr)

Schema `weather_paper_trades`:
  id, ts, market_slug, event_slug, city, target_date, low_f, high_f,
  market_price, model_prob, edge, consensus, kelly_fraction, size_usd,
  resolved (0/1), observed_temp_f, won (0/1), pnl_usd
"""
from __future__ import annotations

import argparse
import sqlite3
import sys
import time
import urllib.parse
import urllib.request
import json
from datetime import date, datetime, timedelta
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent.parent))

from scripts.weather_live_scout import (
    CITY_COORDS,
    Candidate,
    evaluate_event,
    fetch_events,
    http_get,
    parse_event_slug,
)


DB_PATH = Path("data/weather_paper_trades.db")
OM_ARCHIVE = "https://archive-api.open-meteo.com/v1/archive"
BANKROLL_DEFAULT = 100.0
KELLY_MULT = 0.25
MAX_PCT = 0.05
MIN_EDGE = 0.05
MIN_CONSENSUS = 0.0  # effectively off; we're gathering data

# --- Realistic paper-trading frictions (learned from BTC live today) ---
# Taker slippage: live orders cross the spread at 1c above the last price
# we observed. Match BTC bot's live_entry_slippage_usd.
PAPER_ENTRY_SLIPPAGE_USD = 0.01
# Polymarket taker fee on weather markets; matches weather_pnl.py research
# baseline. Dynamic in reality; we assume 2% conservative.
PAPER_FEE_RATE = 0.02
# Don't place paper bets in the last N hours before target_date — too
# little time for price discovery + mispricing likely already arbed.
MIN_HOURS_TO_TARGET = 12.0


def ensure_db(db_path: Path) -> sqlite3.Connection:
    db_path.parent.mkdir(parents=True, exist_ok=True)
    conn = sqlite3.connect(str(db_path))
    conn.execute("""
        CREATE TABLE IF NOT EXISTS weather_paper_trades (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            ts TEXT NOT NULL,
            market_slug TEXT UNIQUE NOT NULL,
            event_slug TEXT NOT NULL,
            city TEXT NOT NULL,
            target_date TEXT NOT NULL,
            low_f REAL, high_f REAL,
            market_price REAL NOT NULL,
            model_prob REAL NOT NULL,
            edge REAL NOT NULL,
            consensus REAL NOT NULL,
            kelly_fraction REAL NOT NULL,
            size_usd REAL NOT NULL,
            resolved INTEGER DEFAULT 0,
            observed_temp_f REAL,
            won INTEGER,
            pnl_usd REAL
        )
    """)
    conn.execute("CREATE INDEX IF NOT EXISTS idx_resolved ON weather_paper_trades(resolved)")
    conn.execute("CREATE INDEX IF NOT EXISTS idx_target_date ON weather_paper_trades(target_date)")
    conn.commit()
    return conn


def record_candidate(conn: sqlite3.Connection, c: Candidate, size_usd: float) -> bool:
    """Insert candidate; return True if new, False if already recorded."""
    try:
        conn.execute("""
            INSERT INTO weather_paper_trades
            (ts, market_slug, event_slug, city, target_date, low_f, high_f,
             market_price, model_prob, edge, consensus, kelly_fraction, size_usd)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        """, (
            datetime.utcnow().isoformat(timespec="seconds"),
            c.market_slug, c.event_slug, c.city, c.target_date.isoformat(),
            c.low_f, c.high_f, c.market_price, c.model_prob, c.edge,
            c.consensus, c.kelly_fraction, size_usd,
        ))
        conn.commit()
        return True
    except sqlite3.IntegrityError:
        return False


def fetch_observed_temp_f(city: str, target_date: date) -> float | None:
    """Daily max temperature (F) at the city's resolution station.

    Polymarket's daily-temperature markets resolve against Weather
    Underground, which pulls from NOAA METAR/ISD station data. We match
    that methodology by using meteostat (wraps NOAA ISD/GSOD) at the
    station's lat/lon. Falls back to Open-Meteo archive if meteostat
    lookup fails (e.g., sparse international coverage).
    """
    coord = CITY_COORDS.get(city)
    if not coord:
        return None

    # Primary source: NOAA station data via meteostat (matches Wunderground).
    try:
        from datetime import datetime as _dt
        from meteostat import Point, Daily

        point = Point(coord["lat"], coord["lon"])
        start = _dt(target_date.year, target_date.month, target_date.day)
        end = _dt(target_date.year, target_date.month, target_date.day, 23, 59, 59)
        df = Daily(point, start, end).fetch()
        if len(df) > 0:
            tmax_c = df["tmax"].iloc[0]
            if tmax_c is not None and not (isinstance(tmax_c, float) and tmax_c != tmax_c):  # NaN check
                return float(tmax_c) * 9.0 / 5.0 + 32.0
    except Exception as exc:
        print(f"  meteostat fetch failed for {city} {target_date}: {exc}", file=sys.stderr)

    # Fallback: Open-Meteo archive (gridded reanalysis; close but not identical).
    try:
        data = http_get(OM_ARCHIVE, {
            "latitude": coord["lat"],
            "longitude": coord["lon"],
            "daily": "temperature_2m_max",
            "start_date": target_date.isoformat(),
            "end_date": target_date.isoformat(),
            "timezone": coord["tz"],
            "temperature_unit": "fahrenheit",
        })
    except Exception as exc:
        print(f"  archive fallback also failed for {city} {target_date}: {exc}", file=sys.stderr)
        return None
    daily = data.get("daily", {}) or {}
    times = daily.get("time", []) or []
    temps = daily.get("temperature_2m_max", []) or []
    if not times or times[0] != target_date.isoformat():
        return None
    if not temps or temps[0] is None:
        return None
    return float(temps[0])


def band_contains(low_f: float | None, high_f: float | None, observed_f: float) -> bool:
    """+/- 0.5F boundary matching Polymarket's integer-F resolution rule."""
    if low_f is not None and observed_f < low_f - 0.5:
        return False
    if high_f is not None and observed_f > high_f + 0.5:
        return False
    return True


def effective_entry_price(market_price: float) -> float:
    """Taker actually pays market_price + slippage (crossing the spread)."""
    return min(0.99, market_price + PAPER_ENTRY_SLIPPAGE_USD)


def resolve_past_trades(conn: sqlite3.Connection) -> tuple[int, float]:
    """Find paper trades with target_date < today and resolve them. Returns
    (count_resolved_this_run, pnl_delta_this_run).

    P&L accounting (realistic paper):
      effective_entry = market_price + slippage
      shares          = size_usd / effective_entry
      gross_fee       = size_usd * fee_rate
      win  ->  (shares * 1.0 - size_usd - gross_fee)
      loss ->  (- size_usd - gross_fee)
    """
    today = date.today()
    cutoff = today.isoformat()
    rows = conn.execute("""
        SELECT id, city, target_date, low_f, high_f, market_price, size_usd
        FROM weather_paper_trades
        WHERE resolved = 0 AND target_date < ?
    """, (cutoff,)).fetchall()

    n_resolved = 0
    pnl_delta = 0.0
    for row_id, city, td_str, low_f, high_f, market_price, size_usd in rows:
        td = datetime.fromisoformat(td_str).date()
        observed = fetch_observed_temp_f(city, td)
        if observed is None:
            # Can't resolve yet; archive may lag by 1-2 days
            continue
        won = band_contains(low_f, high_f, observed)
        entry_px = effective_entry_price(market_price)
        shares = size_usd / entry_px if entry_px > 0 else 0.0
        fee = size_usd * PAPER_FEE_RATE
        if won:
            pnl = shares * 1.0 - size_usd - fee
        else:
            pnl = -size_usd - fee
        conn.execute("""
            UPDATE weather_paper_trades
            SET resolved = 1, observed_temp_f = ?, won = ?, pnl_usd = ?
            WHERE id = ?
        """, (observed, 1 if won else 0, pnl, row_id))
        n_resolved += 1
        pnl_delta += pnl
    conn.commit()
    return n_resolved, pnl_delta


def scout_and_record(conn: sqlite3.Connection, bankroll: float) -> int:
    events = fetch_events()
    n_recorded = 0
    now = datetime.utcnow()
    for ev in events:
        parsed = parse_event_slug(ev.get("slug", ""))
        if not parsed:
            continue
        city, target = parsed
        if city not in CITY_COORDS:
            continue
        # Skip if < MIN_HOURS_TO_TARGET before target_date. Trading too
        # close to resolution leaves no time for price discovery and any
        # mispricing is probably already arbed.
        target_dt = datetime(target.year, target.month, target.day)
        hours_to_target = (target_dt - now).total_seconds() / 3600.0
        if hours_to_target < MIN_HOURS_TO_TARGET:
            continue
        try:
            cands = evaluate_event(ev, MIN_EDGE, MIN_CONSENSUS)
        except Exception as exc:
            print(f"  error evaluating {ev.get('slug','')}: {exc}", file=sys.stderr)
            continue
        for c in cands:
            # After slippage, the bet's effective entry is higher and the
            # real edge is lower. Recompute size on the post-slip edge so
            # we're not overbetting.
            effective_price = effective_entry_price(c.market_price)
            real_edge = c.model_prob - effective_price
            if real_edge < MIN_EDGE:
                continue
            # Kelly on the effective price, not the market price.
            b = (1.0 - effective_price) / effective_price
            q = 1.0 - c.model_prob
            kelly_fraction = max(0.0, (b * c.model_prob - q) / b)
            size = bankroll * min(kelly_fraction * KELLY_MULT, MAX_PCT)
            if size < 0.5:
                continue
            if record_candidate(conn, c, size):
                n_recorded += 1
    return n_recorded


def print_summary(conn: sqlite3.Connection) -> None:
    row = conn.execute("""
        SELECT
          SUM(CASE WHEN resolved = 1 THEN 1 ELSE 0 END) AS resolved,
          SUM(CASE WHEN resolved = 0 THEN 1 ELSE 0 END) AS open,
          SUM(CASE WHEN resolved = 1 AND won = 1 THEN 1 ELSE 0 END) AS wins,
          SUM(CASE WHEN resolved = 1 AND won = 0 THEN 1 ELSE 0 END) AS losses,
          ROUND(SUM(CASE WHEN resolved = 1 THEN pnl_usd ELSE 0 END), 2) AS realized_pnl,
          ROUND(SUM(CASE WHEN resolved = 0 THEN size_usd ELSE 0 END), 2) AS capital_at_risk,
          ROUND(SUM(size_usd), 2) AS total_deployed
        FROM weather_paper_trades
    """).fetchone()
    resolved, open_, wins, losses, realized, at_risk, deployed = [v or 0 for v in row]
    hit_rate = (wins / max(1, resolved)) if resolved else 0.0
    print("--- weather paper summary ---")
    print(f"  bets recorded:          {resolved + open_}")
    print(f"  resolved:               {resolved} ({wins}W / {losses}L = {hit_rate:.1%})")
    print(f"  open (awaiting result): {open_}")
    print(f"  realized PnL:           ${realized:.2f}")
    print(f"  capital at risk now:    ${at_risk:.2f}")
    print(f"  total capital deployed: ${deployed:.2f}")

    # Recent decisions
    recent = conn.execute("""
        SELECT ts, city, target_date, low_f, high_f, market_price, model_prob, edge, size_usd, resolved, won
        FROM weather_paper_trades ORDER BY id DESC LIMIT 10
    """).fetchall()
    if recent:
        print("\n  recent decisions:")
        for ts, city, td, lo, hi, px, mp, ed, sz, res, won in recent:
            band = (f"{lo:.0f}-{hi:.0f}F" if lo and hi else
                    (f"<={hi:.0f}F" if hi else f">={lo:.0f}F"))
            outcome = "-" if res == 0 else ("WIN" if won else "LOSS")
            print(f"    {ts[11:16]} {city:<14s} {td} {band:<10s} px={px:.3f} p={mp:.2f} edge={ed:+.2f} ${sz:.2f} {outcome}")


def tick(conn: sqlite3.Connection, bankroll: float) -> None:
    recorded = scout_and_record(conn, bankroll)
    resolved, pnl = resolve_past_trades(conn)
    print(f"[tick] recorded {recorded} new bets, resolved {resolved} markets (pnl ${pnl:+.2f})")
    print_summary(conn)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--db", default=str(DB_PATH))
    ap.add_argument("--bankroll", type=float, default=BANKROLL_DEFAULT)
    ap.add_argument("--loop", type=int, default=0,
                    help="Run tick every N seconds (0 = run once then exit)")
    args = ap.parse_args()

    conn = ensure_db(Path(args.db))
    if args.loop <= 0:
        tick(conn, args.bankroll)
        conn.close()
        return 0

    while True:
        try:
            tick(conn, args.bankroll)
        except Exception as exc:
            print(f"[tick] error: {exc}", file=sys.stderr)
        time.sleep(args.loop)


if __name__ == "__main__":
    raise SystemExit(main())
