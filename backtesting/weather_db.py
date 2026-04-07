"""Dedicated SQLite store for weather strategy research."""

from __future__ import annotations

import json
from pathlib import Path

from models.weather import ForecastSnapshot, WeatherMarket
from shared.constants import WEATHER_DB_PATH
from shared.db import get_connection


def init_weather_db(db_path: Path = WEATHER_DB_PATH):
    conn = get_connection(db_path)
    conn.execute(
        """
        CREATE TABLE IF NOT EXISTS weather_markets (
            market_id TEXT PRIMARY KEY,
            slug TEXT NOT NULL,
            question TEXT NOT NULL,
            city_key TEXT NOT NULL,
            target_date TEXT NOT NULL,
            lower_temp_c REAL,
            upper_temp_c REAL,
            lower_inclusive INTEGER NOT NULL,
            upper_inclusive INTEGER NOT NULL,
            unit TEXT NOT NULL,
            yes_price REAL NOT NULL,
            no_price REAL NOT NULL,
            volume REAL NOT NULL DEFAULT 0,
            liquidity REAL NOT NULL DEFAULT 0,
            active INTEGER NOT NULL DEFAULT 1,
            raw_json TEXT NOT NULL DEFAULT '{}'
        )
        """
    )
    conn.execute(
        """
        CREATE TABLE IF NOT EXISTS weather_forecasts (
            market_id TEXT NOT NULL,
            as_of TEXT NOT NULL,
            source TEXT NOT NULL,
            model TEXT NOT NULL,
            target_date TEXT NOT NULL,
            forecast_temp_c REAL NOT NULL,
            probability_yes REAL NOT NULL,
            confidence REAL NOT NULL,
            raw_json TEXT NOT NULL DEFAULT '{}',
            PRIMARY KEY (market_id, as_of, source, model)
        )
        """
    )
    conn.execute(
        """
        CREATE TABLE IF NOT EXISTS weather_actuals (
            market_id TEXT PRIMARY KEY,
            observed_date TEXT NOT NULL,
            observed_temp_c REAL NOT NULL,
            raw_json TEXT NOT NULL DEFAULT '{}'
        )
        """
    )
    conn.commit()
    return conn


def upsert_weather_markets(markets: list[WeatherMarket], db_path: Path = WEATHER_DB_PATH) -> int:
    conn = init_weather_db(db_path)
    rows = [
        (
            market.market_id,
            market.slug,
            market.question,
            market.city_key,
            market.target_date.isoformat(),
            market.band.low_c,
            market.band.high_c,
            int(market.band.low_inclusive),
            int(market.band.high_inclusive),
            market.unit,
            market.yes_price,
            market.no_price,
            market.volume,
            market.liquidity,
            int(market.active),
            json.dumps(market.raw, separators=(",", ":")),
        )
        for market in markets
    ]
    conn.executemany(
        """
        INSERT INTO weather_markets (
            market_id, slug, question, city_key, target_date,
            lower_temp_c, upper_temp_c, lower_inclusive, upper_inclusive,
            unit, yes_price, no_price, volume, liquidity, active, raw_json
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(market_id) DO UPDATE SET
            slug=excluded.slug,
            question=excluded.question,
            city_key=excluded.city_key,
            target_date=excluded.target_date,
            lower_temp_c=excluded.lower_temp_c,
            upper_temp_c=excluded.upper_temp_c,
            lower_inclusive=excluded.lower_inclusive,
            upper_inclusive=excluded.upper_inclusive,
            unit=excluded.unit,
            yes_price=excluded.yes_price,
            no_price=excluded.no_price,
            volume=excluded.volume,
            liquidity=excluded.liquidity,
            active=excluded.active,
            raw_json=excluded.raw_json
        """,
        rows,
    )
    conn.commit()
    conn.close()
    return len(rows)


def insert_forecast_snapshots(snapshots: list[ForecastSnapshot], db_path: Path = WEATHER_DB_PATH) -> int:
    conn = init_weather_db(db_path)
    rows = [
        (
            snap.market_id,
            snap.as_of,
            snap.source,
            snap.model,
            snap.target_date.isoformat(),
            snap.forecast_temp_c,
            snap.probability_yes,
            snap.confidence,
            json.dumps(snap.raw, separators=(",", ":")),
        )
        for snap in snapshots
    ]
    conn.executemany(
        """
        INSERT OR REPLACE INTO weather_forecasts (
            market_id, as_of, source, model, target_date,
            forecast_temp_c, probability_yes, confidence, raw_json
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
        """,
        rows,
    )
    conn.commit()
    conn.close()
    return len(rows)


def upsert_actual(
    market_id: str,
    observed_date: str,
    observed_temp_c: float,
    raw: dict | None = None,
    db_path: Path = WEATHER_DB_PATH,
) -> None:
    conn = init_weather_db(db_path)
    conn.execute(
        """
        INSERT OR REPLACE INTO weather_actuals (
            market_id, observed_date, observed_temp_c, raw_json
        ) VALUES (?, ?, ?, ?)
        """,
        (
            market_id,
            observed_date,
            observed_temp_c,
            json.dumps(raw or {}, separators=(",", ":")),
        ),
    )
    conn.commit()
    conn.close()


def load_research_rows(db_path: Path = WEATHER_DB_PATH) -> list[dict]:
    conn = init_weather_db(db_path)
    rows = conn.execute(
        """
        SELECT
            m.market_id,
            m.slug,
            m.question,
            m.city_key,
            m.target_date,
            m.lower_temp_c,
            m.upper_temp_c,
            m.lower_inclusive,
            m.upper_inclusive,
            m.unit,
            m.yes_price,
            m.no_price,
            m.volume,
            m.liquidity,
            f.as_of,
            f.source,
            f.model,
            f.forecast_temp_c,
            f.probability_yes,
            f.confidence,
            a.observed_temp_c
        FROM weather_markets m
        JOIN weather_forecasts f ON f.market_id = m.market_id
        LEFT JOIN weather_actuals a ON a.market_id = m.market_id
        ORDER BY m.target_date, f.as_of, m.market_id
        """
    ).fetchall()
    result = [dict(row) for row in rows]
    conn.close()
    return result
