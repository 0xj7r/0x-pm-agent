"""Tunable strategy parameters loaded from JSON config file."""
from __future__ import annotations

import json
import logging
from dataclasses import dataclass, field
import os
from pathlib import Path
from typing import Any

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
    cheap_token_multiplier: float = 2.0
    max_daily_trades: int = 0
    reject_streak_limit: int = 0
    drift_threshold_usd: float = 2.0


@dataclass
class PaperConfig:
    enabled: bool = True
    starting_balance: float = 100.0


@dataclass
class StrategyProfile:
    coins: dict[str, "CoinStrategyConfig"] = field(default_factory=dict)
    risk: RiskConfig = field(default_factory=RiskConfig)
    paper: PaperConfig = field(default_factory=PaperConfig)


@dataclass
class CoinStrategyConfig:
    strategy: str = "threshold"
    params: dict[str, Any] = field(default_factory=lambda: {
        "move": 0.08,
        "max_entry": 0.55,
    })

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> "CoinStrategyConfig":
        if "strategy" in data or "params" in data:
            strategy = data.get("strategy", "threshold")
            params = data.get("params", {})
            if strategy == "threshold" and not params:
                params = {
                    "move": data.get("move_threshold", 0.08),
                    "max_entry": data.get("max_entry", 0.55),
                }
            return cls(strategy=strategy, params=params)
        return cls(strategy="threshold", params={
            "move": data.get("move_threshold", 0.08),
            "max_entry": data.get("max_entry", 0.55),
        })


@dataclass
class StrategyConfig:
    version: int = 1
    profile: str = "default"
    available_profiles: tuple[str, ...] = ("default",)
    coins: dict[str, CoinStrategyConfig] = field(default_factory=lambda: {
        "btc": CoinStrategyConfig("threshold", {"move": 0.08, "max_entry": 0.55}),
        "eth": CoinStrategyConfig("threshold", {"move": 0.15, "max_entry": 0.55}),
        "sol": CoinStrategyConfig("threshold", {"move": 0.08, "max_entry": 0.55}),
    })
    risk: RiskConfig = field(default_factory=RiskConfig)
    paper: PaperConfig = field(default_factory=PaperConfig)


def _filter_fields(cls: type, data: dict[str, Any]) -> dict[str, Any]:
    return {k: v for k, v in data.items() if k in cls.__dataclass_fields__}


def _parse_profile(data: dict[str, Any]) -> StrategyProfile:
    profile = StrategyProfile()
    if "coins" in data:
        profile.coins = {
            coin: CoinStrategyConfig.from_dict(params)
            for coin, params in data["coins"].items()
        }
    if "risk" in data:
        profile.risk = RiskConfig(**_filter_fields(RiskConfig, data["risk"]))
    if "paper" in data:
        profile.paper = PaperConfig(**_filter_fields(PaperConfig, data["paper"]))
    return profile


def normalize_coin_config(config: Any) -> CoinStrategyConfig:
    if isinstance(config, CoinStrategyConfig):
        return config
    if isinstance(config, ThresholdConfig):
        return CoinStrategyConfig(
            strategy="threshold",
            params={
                "move": config.move_threshold,
                "max_entry": config.max_entry,
            },
        )
    if isinstance(config, dict):
        return CoinStrategyConfig.from_dict(config)
    return CoinStrategyConfig()


def list_strategy_profiles(path: str) -> list[str]:
    p = Path(path)
    if not p.exists():
        return ["default"]
    data = json.loads(p.read_text())
    if "profiles" not in data:
        return ["default"]
    return list(data["profiles"].keys())


def load_strategy_config(path: str, profile: str | None = None) -> StrategyConfig:
    """Load strategy config from JSON file. Returns defaults if file missing."""
    p = Path(path)
    if not p.exists():
        logger.warning(f"Config file not found at {path}, using defaults")
        return StrategyConfig(profile=profile or "default")

    with open(p) as f:
        data = json.load(f)

    if "profiles" in data:
        available_profiles = tuple(data["profiles"].keys())
        selected_profile = profile or os.getenv("STRATEGY_PROFILE") or data.get("default_profile", "default")
        profile_data = data["profiles"].get(selected_profile)
        if profile_data is None:
            raise ValueError(
                f"Unknown strategy profile '{selected_profile}'. "
                f"Available profiles: {', '.join(available_profiles)}"
            )
        resolved = _parse_profile(profile_data)
        return StrategyConfig(
            version=data.get("version", 1),
            profile=selected_profile,
            available_profiles=available_profiles,
            coins=resolved.coins or StrategyConfig().coins,
            risk=resolved.risk,
            paper=resolved.paper,
        )

    resolved = _parse_profile(data)
    return StrategyConfig(
        version=data.get("version", 1),
        profile=profile or "default",
        available_profiles=("default",),
        coins=resolved.coins or StrategyConfig().coins,
        risk=resolved.risk,
        paper=resolved.paper,
    )
