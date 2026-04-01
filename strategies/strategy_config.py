"""Tunable strategy parameters loaded from JSON config file."""
from __future__ import annotations

import json
import logging
from dataclasses import dataclass, field
from pathlib import Path

logger = logging.getLogger(__name__)


@dataclass
class SignalConfig:
    w1_order_flow: float = 0.3
    w2_microprice: float = 0.2
    w3_price_delta: float = 0.4
    w4_acceleration: float = 0.1
    confidence_threshold: float = 0.85
    prior: float = 0.5


@dataclass
class ExecutionConfig:
    max_entry_price: float = 0.05
    entry_window_early: list[int] = field(default_factory=lambda: [0, 60])
    entry_window_late: list[int] = field(default_factory=lambda: [270, 295])
    enable_early_snipe: bool = True
    enable_late_snipe: bool = True


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
    cheap_token_multiplier: float = 2.0


@dataclass
class PaperConfig:
    enabled: bool = True
    starting_balance: float = 100.0


@dataclass
class StrategyConfig:
    version: int = 1
    promoted_at: str = ""
    signal: SignalConfig = field(default_factory=SignalConfig)
    execution: ExecutionConfig = field(default_factory=ExecutionConfig)
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

    cfg = StrategyConfig(
        version=data.get("version", 1),
        promoted_at=data.get("promoted_at", ""),
    )
    def _filter_fields(cls: type, d: dict) -> dict:
        return {k: v for k, v in d.items() if k in cls.__dataclass_fields__}

    if "signal" in data:
        cfg.signal = SignalConfig(**_filter_fields(SignalConfig, data["signal"]))
    if "execution" in data:
        cfg.execution = ExecutionConfig(**_filter_fields(ExecutionConfig, data["execution"]))
    if "risk" in data:
        cfg.risk = RiskConfig(**_filter_fields(RiskConfig, data["risk"]))
    if "paper" in data:
        cfg.paper = PaperConfig(**_filter_fields(PaperConfig, data["paper"]))
    return cfg
