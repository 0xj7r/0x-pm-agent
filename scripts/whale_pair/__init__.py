"""Structured whale-pair strategy utilities and reusable presets."""

from .strategy_presets import (
    ValidationSpec,
    W1_COMPARE_VARIANTS,
    W1_VALIDATION_SPECS,
    default_compare_variants,
    rust_variant_env_lines,
)

__all__ = [
    "ValidationSpec",
    "W1_COMPARE_VARIANTS",
    "W1_VALIDATION_SPECS",
    "default_compare_variants",
    "rust_variant_env_lines",
]
