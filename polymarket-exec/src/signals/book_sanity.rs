//! Order-book sanity signal for paired-MM.
//!
//! This is deliberately a soft penalty. It tells sizing logic when visible
//! liquidity, spreads, or BBO quality make quotes less trustworthy.

use crate::market_making::pairing::types::{LadderLeg, PairedMarketSnapshot};
use crate::types::{BookLevel, EpochMillis, QuoteSnapshot};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BookSanityConfig {
    pub max_spread: f64,
    pub min_top_depth_notional_usd: f64,
    pub max_staleness_ms: u64,
    pub max_queue_depth_usd: f64,
    pub max_projected_pair_cost: f64,
}

impl Default for BookSanityConfig {
    fn default() -> Self {
        Self {
            max_spread: 0.08,
            min_top_depth_notional_usd: 3.0,
            max_staleness_ms: 1_500,
            max_queue_depth_usd: 200.0,
            max_projected_pair_cost: 0.995,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BookSanityLeg {
    pub spread: Option<f64>,
    pub top_depth_notional_usd: f64,
    pub same_side_queue_notional_usd: f64,
    pub projected_pair_cost: Option<f64>,
    pub missing_bbo: bool,
    pub crossed: bool,
    pub stale: bool,
    pub penalty: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BookSanitySignal {
    pub yes: BookSanityLeg,
    pub no: BookSanityLeg,
    pub max_penalty: f64,
}

impl BookSanitySignal {
    pub fn compute(
        snapshot: &PairedMarketSnapshot,
        now_ms: EpochMillis,
        config: BookSanityConfig,
    ) -> Self {
        let yes = leg_sanity(&snapshot.yes_quote, &snapshot.no_quote, now_ms, config);
        let no = leg_sanity(&snapshot.no_quote, &snapshot.yes_quote, now_ms, config);
        Self {
            yes,
            no,
            max_penalty: yes.penalty.max(no.penalty),
        }
    }

    pub fn penalty_for_leg(self, leg: LadderLeg) -> f64 {
        match leg {
            LadderLeg::Yes => self.yes.penalty,
            LadderLeg::No => self.no.penalty,
        }
    }
}

fn leg_sanity(
    quote: &QuoteSnapshot,
    opposite_quote: &QuoteSnapshot,
    now_ms: EpochMillis,
    config: BookSanityConfig,
) -> BookSanityLeg {
    let missing_bbo = quote.best_bid.is_none() || quote.best_ask.is_none();
    let spread = quote.spread();
    let crossed = match (&quote.best_bid, &quote.best_ask) {
        (Some(bid), Some(ask)) => bid.price >= ask.price,
        _ => false,
    };
    let stale = is_stale(quote, now_ms, config.max_staleness_ms);
    let top_depth_notional_usd = top_depth_notional(quote);
    let same_side_queue_notional_usd = quote.best_bid.as_ref().map(level_notional).unwrap_or(0.0);
    let projected_pair_cost = match (&quote.best_bid, &opposite_quote.best_ask) {
        (Some(bid), Some(opposite_ask)) => Some(bid.price + opposite_ask.price),
        _ => None,
    };

    let spread_penalty = spread
        .map(|spread| (spread / config.max_spread.max(1e-9)).clamp(0.0, 1.0))
        .unwrap_or(1.0);
    let depth_penalty = (1.0
        - (top_depth_notional_usd / config.min_top_depth_notional_usd.max(1e-9)))
    .clamp(0.0, 1.0);
    let queue_penalty =
        (same_side_queue_notional_usd / config.max_queue_depth_usd.max(1e-9)).clamp(0.0, 1.0);
    let pair_cost_penalty = projected_pair_cost
        .map(|pair_cost| ((pair_cost - config.max_projected_pair_cost) / 0.02).clamp(0.0, 1.0))
        .unwrap_or(0.5);
    let structural_penalty =
        bool_penalty(missing_bbo) + bool_penalty(crossed) + bool_penalty(stale);
    let penalty = ((0.25 * spread_penalty)
        + (0.25 * depth_penalty)
        + (0.15 * queue_penalty)
        + (0.20 * pair_cost_penalty)
        + (0.15 * structural_penalty.min(1.0)))
    .clamp(0.0, 1.0);

    BookSanityLeg {
        spread,
        top_depth_notional_usd,
        same_side_queue_notional_usd,
        projected_pair_cost,
        missing_bbo,
        crossed,
        stale,
        penalty,
    }
}

fn is_stale(quote: &QuoteSnapshot, now_ms: EpochMillis, max_staleness_ms: u64) -> bool {
    if now_ms == 0 || max_staleness_ms == 0 {
        return false;
    }
    let observed = quote.depth_observed_at_ms.unwrap_or(quote.observed_at_ms);
    observed > 0 && now_ms.saturating_sub(observed) > max_staleness_ms
}

fn top_depth_notional(quote: &QuoteSnapshot) -> f64 {
    quote.best_bid.as_ref().map(level_notional).unwrap_or(0.0)
        + quote.best_ask.as_ref().map(level_notional).unwrap_or(0.0)
}

fn level_notional(level: &BookLevel) -> f64 {
    level.price.max(0.0) * level.quantity.max(0.0)
}

fn bool_penalty(value: bool) -> f64 {
    if value {
        1.0
    } else {
        0.0
    }
}
