"""Deterministic search layer for evaluating many strategy candidates."""
from __future__ import annotations

from pathlib import Path
from typing import Any

from autoresearch.metrics import is_result_significant, sort_results
from autoresearch.models import SearchCandidate
from backtesting.eval.feature_store import (
    append_new_markets,
    build_feature_store,
    load_feature_store,
    store_dir,
)
from backtesting.eval.validate_strategy_readiness import simulate_manifest
from shared.constants import db_path as default_db_path
from strategies.registry import build_strategy_grid


def ensure_feature_store(coin: str, db_dir: Path | None = None) -> tuple[list, dict, Path]:
    db = Path(db_dir) / f"{coin}.db" if db_dir else default_db_path(coin)
    if not db.exists():
        raise FileNotFoundError(f"No DB found for {coin}: {db}")

    try:
        manifest_path = store_dir(coin) / "manifest.json"
        if manifest_path.exists() and db.stat().st_mtime > manifest_path.stat().st_mtime:
            append_new_markets(db, coin)
        manifest, features = load_feature_store(coin)
        return manifest, features, db
    except FileNotFoundError:
        build_feature_store(db, coin)
        manifest, features = load_feature_store(coin)
        return manifest, features, db


def split_manifest(manifest: list, train_ratio: float = 0.7) -> tuple[list, list]:
    split = int(len(manifest) * train_ratio)
    return manifest[:split], manifest[split:]


def evaluate_search_space(
    coin: str,
    manifest: list,
    features: dict,
    strategy_names: list[str] | None = None,
) -> tuple[list[dict[str, Any]], list[dict[str, Any]]]:
    train, test = split_manifest(manifest)
    eligible_results: list[dict[str, Any]] = []
    raw_profitable_results: list[dict[str, Any]] = []

    for strategy_name, params, _ in build_strategy_grid(strategy_names):
        train_stats = simulate_manifest(train, features, strategy_name, params)
        test_stats = simulate_manifest(test, features, strategy_name, params)
        candidate = SearchCandidate(
            coin=coin,
            strategy_name=strategy_name,
            params=params,
            train=train_stats,
            test=test_stats,
        )
        row = candidate.to_dict()
        row["rejection_reasons"] = _rejection_reasons(row)
        if row.get("train_pnl", 0) > 0 and row.get("test_pnl", 0) > 0:
            raw_profitable_results.append(row)
        if is_result_significant(row):
            eligible_results.append(row)

    return sort_results(eligible_results), sort_results(raw_profitable_results)


def _rejection_reasons(result: dict[str, Any]) -> list[str]:
    reasons: list[str] = []
    if result.get("train_trades", 0) < 100:
        reasons.append("insufficient_train_trades")
    if result.get("test_trades", 0) < 30:
        reasons.append("insufficient_test_trades")
    if result.get("train_pnl", 0) <= 0:
        reasons.append("non_positive_train_pnl")
    if result.get("test_pnl", 0) <= 0:
        reasons.append("non_positive_test_pnl")
    if result.get("test_sharpe", 0) < 0.5:
        reasons.append("low_test_sharpe")
    return reasons


def shortlist_candidates(
    coin: str,
    db_dir: Path | None = None,
    limit: int = 5,
    strategy_names: list[str] | None = None,
) -> tuple[list[dict[str, Any]], int, int, list[dict[str, Any]], list, dict, Path]:
    manifest, features, db = ensure_feature_store(coin, db_dir=db_dir)
    eligible, raw_profitable = evaluate_search_space(
        coin,
        manifest,
        features,
        strategy_names=strategy_names,
    )
    return (
        eligible[:limit],
        len(eligible),
        len(raw_profitable),
        raw_profitable[:limit],
        manifest,
        features,
        db,
    )
