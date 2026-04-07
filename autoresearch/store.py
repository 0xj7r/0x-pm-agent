"""Persistence helpers for autoresearch experiments and candidates."""
from __future__ import annotations

import json
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from autoresearch.models import CandidateArtifact, DatasetFingerprint, ExperimentRecord


BASE_DIR = Path(__file__).resolve().parent.parent
BACKTESTING_DIR = BASE_DIR / "backtesting"
RESULTS_PATH = BACKTESTING_DIR / "strategy_results.json"
EXPERIMENTS_PATH = BASE_DIR / "autoresearch" / "experiments.jsonl"
CANDIDATES_DIR = BASE_DIR / "autoresearch" / "candidates"


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat()


def load_current_best() -> dict[str, Any]:
    if not RESULTS_PATH.exists():
        return {}
    return json.loads(RESULTS_PATH.read_text())


def fingerprint_dataset(coin: str, source_path: Path, markets: int) -> DatasetFingerprint:
    stat = source_path.stat()
    return DatasetFingerprint(
        coin=coin,
        source_path=str(source_path),
        size_bytes=stat.st_size,
        modified_ns=stat.st_mtime_ns,
        markets=markets,
    )


def append_experiment(record: ExperimentRecord) -> None:
    EXPERIMENTS_PATH.parent.mkdir(parents=True, exist_ok=True)
    with EXPERIMENTS_PATH.open("a") as handle:
        handle.write(json.dumps(record.to_dict(), separators=(",", ":")) + "\n")


def save_candidate(artifact: CandidateArtifact) -> Path:
    CANDIDATES_DIR.mkdir(parents=True, exist_ok=True)
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    path = CANDIDATES_DIR / f"{stamp}-{artifact.coin}.json"
    path.write_text(json.dumps(artifact.to_dict(), indent=2))
    return path
