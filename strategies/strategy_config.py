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
    empirical_kelly_enabled: bool = False
    empirical_kelly_prior_weight: float = 1.0
    empirical_kelly_prior_edge: float = 0.05
    empirical_kelly_min_size_usd: float = 1.0
    max_daily_trades: int = 0
    reject_streak_limit: int = 0
    reject_cooldown_seconds: int = 60
    reject_streak_cooldown_seconds: int = 600
    drift_threshold_usd: float = 2.0
    order_poll_interval_seconds: float = 3.0
    order_fill_deadline_buffer_seconds: float = 30.0
    # Cross-the-spread slippage added to token_price on BUY. Live orders
    # that don't cross the ask often sit unfilled inside a 5-min window;
    # paper uses the same constant so live/paper PnL are comparable.
    live_entry_slippage_usd: float = 0.01
    # Polymarket CLOB minimum shares per order. Orders below this are
    # rejected server-side; engine skips or upsizes to meet the floor.
    min_shares: float = 5.0
    # Paper-mode realism: simulate live fill constraints (price can cross,
    # partial fill at best-ask size). Disabled by default so historical
    # paper stats remain comparable.
    paper_livelike_enabled: bool = False
    paper_livelike_latency_ms: int = 150
    paper_livelike_use_best_ask_size: bool = True
    # Live-mode redemption hygiene: periodically sweep for unredeemed wins.
    redeem_sweep_interval_seconds: int = 300
    redeem_blind_when_token_missing: bool = True
    # Optional early-exit logic (SELL to take profit before resolution).
    take_profit_enabled: bool = False
    take_profit_best_bid_threshold: float = 0.95
    take_profit_sell_fraction: float = 1.0
    take_profit_min_best_bid_size_shares: float = 0.0
    take_profit_min_unrealized_usd: float = 0.0
    take_profit_exit_slippage_usd: float = 0.005
    take_profit_order_timeout_seconds: int = 20
    # Skip take-profit trigger if the current window has < this many seconds
    # remaining. Late-window sells give up 1-5¢/share vs holding to $1 at
    # resolution, with no meaningful reversal risk to capture. 0 disables.
    take_profit_min_seconds_remaining: float = 0.0
    # Optional early-exit logic (SELL to cut losses before resolution).
    stop_loss_enabled: bool = False
    stop_loss_best_bid_threshold: float = 0.15
    stop_loss_sell_fraction: float = 1.0
    stop_loss_min_best_bid_size_shares: float = 0.0
    stop_loss_exit_slippage_usd: float = 0.01
    stop_loss_order_timeout_seconds: int = 20


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
