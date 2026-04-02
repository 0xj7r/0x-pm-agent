"""Tunable strategy parameters loaded from JSON config file."""
from __future__ import annotations

import json
import logging
from dataclasses import dataclass, field
from pathlib import Path

logger = logging.getLogger(__name__)


@dataclass
class ThresholdConfig:
    move_threshold: float = 0.08
    max_entry: float = 0.55


@dataclass
class RiskConfig:
    max_position_usd: float = 50.0
    max_position_pct: float = 0.10
    daily_loss_limit_pct: float = 0.25
    kill_balance_usd: float = 10.0
    max_concurrent_positions: int = 20
    loss_cooldown_trades: int = 5
    loss_cooldown_seconds: int = 300
    kelly_multiplier: float = 0.25


@dataclass
class PaperConfig:
    enabled: bool = True
    starting_balance: float = 100.0


@dataclass
class StrategyConfig:
    version: int = 1
    coins: dict[str, ThresholdConfig] = field(default_factory=lambda: {
        "btc": ThresholdConfig(0.08, 0.55),
        "eth": ThresholdConfig(0.15, 0.55),
        "sol": ThresholdConfig(0.08, 0.55),
    })
    risk: RiskConfig = field(default_factory=RiskConfig)
    paper: PaperConfig = field(default_factory=PaperConfig)


def load_strategy_config(path: str) -> StrategyConfig:
    """Load strategy config from JSON file. Returns defaults if file missing."""
    p = Path(path)
    if not p.exists():
        logger.warning(f"Config file not found at {path}, using defaults")
        return StrategyConfig()

    with open(p) as f:
        data = json.load(f)

    def _filter_fields(cls: type, d: dict) -> dict:
        return {k: v for k, v in d.items() if k in cls.__dataclass_fields__}

    cfg = StrategyConfig(version=data.get("version", 1))

    if "coins" in data:
        cfg.coins = {
            coin: ThresholdConfig(**_filter_fields(ThresholdConfig, params))
            for coin, params in data["coins"].items()
        }

    if "risk" in data:
        cfg.risk = RiskConfig(**_filter_fields(RiskConfig, data["risk"]))
    if "paper" in data:
        cfg.paper = PaperConfig(**_filter_fields(PaperConfig, data["paper"]))

    return cfg
