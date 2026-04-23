"""Shared whale-pair configuration presets used by analysis and validation scripts."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Final

from strategies.whale_pair import WhalePairConfig, WhalePairVariant


@dataclass(frozen=True)
class ValidationSpec:
    variant: WhalePairVariant
    description: str
    defaults: WhalePairConfig


W1_COMPARE_VARIANTS: Final[dict[str, WhalePairConfig]] = {
    "pair_recycler": WhalePairConfig(
        variant="pair_recycler",
        max_pair_cost=0.985,
        accumulate_price_max=0.50,
        aggressive_price_max=0.10,
        base_clip_usd=25.0,
        aggressive_clip_usd=50.0,
        max_gross_cost_usd=250.0,
        min_seconds_from_start=10,
        max_seconds_from_start=240,
        completion_min_pnl_per_share=0.002,
        max_imbalance_ratio=3.0,
    ),
    "skewed_pair_builder": WhalePairConfig(
        variant="skewed_pair_builder",
        max_pair_cost=0.985,
        accumulate_price_max=0.60,
        aggressive_price_max=0.10,
        base_clip_usd=25.0,
        aggressive_clip_usd=50.0,
        max_gross_cost_usd=250.0,
        min_seconds_from_start=10,
        max_seconds_from_start=240,
        completion_min_pnl_per_share=0.002,
        max_imbalance_ratio=4.0,
    ),
    "passive_ladder": WhalePairConfig(
        variant="passive_ladder",
        max_pair_cost=0.99,
        accumulate_price_max=0.50,
        aggressive_price_max=0.10,
        base_clip_usd=25.0,
        aggressive_clip_usd=50.0,
        base_clip_shares=25.0,
        aggressive_clip_shares=100.0,
        max_gross_cost_usd=250.0,
        min_seconds_from_start=10,
        max_seconds_from_start=240,
        completion_min_pnl_per_share=0.002,
        max_imbalance_ratio=4.0,
    ),
    "w1_mimic": WhalePairConfig(
        variant="w1_mimic",
        max_pair_cost=0.99,
        accumulate_price_max=0.50,
        aggressive_price_max=0.10,
        base_clip_usd=25.0,
        aggressive_clip_usd=50.0,
        base_clip_shares=25.0,
        aggressive_clip_shares=100.0,
        max_gross_cost_usd=250.0,
        min_seconds_from_start=10,
        max_seconds_from_start=240,
        completion_min_pnl_per_share=0.002,
        max_imbalance_ratio=4.0,
    ),
}


W1_VALIDATION_SPECS: Final[dict[str, ValidationSpec]] = {
    "pair_recycler": ValidationSpec(
        variant="pair_recycler",
        description=(
            "Neutral recycler baseline: pair-first preference + strict same-side cap"
        ),
        defaults=W1_COMPARE_VARIANTS["pair_recycler"],
    ),
    "skewed_pair_builder": ValidationSpec(
        variant="skewed_pair_builder",
        description=(
            "Intentional skew allowed with controlled imbalance bounds"
        ),
        defaults=W1_COMPARE_VARIANTS["skewed_pair_builder"],
    ),
    "passive_ladder": ValidationSpec(
        variant="passive_ladder",
        description=(
            "Passive-style clip behavior (share-sized legs) with same execution budget"
        ),
        defaults=W1_COMPARE_VARIANTS["passive_ladder"],
    ),
    "w1_mimic": ValidationSpec(
        variant="w1_mimic",
        description=(
            "Explicit W1-hypothesis profile: passive-style clips plus controlled skew"
        ),
        defaults=W1_COMPARE_VARIANTS["w1_mimic"],
    ),
}


def default_compare_variants() -> dict[str, WhalePairConfig]:
    return {name: cfg for name, cfg in W1_COMPARE_VARIANTS.items()}


RUST_VARIANT_MATRIX: Final[list[tuple[str, dict[str, str]]]] = [
    (
        "w1_like",
        {
            "WHALE_PAIR_ACCUMULATE_PRICE_MAX": "0.58",
            "WHALE_PAIR_AGGRESSIVE_PRICE_MAX": "0.10",
            "WHALE_PAIR_BASE_CLIP_USD": "22.5",
            "WHALE_PAIR_AGGRESSIVE_CLIP_USD": "100.0",
            "WHALE_PAIR_MAX_GROSS_COST_USD": "250.0",
            "WHALE_PAIR_COMPLETION_MIN_PNL_PER_SHARE": "0.0025",
            "WHALE_PAIR_MAX_IMBALANCE_RATIO": "4.0",
            "WHALE_PAIR_TAKER_FEE_COEFF": "0.072",
            "WHALE_PAIR_EXEC_MAX_ORDER_NOTIONAL_USD": "1000.0",
            "WHALE_PAIR_EXEC_MAX_NET_NOTIONAL_PER_MARKET_USD": "700.0",
        },
    ),
    (
        "balanced_pair",
        {
            "WHALE_PAIR_ACCUMULATE_PRICE_MAX": "0.55",
            "WHALE_PAIR_AGGRESSIVE_PRICE_MAX": "0.08",
            "WHALE_PAIR_BASE_CLIP_USD": "18.0",
            "WHALE_PAIR_AGGRESSIVE_CLIP_USD": "80.0",
            "WHALE_PAIR_MAX_GROSS_COST_USD": "220.0",
            "WHALE_PAIR_COMPLETION_MIN_PNL_PER_SHARE": "0.0020",
            "WHALE_PAIR_MAX_IMBALANCE_RATIO": "3.5",
            "WHALE_PAIR_TAKER_FEE_COEFF": "0.072",
            "WHALE_PAIR_EXEC_MAX_ORDER_NOTIONAL_USD": "700.0",
            "WHALE_PAIR_EXEC_MAX_NET_NOTIONAL_PER_MARKET_USD": "600.0",
        },
    ),
    (
        "completion_guard",
        {
            "WHALE_PAIR_ACCUMULATE_PRICE_MAX": "0.62",
            "WHALE_PAIR_AGGRESSIVE_PRICE_MAX": "0.12",
            "WHALE_PAIR_BASE_CLIP_USD": "16.0",
            "WHALE_PAIR_AGGRESSIVE_CLIP_USD": "70.0",
            "WHALE_PAIR_MAX_GROSS_COST_USD": "200.0",
            "WHALE_PAIR_COMPLETION_MIN_PNL_PER_SHARE": "0.0035",
            "WHALE_PAIR_MAX_IMBALANCE_RATIO": "5.0",
            "WHALE_PAIR_TAKER_FEE_COEFF": "0.072",
            "WHALE_PAIR_EXEC_MAX_ORDER_NOTIONAL_USD": "500.0",
            "WHALE_PAIR_EXEC_MAX_NET_NOTIONAL_PER_MARKET_USD": "500.0",
        },
    ),
]


def rust_variant_env_lines() -> list[str]:
    return ["|".join([name] + [f"{key}={value}" for key, value in env.items()]) for name, env in RUST_VARIANT_MATRIX]

