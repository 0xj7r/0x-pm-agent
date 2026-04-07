"""Structured experiment and candidate models for autoresearch."""
from __future__ import annotations

from dataclasses import asdict, dataclass, field
from typing import Any


@dataclass(frozen=True)
class SearchCandidate:
    coin: str
    strategy_name: str
    params: dict[str, Any]
    train: dict[str, Any]
    test: dict[str, Any]

    def to_dict(self) -> dict[str, Any]:
        return {
            "coin": self.coin,
            "name": self.strategy_name,
            "params": self.params,
            "train_trades": self.train["trades"],
            "train_wins": self.train["wins"],
            "train_wr": self.train["win_rate"],
            "train_pnl": self.train["pnl"],
            "train_sharpe": self.train["sharpe"],
            "train_max_drawdown": self.train["max_drawdown"],
            "test_trades": self.test["trades"],
            "test_wins": self.test["wins"],
            "test_wr": self.test["win_rate"],
            "test_pnl": self.test["pnl"],
            "test_pnl_per_trade": self.test["pnl_per_trade"],
            "test_sharpe": self.test["sharpe"],
            "test_max_drawdown": self.test["max_drawdown"],
        }


@dataclass(frozen=True)
class DatasetFingerprint:
    coin: str
    source_path: str
    size_bytes: int
    modified_ns: int
    markets: int

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True)
class ExperimentRecord:
    coin: str
    created_at: str
    dataset: DatasetFingerprint
    current_best: dict[str, Any]
    candidate: dict[str, Any]
    validation: dict[str, Any]
    accepted: bool
    reason: str
    search_time_s: float
    evaluator_version: str = "v2"
    search_policy: str = "canonical_registry_grid"

    def to_dict(self) -> dict[str, Any]:
        return {
            "coin": self.coin,
            "created_at": self.created_at,
            "dataset": self.dataset.to_dict(),
            "current_best": self.current_best,
            "candidate": self.candidate,
            "validation": self.validation,
            "accepted": self.accepted,
            "reason": self.reason,
            "search_time_s": self.search_time_s,
            "evaluator_version": self.evaluator_version,
            "search_policy": self.search_policy,
        }


@dataclass(frozen=True)
class CandidateArtifact:
    coin: str
    created_at: str
    current_best: dict[str, Any]
    proposed: dict[str, Any]
    top_5: list[dict[str, Any]]
    validation: dict[str, Any]
    total_profitable: int
    search_time_s: float
    profile_patch: dict[str, Any] = field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)
