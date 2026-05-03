//! Paper-mode fill simulation helpers for the runner.

use crate::book::BookState;
use crate::runtime::execution_policy::ExecutionPolicy;
use crate::runtime::runner::PaperOrderContext;
use crate::types::{FillLiquidity, FillReport, OrderIntent, TradeSide};

pub(super) fn paper_fill_from_book_snapshot(
    book: &BookState,
    intent: &OrderIntent,
    observed_at_ms: u64,
    paper_fee_coeff: f64,
    order_ctx: &mut PaperOrderContext,
    remaining_qty: f64,
    execution_policy: &ExecutionPolicy,
) -> Option<FillReport> {
    if remaining_qty <= 0.0 || intent.limit_price <= 0.0 {
        return None;
    }
    if order_ctx.fill_count >= execution_policy.paper_max_fills_per_order {
        return None;
    }
    if execution_policy.paper_submit_latency_ms > 0
        && observed_at_ms.saturating_sub(order_ctx.arrival_ms)
            < execution_policy.paper_submit_latency_ms
    {
        return None;
    }
    if order_ctx.last_fill_ms > 0
        && observed_at_ms.saturating_sub(order_ctx.last_fill_ms)
            < execution_policy.paper_min_fill_interval_ms
    {
        return None;
    }
    if book.last_update_unix_ms > 0
        && order_ctx.last_fill_book_update_ms == book.last_update_unix_ms
    {
        return None;
    }

    let candidate_levels: Vec<_> = if matches!(intent.side, TradeSide::Buy) {
        book.ask_levels()
            .iter()
            .filter(|level| level.price > 0.0 && level.price <= intent.limit_price)
            .collect()
    } else {
        book.bid_levels()
            .iter()
            .filter(|level| level.price > 0.0 && level.price >= intent.limit_price)
            .collect()
    };
    if candidate_levels.is_empty() {
        return None;
    }

    let total_available: f64 = candidate_levels.iter().map(|level| level.size).sum();
    if total_available <= 0.0 {
        return None;
    }

    let best_opposite = candidate_levels[0].price;
    let crossing = if matches!(intent.side, TradeSide::Buy) {
        book.best_ask > 0.0 && intent.limit_price >= book.best_ask
    } else {
        book.best_bid > 0.0 && intent.limit_price <= book.best_bid
    };
    let maker_trade_through = if matches!(intent.side, TradeSide::Buy) {
        book.last_trade_price > 0.0 && book.last_trade_price <= intent.limit_price
    } else {
        book.last_trade_price > 0.0 && book.last_trade_price >= intent.limit_price
    };
    let order_age_ms = observed_at_ms.saturating_sub(order_ctx.arrival_ms);
    let is_resting = order_age_ms > execution_policy.paper_submit_latency_ms;
    let crosses_as_taker = crossing && !is_resting;
    if !crossing {
        let queue_wait_ms = 1_000 + (order_ctx.queue_bias * 3_000.0) as u64;
        if order_age_ms < queue_wait_ms || !maker_trade_through {
            return None;
        }
    }
    let best_fill_price = if crosses_as_taker {
        best_opposite
    } else {
        intent.limit_price
    };
    let fill_ratio = paper_fill_ratio(
        remaining_qty,
        total_available,
        book.last_update_unix_ms,
        best_fill_price,
        intent.limit_price,
        order_ctx,
        crossing,
        observed_at_ms,
    );
    let target_fill_qty = (remaining_qty * fill_ratio)
        .min(total_available)
        .min(remaining_qty);
    if target_fill_qty <= 0.0 {
        return None;
    }

    let mut remaining = target_fill_qty;
    let mut qty_filled = 0.0;
    let mut amount = 0.0;
    for (idx, level) in candidate_levels.iter().enumerate() {
        if remaining <= 0.0 {
            break;
        }
        let level_ratio = if crossing {
            1.0
        } else if idx == 0 {
            (1.0 - execution_policy.paper_queue_depth_fraction).max(0.0)
        } else {
            0.0
        };
        let level_fill = (level.size * level_ratio).min(remaining);
        if level_fill > 0.0 {
            qty_filled += level_fill;
            let price_at_level = if crosses_as_taker {
                level.price
            } else {
                intent.limit_price
            };
            amount += level_fill * price_at_level;
            remaining -= level_fill;
        }
    }

    if qty_filled <= 0.0 {
        qty_filled = target_fill_qty.min(candidate_levels[0].size);
        let fallback_price = if crosses_as_taker {
            candidate_levels[0].price
        } else {
            intent.limit_price
        };
        amount = qty_filled * fallback_price;
    }

    if qty_filled <= 0.0 {
        return None;
    }

    let price = amount / qty_filled;
    if price <= 0.0 {
        return None;
    }

    let liquidity = if crosses_as_taker {
        FillLiquidity::Taker
    } else {
        FillLiquidity::Maker
    };
    let notional = qty_filled * price;
    if notional < execution_policy.paper_min_fill_notional_usd
        && (remaining_qty * price) >= execution_policy.paper_min_fill_notional_usd
    {
        return None;
    }
    let effective_taker_coeff = execution_policy
        .paper_taker_fee_coeff_override
        .unwrap_or(paper_fee_coeff);
    let fee_basis = price * (1.0 - price);
    let fee = match liquidity {
        FillLiquidity::Maker => -(notional * execution_policy.paper_maker_rebate_coeff * fee_basis),
        FillLiquidity::Taker | FillLiquidity::Unknown => {
            notional * effective_taker_coeff * fee_basis
        }
    };
    order_ctx.last_fill_ms = observed_at_ms;
    order_ctx.last_fill_book_update_ms = book.last_update_unix_ms;
    order_ctx.fill_count = order_ctx.fill_count.saturating_add(1);

    Some(FillReport {
        order_id: None,
        client_order_id: Some(intent.client_order_id.clone()),
        market_id: intent.market_id.clone(),
        instrument_id: intent.instrument_id.clone(),
        side: intent.side,
        price,
        quantity: qty_filled,
        fee_usd: fee,
        liquidity,
        close_method: None,
        observed_at_ms,
    })
}

fn paper_fill_ratio(
    order_qty: f64,
    available_qty: f64,
    snapshot_unix_ms: u64,
    fill_price: f64,
    limit_price: f64,
    order_ctx: &PaperOrderContext,
    crossing: bool,
    observed_at_ms: u64,
) -> f64 {
    if order_qty <= 0.0 || available_qty <= 0.0 || fill_price <= 0.0 || limit_price <= 0.0 {
        return 0.0;
    }

    let age_ms = observed_at_ms.saturating_sub(order_ctx.arrival_ms.max(order_ctx.last_attempt_ms));
    let age_pressure = if crossing {
        0.15 + 0.30 * ((age_ms as f64 / 3_000.0).clamp(0.0, 1.0))
    } else {
        0.02 + 0.18 * ((age_ms as f64 / 5_000.0).clamp(0.0, 1.0))
    };
    let size_pressure = 0.25 + 0.75 * (available_qty / (available_qty + order_qty));
    let queue_pressure = 0.08 + order_ctx.queue_bias * 0.52;
    let staleness_pressure = 0.30
        + 0.60
            * ((observed_at_ms.saturating_sub(snapshot_unix_ms) as f64 / 2_000.0).clamp(0.0, 1.0));
    let premium = ((limit_price - fill_price) / fill_price).max(0.0).min(1.0);
    let limit_pressure = if crossing {
        0.65
    } else {
        0.20 + (premium * 0.20)
    };
    (age_pressure * size_pressure * queue_pressure * staleness_pressure * limit_pressure)
        .clamp(0.0, if crossing { 0.65 } else { 0.20 })
}

pub(super) fn deterministic_hash_0_95(value: &str) -> f64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    let normalized = (hash & 0xffff) as f64 / 65_536.0;
    (0.05 + (normalized * 0.95)).min(1.0)
}

fn deterministic_unit_hash(client_order_id: &str, book_update_ms: u64) -> f64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in client_order_id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    for byte in book_update_ms.to_le_bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    (hash & 0xffff_ffff) as f64 / 4_294_967_296.0
}

pub(super) fn paper_post_only_should_reject(
    intent: &OrderIntent,
    book: &BookState,
    execution_policy: &ExecutionPolicy,
) -> bool {
    if !execution_policy.paper_mode {
        return false;
    }
    if execution_policy.paper_post_only_reject_probability <= 0.0 {
        return false;
    }
    let crossing = if matches!(intent.side, TradeSide::Buy) {
        book.best_ask > 0.0 && intent.limit_price >= book.best_ask
    } else {
        book.best_bid > 0.0 && intent.limit_price <= book.best_bid
    };
    if !crossing {
        return false;
    }
    let roll = deterministic_unit_hash(intent.client_order_id.as_str(), book.last_update_unix_ms);
    roll < execution_policy.paper_post_only_reject_probability
}
