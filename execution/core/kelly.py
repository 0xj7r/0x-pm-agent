"""Empirical Kelly sizing from resolved trade history."""
from __future__ import annotations

import json
from dataclasses import dataclass

from shared.fees import taker_fee
from strategies.strategy_config import RiskConfig


PRICE_BUCKETS = (
    (0.00, 0.10, "0-10¢"),
    (0.10, 0.25, "10-25¢"),
    (0.25, 0.40, "25-40¢"),
    (0.40, 0.55, "40-55¢"),
    (0.55, 1.00, "55¢+"),
)


@dataclass(frozen=True)
class PriceBucketEstimate:
    bucket: str
    wins: int
    losses: int
    p_win: float

    @property
    def samples(self) -> int:
        return self.wins + self.losses


def bucket_for_price(price: float) -> str:
    for low, high, label in PRICE_BUCKETS:
        if low <= price < high:
            return label
    return "55¢+"


def full_kelly_fraction(p_win: float, token_price: float, *, fee_aware: bool = True) -> float:
    """Full Kelly fraction for binary shares sized in trade dollars.

    `token_price` is the dollars paid per share. The returned fraction is the
    fraction of bankroll to spend on the position before applying fractional
    Kelly and caps.
    """
    if p_win <= 0 or token_price <= 0 or token_price >= 1:
        return 0.0

    fee_rate = taker_fee(token_price) if fee_aware else 0.0
    win_return = (1.0 - token_price) / token_price - fee_rate
    loss_return = 1.0 + fee_rate
    if win_return <= 0 or loss_return <= 0:
        return 0.0

    expected_return = p_win * win_return - (1.0 - p_win) * loss_return
    if expected_return <= 0:
        return 0.0

    return max(0.0, expected_return / (win_return * loss_return))


def empirical_kelly_size(
    p_win: float,
    token_price: float,
    bankroll: float,
    risk_cfg: RiskConfig,
) -> float:
    if bankroll <= 0:
        return 0.0

    full_fraction = full_kelly_fraction(p_win, token_price)
    adjusted_fraction = full_fraction * risk_cfg.kelly_multiplier
    max_size = min(bankroll * risk_cfg.max_position_pct, risk_cfg.max_position_usd)
    size = min(adjusted_fraction * bankroll, max_size)

    if size < risk_cfg.empirical_kelly_min_size_usd:
        return 0.0
    return round(size, 2)


def estimate_price_bucket_p_win(memory, token_price: float) -> PriceBucketEstimate | None:
    """Estimate p_win from prior resolved trades in the same entry-price bucket.

    Returns None when the bucket has no resolved history, allowing callers to
    preserve the existing flat sizing behavior until the DB has evidence.
    """
    bucket = bucket_for_price(token_price)
    wins = 0
    losses = 0

    rows = memory.conn.execute(
        "SELECT details FROM event_log WHERE event_type = 'resolution'"
    ).fetchall()
    for (details,) in rows:
        if not details:
            continue
        try:
            payload = json.loads(details)
            trade = payload.get("trade") or {}
            historical_price = float(trade.get("token_price") or 0.0)
        except (TypeError, ValueError, json.JSONDecodeError):
            continue
        if bucket_for_price(historical_price) != bucket:
            continue
        if bool(payload.get("won")):
            wins += 1
        else:
            losses += 1

    samples = wins + losses
    if samples == 0:
        return None

    return PriceBucketEstimate(
        bucket=bucket,
        wins=wins,
        losses=losses,
        p_win=wins / samples,
    )


def smoothed_bucket_estimate(
    estimate: PriceBucketEstimate,
    token_price: float,
    prior_weight: float,
    prior_edge: float = 0.0,
) -> PriceBucketEstimate:
    """Bayesian-ish smoothing toward a price-based prior.

    Prior target is `token_price + prior_edge`, clamped to [0, 0.98]. A
    non-zero `prior_edge` expresses "we think we have some edge over
    market consensus" so cold-start buckets don't collapse to Kelly = 0.
    As `samples` grows, the empirical rate dominates.
    """
    if prior_weight <= 0:
        return estimate
    samples = estimate.samples
    prior_target = max(0.0, min(0.98, token_price + prior_edge))
    p_win = (estimate.wins + prior_weight * prior_target) / (samples + prior_weight)
    return PriceBucketEstimate(
        bucket=estimate.bucket,
        wins=estimate.wins,
        losses=estimate.losses,
        p_win=p_win,
    )
