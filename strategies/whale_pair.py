"""Shared logic for whale-family two-sided pair/merge strategies."""
from __future__ import annotations

from dataclasses import dataclass, field
from typing import Literal

from shared.fees import taker_fee_usd

WhalePairVariant = Literal["pair_recycler", "skewed_pair_builder", "passive_ladder"]


@dataclass(frozen=True)
class WhalePairConfig:
    variant: WhalePairVariant = "pair_recycler"
    max_pair_cost: float = 0.99
    accumulate_price_max: float = 0.50
    aggressive_price_max: float = 0.10
    base_clip_usd: float = 10.0
    aggressive_clip_usd: float = 25.0
    base_clip_shares: float | None = None
    aggressive_clip_shares: float | None = None
    max_gross_cost_usd: float = 200.0
    min_seconds_from_start: int = 10
    max_seconds_from_start: int = 298
    completion_min_pnl_per_share: float = 0.002
    max_imbalance_ratio: float = 3.0


@dataclass(frozen=True)
class BookTop:
    ask: float
    ask_size: float


@dataclass
class OpenLot:
    side: str
    shares_remaining: float
    gross_cost_remaining_usd: float
    fee_remaining_usd: float

    @property
    def all_in_cost_per_share(self) -> float | None:
        if self.shares_remaining <= 0:
            return None
        return (
            self.gross_cost_remaining_usd + self.fee_remaining_usd
        ) / self.shares_remaining


@dataclass(frozen=True)
class FillDecision:
    side: str
    reason: str
    price: float
    ask_size: float
    shares: float
    gross_cost_usd: float
    fee_usd: float


@dataclass(frozen=True)
class PairFillDecision:
    up_fill: FillDecision
    down_fill: FillDecision

    @property
    def gross_cost_usd(self) -> float:
        return self.up_fill.gross_cost_usd + self.down_fill.gross_cost_usd


@dataclass(frozen=True)
class MatchResult:
    shares: float
    up_cost_usd: float
    up_fee_usd: float
    down_cost_usd: float
    down_fee_usd: float
    payout_usd: float
    realized_pnl_usd: float


@dataclass
class WhalePairMarketState:
    gross_cost_usd: float = 0.0
    up_lots: list[OpenLot] = field(default_factory=list)
    down_lots: list[OpenLot] = field(default_factory=list)
    fills: list[FillDecision] = field(default_factory=list)
    matches: list[MatchResult] = field(default_factory=list)

    def open_inventory(self, side: str) -> OpenLot:
        lots = self.up_lots if side == "Up" else self.down_lots
        shares = sum(l.shares_remaining for l in lots if l.shares_remaining > 0)
        gross = sum(l.gross_cost_remaining_usd for l in lots if l.shares_remaining > 0)
        fee = sum(l.fee_remaining_usd for l in lots if l.shares_remaining > 0)
        return OpenLot(
            side=side,
            shares_remaining=shares,
            gross_cost_remaining_usd=gross,
            fee_remaining_usd=fee,
        )


def choose_accumulate_clip_usd(price: float, cfg: WhalePairConfig) -> float | None:
    if price <= 0:
        return None
    if price <= cfg.aggressive_price_max:
        return cfg.aggressive_clip_usd
    if price <= cfg.accumulate_price_max:
        return cfg.base_clip_usd
    return None


def choose_clip_shares(price: float, cfg: WhalePairConfig) -> float | None:
    if cfg.variant != "passive_ladder" or price <= 0:
        return None
    if price <= cfg.aggressive_price_max:
        return cfg.aggressive_clip_shares or 100.0
    if price <= cfg.accumulate_price_max:
        return cfg.base_clip_shares or 25.0
    return cfg.base_clip_shares or 25.0


def choose_pair_clip_shares(cfg: WhalePairConfig) -> float | None:
    if cfg.variant != "passive_ladder":
        return None
    return cfg.aggressive_clip_shares or 100.0


def maybe_decide_pair_fill(
    *,
    up_top: BookTop,
    down_top: BookTop,
    state: WhalePairMarketState,
    cfg: WhalePairConfig,
) -> PairFillDecision | None:
    pair_cost = up_top.ask + down_top.ask
    if up_top.ask <= 0 or down_top.ask <= 0 or pair_cost > cfg.max_pair_cost:
        return None
    remaining_budget = cfg.max_gross_cost_usd - state.gross_cost_usd
    if remaining_budget <= 0:
        return None

    cheaper = min(up_top.ask, down_top.ask)
    pair_clip_shares = choose_pair_clip_shares(cfg)
    if pair_clip_shares is not None:
        shares = min(
            up_top.ask_size,
            down_top.ask_size,
            pair_clip_shares,
            remaining_budget / pair_cost if pair_cost > 0 else 0.0,
        )
    else:
        clip_usd = choose_accumulate_clip_usd(cheaper, cfg) or cfg.base_clip_usd
        target_pair_cost = min(clip_usd, remaining_budget)
        shares = min(
            up_top.ask_size,
            down_top.ask_size,
            target_pair_cost / pair_cost if pair_cost > 0 else 0.0,
        )
    if shares <= 0:
        return None

    up_gross = shares * up_top.ask
    down_gross = shares * down_top.ask
    return PairFillDecision(
        up_fill=FillDecision(
            side="Up",
            reason="pair_accumulate",
            price=up_top.ask,
            ask_size=up_top.ask_size,
            shares=shares,
            gross_cost_usd=up_gross,
            fee_usd=taker_fee_usd(up_top.ask, up_gross),
        ),
        down_fill=FillDecision(
            side="Down",
            reason="pair_accumulate",
            price=down_top.ask,
            ask_size=down_top.ask_size,
            shares=shares,
            gross_cost_usd=down_gross,
            fee_usd=taker_fee_usd(down_top.ask, down_gross),
        ),
    )


def completion_pnl_per_share(opposite_all_in_per_share: float | None, current_price: float) -> float | None:
    if opposite_all_in_per_share is None or current_price <= 0:
        return None
    current_fee_per_share = taker_fee_usd(current_price, current_price)
    return 1.0 - opposite_all_in_per_share - current_price - current_fee_per_share


def choose_fill_shares(price: float, ask_size: float, clip_usd: float, remaining_budget_usd: float) -> float:
    if price <= 0 or ask_size <= 0 or clip_usd <= 0 or remaining_budget_usd <= 0:
        return 0.0
    return min(ask_size, clip_usd / price, remaining_budget_usd / price)


def choose_fill_shares_variant(
    *,
    price: float,
    ask_size: float,
    clip_usd: float,
    remaining_budget_usd: float,
    clip_shares: float | None,
) -> float:
    shares = choose_fill_shares(
        price=price,
        ask_size=ask_size,
        clip_usd=clip_usd,
        remaining_budget_usd=remaining_budget_usd,
    )
    if clip_shares is None or shares <= 0:
        return shares
    return min(shares, ask_size, clip_shares)


def maybe_decide_fill(
    *,
    side: str,
    top: BookTop,
    state: WhalePairMarketState,
    cfg: WhalePairConfig,
    allow_accumulate: bool = True,
) -> FillDecision | None:
    same_inventory = state.open_inventory(side)
    opposite_inventory = state.open_inventory("Down" if side == "Up" else "Up")
    remaining_budget = cfg.max_gross_cost_usd - state.gross_cost_usd
    if remaining_budget <= 0:
        return None

    clip_usd = choose_accumulate_clip_usd(top.ask, cfg)
    clip_shares = choose_clip_shares(top.ask, cfg)
    reason = None
    if clip_usd is not None and allow_accumulate:
        projected_increment = clip_shares if clip_shares is not None else (clip_usd / max(top.ask, 1e-9))
        projected_same = same_inventory.shares_remaining + projected_increment
        projected_opp = max(opposite_inventory.shares_remaining, 1e-9)
        if opposite_inventory.shares_remaining > 0 and projected_same / projected_opp > cfg.max_imbalance_ratio:
            return None
        reason = "accumulate"
    else:
        pnl_per_share = completion_pnl_per_share(
            opposite_inventory.all_in_cost_per_share, top.ask
        )
        if (
            opposite_inventory.shares_remaining > same_inventory.shares_remaining
            and pnl_per_share is not None
            and pnl_per_share >= cfg.completion_min_pnl_per_share
        ):
            clip_usd = cfg.base_clip_usd
            reason = "complete"
        else:
            return None

    shares = choose_fill_shares_variant(
        price=top.ask,
        ask_size=top.ask_size,
        clip_usd=clip_usd or 0.0,
        remaining_budget_usd=remaining_budget,
        clip_shares=clip_shares,
    )
    if shares <= 0:
        return None
    gross_cost = shares * top.ask
    fee = taker_fee_usd(top.ask, gross_cost)
    return FillDecision(
        side=side,
        reason=reason or "accumulate",
        price=top.ask,
        ask_size=top.ask_size,
        shares=shares,
        gross_cost_usd=gross_cost,
        fee_usd=fee,
    )


def apply_fill(state: WhalePairMarketState, fill: FillDecision) -> None:
    state.gross_cost_usd += fill.gross_cost_usd
    lot = OpenLot(
        side=fill.side,
        shares_remaining=fill.shares,
        gross_cost_remaining_usd=fill.gross_cost_usd,
        fee_remaining_usd=fill.fee_usd,
    )
    if fill.side == "Up":
        state.up_lots.append(lot)
    else:
        state.down_lots.append(lot)
    state.fills.append(fill)


def match_pairs(state: WhalePairMarketState) -> list[MatchResult]:
    realized: list[MatchResult] = []
    while True:
        up = next((lot for lot in state.up_lots if lot.shares_remaining > 0), None)
        down = next((lot for lot in state.down_lots if lot.shares_remaining > 0), None)
        if up is None or down is None:
            break
        shares = min(up.shares_remaining, down.shares_remaining)
        up_ratio = shares / up.shares_remaining
        down_ratio = shares / down.shares_remaining
        up_cost = up.gross_cost_remaining_usd * up_ratio
        up_fee = up.fee_remaining_usd * up_ratio
        down_cost = down.gross_cost_remaining_usd * down_ratio
        down_fee = down.fee_remaining_usd * down_ratio
        payout = shares
        match = MatchResult(
            shares=shares,
            up_cost_usd=up_cost,
            up_fee_usd=up_fee,
            down_cost_usd=down_cost,
            down_fee_usd=down_fee,
            payout_usd=payout,
            realized_pnl_usd=payout - up_cost - up_fee - down_cost - down_fee,
        )
        up.shares_remaining -= shares
        up.gross_cost_remaining_usd -= up_cost
        up.fee_remaining_usd -= up_fee
        down.shares_remaining -= shares
        down.gross_cost_remaining_usd -= down_cost
        down.fee_remaining_usd -= down_fee
        state.matches.append(match)
        realized.append(match)
    return realized


def resolve_residual_pnl(state: WhalePairMarketState, winner: str) -> float:
    pnl = 0.0
    for side, lots in (("Up", state.up_lots), ("Down", state.down_lots)):
        for lot in lots:
            if lot.shares_remaining <= 0:
                continue
            payout = lot.shares_remaining if side == winner else 0.0
            pnl += payout - lot.gross_cost_remaining_usd - lot.fee_remaining_usd
    return pnl
