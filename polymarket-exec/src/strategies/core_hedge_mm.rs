//! Shared core/hedge ladder logic modeled on the `unlawful-shear` whale.
//!
//! Per `docs/research/unlawful-shear-microstructure-spec.md` the whale
//! does NOT run symmetric paired-MM around mid. Instead, on each bar:
//!
//! 1. Classifies legs by ask price: cheap (~0.29-0.31) and expensive
//!    (~0.66-0.70). The expensive leg is treated as the favorite.
//! 2. Builds the expensive leg as core (main exposure, ~67% of capital,
//!    ~$13 avg clip).
//! 3. Builds the cheap leg as hedge (~33% of capital, ~$5 avg clip).
//! 4. Continuously rebalances toward a target hedge ratio (~0.47
//!    cheap-to-expensive notional).
//! 5. Merges paired inventory aggressively to recycle capital (median
//!    first merge ~30s after entry).
//!
//! P1 (this revision): leg classifier + core/hedge ladder pricing only.
//! Merge planner integration and salvage are deferred.

use std::collections::HashMap;

use crate::core::types::{ClientOrderId, EpochMillis, IntentKind, OrderIntent};
use crate::market_making::pairing::types::{LadderLeg, PairedMarketSnapshot};
use crate::markets::MarketDescriptor;
use crate::strategies::traits::{StrategyFillInput, StrategyInput, TradingStrategy};
use crate::types::{MergeIntent, MarketId, StrategyDecision};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CoreHedgeMmConfig {
    pub enabled: bool,
    /// Maximum ask price for the cheap leg to be considered a valid hedge.
    pub cheap_leg_max_price: f64,
    /// Inclusive ask-price band for the expensive leg to be considered the
    /// favorite/core target.
    pub expensive_leg_min_price: f64,
    pub expensive_leg_max_price: f64,
    /// Minimum gap between the two ask prices required to engage.
    pub min_price_gap: f64,
    /// Per-bar capital target spread across both legs.
    pub bar_capital_usd: f64,
    /// Target cheap-to-expensive notional ratio. Whale sits at ~0.47.
    pub target_hedge_ratio: f64,
    /// Per-clip size by leg.
    pub core_clip_usd: f64,
    pub hedge_clip_usd: f64,
    /// Pricing offset from best_bid (in ticks) when posting a maker buy.
    /// 0 = join queue at best_bid; positive = improve.
    pub maker_improve_ticks: f64,
    /// Floor on per-order USD notional.
    pub min_order_usd: f64,
    /// Minimum paired inventory (min(yes_qty, no_qty)) that triggers a
    /// merge intent on the next tick. Whale data (493 paired windows)
    /// shows first merge ~30s after entry, so we want a low threshold.
    pub merge_min_qty: f64,
    /// Maximum paired quantity to merge in a single batch.
    pub merge_batch_cap: f64,
    /// Disable merge planning after this epoch-ms timestamp (redeem-only mode).
    pub disable_merge_after_ms: Option<EpochMillis>,
}

impl Default for CoreHedgeMmConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            cheap_leg_max_price: 0.38,
            expensive_leg_min_price: 0.52,
            expensive_leg_max_price: 0.92,
            min_price_gap: 0.12,
            bar_capital_usd: 50.0,
            target_hedge_ratio: 0.47,
            core_clip_usd: 13.0,
            hedge_clip_usd: 5.0,
            maker_improve_ticks: 0.0,
            min_order_usd: 1.0,
            merge_min_qty: 1.0,
            merge_batch_cap: f64::INFINITY,
            disable_merge_after_ms: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CoreHedgeMmStrategyConfig {
    pub core_hedge: CoreHedgeMmConfig,
}

impl Default for CoreHedgeMmStrategyConfig {
    fn default() -> Self {
        Self {
            core_hedge: CoreHedgeMmConfig::default(),
        }
    }
}

/// Tracks the last (price, qty) we emitted per (market, leg, tag) so we
/// only re-emit when those change. Without this, the strategy fires a
/// new intent every tick (~1 Hz), each with a fresh CoID. The replay
/// fill simulator treats every replace as a fresh `cumulative_trade_
/// through` counter — meaning we reset our queue-burn estimate every
/// second instead of accumulating it over minutes like a real maker.
/// That artifact lets the simulator fill us on tiny trade-through
/// events that wouldn't reach a long-resting maker.
type LastEmitKey = (MarketId, LadderLeg, &'static str);

#[derive(Clone, Debug)]
pub struct CoreHedgeMmStrategy {
    config: CoreHedgeMmStrategyConfig,
    last_emit: HashMap<LastEmitKey, (f64, f64)>,
}

impl CoreHedgeMmStrategy {
    pub fn new(config: CoreHedgeMmStrategyConfig) -> Self {
        Self {
            config,
            last_emit: HashMap::new(),
        }
    }

    pub fn config(&self) -> &CoreHedgeMmStrategyConfig {
        &self.config
    }

    /// Returns true (and records) if this leg+tag should re-emit at the
    /// given price/qty. Returns false if the prior emission was identical
    /// within tolerance — caller should skip the intent.
    fn should_emit(&mut self, market_id: &MarketId, leg: LadderLeg, tag: &'static str, price: f64, qty: f64) -> bool {
        let key = (market_id.clone(), leg, tag);
        let changed = match self.last_emit.get(&key) {
            Some(&(prev_px, prev_qty)) => {
                (price - prev_px).abs() > 1e-6 || (qty - prev_qty).abs() > 1e-6
            }
            None => true,
        };
        if changed {
            self.last_emit.insert(key, (price, qty));
        }
        changed
    }
}

#[derive(Clone, Copy, Debug)]
struct LegGeometry {
    expensive_leg: LadderLeg,
    cheap_leg: LadderLeg,
    expensive_ask: f64,
    cheap_ask: f64,
    expensive_bid: f64,
    cheap_bid: f64,
}

fn classify_legs(
    snapshot: &PairedMarketSnapshot,
    config: &CoreHedgeMmConfig,
) -> Option<LegGeometry> {
    let yes_ask = snapshot.yes_quote.best_ask.as_ref()?.price;
    let no_ask = snapshot.no_quote.best_ask.as_ref()?.price;
    let yes_bid = snapshot.yes_quote.best_bid.as_ref()?.price;
    let no_bid = snapshot.no_quote.best_bid.as_ref()?.price;

    let (expensive_leg, cheap_leg, expensive_ask, cheap_ask, expensive_bid, cheap_bid) =
        if yes_ask >= no_ask {
            (LadderLeg::Yes, LadderLeg::No, yes_ask, no_ask, yes_bid, no_bid)
        } else {
            (LadderLeg::No, LadderLeg::Yes, no_ask, yes_ask, no_bid, yes_bid)
        };

    if cheap_ask > config.cheap_leg_max_price {
        return None;
    }
    if expensive_ask < config.expensive_leg_min_price
        || expensive_ask > config.expensive_leg_max_price
    {
        return None;
    }
    if (expensive_ask - cheap_ask) < config.min_price_gap {
        return None;
    }

    Some(LegGeometry {
        expensive_leg,
        cheap_leg,
        expensive_ask,
        cheap_ask,
        expensive_bid,
        cheap_bid,
    })
}

fn build_clip<M: MarketDescriptor>(
    market: &M,
    leg: LadderLeg,
    best_bid: f64,
    best_ask: f64,
    clip_usd: f64,
    tag: &str,
    improve_ticks: f64,
    min_order_usd: f64,
    now_ms: EpochMillis,
) -> Option<OrderIntent> {
    let tick = market.tick_size().max(0.0001);
    let mut limit_price = best_bid + improve_ticks * tick;
    let max_passive = (best_ask - tick).max(tick);
    if limit_price > max_passive {
        limit_price = max_passive;
    }
    if limit_price <= 0.0 || limit_price >= 1.0 {
        return None;
    }
    if clip_usd < min_order_usd {
        return None;
    }
    let qty = (clip_usd / limit_price).max(market.min_order_size());
    let instrument_id = match leg {
        LadderLeg::Yes => market.yes_instrument_id().clone(),
        LadderLeg::No => market.no_instrument_id().clone(),
    };
    // Stable CoID per (market, leg, tag): same string across ticks. The
    // strategy gates re-emits via `should_emit` so we only submit when
    // (price, qty) actually change.
    let coid = ClientOrderId::from(format!(
        "core-hedge:{}:{:?}:{}",
        market.market_id(),
        leg,
        tag,
    ));
    let mut intent = OrderIntent::new_buy(
        coid,
        market.market_id().clone(),
        instrument_id,
        limit_price,
        qty,
        format!(
            "core_hedge {} leg={:?} px={:.4} sz_usd={:.2} (best_bid={:.4} best_ask={:.4})",
            tag, leg, limit_price, clip_usd, best_bid, best_ask
        ),
        now_ms,
    );
    intent.kind = IntentKind::Entry;
    intent.quote_level_tag = Some(format!("core-hedge:{}", tag));
    Some(intent)
}

impl<M> TradingStrategy<M> for CoreHedgeMmStrategy
where
    M: MarketDescriptor,
{
    fn name(&self) -> &'static str {
        "core_hedge_mm"
    }

    fn on_tick(&mut self, input: StrategyInput<M>) -> StrategyDecision {
        let cfg = &self.config.core_hedge;
        if !cfg.enabled {
            return StrategyDecision::Noop {
                notes: vec!["core_hedge disabled".to_string()],
            };
        }

        // Merge planner: if paired inventory exists, recycle it BEFORE
        // posting new entry intents.
        let merge_window_open = cfg
            .disable_merge_after_ms
            .is_none_or(|cutoff_ms| input.now_ms < cutoff_ms);

        if merge_window_open {
            let mut paired_qty = input.inventory.yes_qty.min(input.inventory.no_qty);
            if cfg.merge_batch_cap.is_finite() && cfg.merge_batch_cap > 0.0 {
                paired_qty = paired_qty.min(cfg.merge_batch_cap);
            }
            if paired_qty >= cfg.merge_min_qty {
                let yes_avg = input.inventory.yes_avg_cost.max(0.0);
                let no_avg = input.inventory.no_avg_cost.max(0.0);
                let expected_cost_usd = paired_qty * (yes_avg + no_avg);
                let expected_cash_usd = paired_qty;
                let merge_intent = MergeIntent {
                    command_id: ClientOrderId::new(format!(
                        "core-hedge-merge:{}:{}",
                        input.market.market_id(),
                        input.now_ms,
                    )),
                    market_id: input.market.market_id().clone(),
                    condition_id: None,
                    yes_instrument_id: input.market.yes_instrument_id().clone(),
                    no_instrument_id: input.market.no_instrument_id().clone(),
                    quantity: paired_qty,
                    expected_cash_usd,
                    expected_cost_usd,
                    expected_fee_usd: 0.0,
                    expected_gas_usd: 0.0,
                    reason: format!(
                        "core_hedge merge paired_qty={:.4} expected_net={:.4}",
                        paired_qty,
                        expected_cash_usd - expected_cost_usd
                    ),
                    created_at_ms: input.now_ms,
                };
                return StrategyDecision::Merge {
                    intent: merge_intent,
                    notes: vec![format!(
                        "core_hedge merging paired_qty={:.4} cost={:.2} cash={:.2} batch_cap={:.4}",
                        paired_qty, expected_cost_usd, expected_cash_usd, cfg.merge_batch_cap
                    )],
                };
            }
        }

        let Some(geom) = classify_legs(&input.snapshot, cfg) else {
            return StrategyDecision::Noop {
                notes: vec![format!(
                    "core_hedge geometry mismatch yes_ask={:?} no_ask={:?}",
                    input.snapshot.yes_quote.best_ask.as_ref().map(|l| l.price),
                    input.snapshot.no_quote.best_ask.as_ref().map(|l| l.price),
                )],
            };
        };

        // Current per-leg notional from inventory (cost basis, not market).
        let (expensive_qty, expensive_avg, cheap_qty, cheap_avg) = match geom.expensive_leg {
            LadderLeg::Yes => (
                input.inventory.yes_qty,
                input.inventory.yes_avg_cost,
                input.inventory.no_qty,
                input.inventory.no_avg_cost,
            ),
            LadderLeg::No => (
                input.inventory.no_qty,
                input.inventory.no_avg_cost,
                input.inventory.yes_qty,
                input.inventory.yes_avg_cost,
            ),
        };
        let expensive_notional = expensive_qty.max(0.0) * expensive_avg.max(0.0);
        let cheap_notional = cheap_qty.max(0.0) * cheap_avg.max(0.0);

        // Target allocation: expensive gets (1 / (1 + ratio)), cheap gets ratio / (1 + ratio).
        let ratio = cfg.target_hedge_ratio.clamp(0.0, 1.0);
        let denom = 1.0 + ratio;
        let expensive_target = cfg.bar_capital_usd / denom;
        let cheap_target = cfg.bar_capital_usd * ratio / denom;

        let expensive_gap = (expensive_target - expensive_notional).max(0.0);
        let cheap_gap = (cheap_target - cheap_notional).max(0.0);

        let mut intents = Vec::new();
        let mut notes = Vec::new();
        notes.push(format!(
            "core_hedge geom expensive={:?}@{:.4} cheap={:?}@{:.4} gap={:.4}",
            geom.expensive_leg,
            geom.expensive_ask,
            geom.cheap_leg,
            geom.cheap_ask,
            geom.expensive_ask - geom.cheap_ask,
        ));
        notes.push(format!(
            "core_hedge alloc expensive_notional={:.2}/{:.2} cheap_notional={:.2}/{:.2}",
            expensive_notional, expensive_target, cheap_notional, cheap_target,
        ));

        let market_id = input.market.market_id().clone();
        let cfg_min_order = cfg.min_order_usd;
        let cfg_core_clip = cfg.core_clip_usd;
        let cfg_hedge_clip = cfg.hedge_clip_usd;
        let cfg_improve = cfg.maker_improve_ticks;
        if expensive_gap >= cfg_min_order {
            let clip = cfg_core_clip.min(expensive_gap).max(cfg_min_order);
            if let Some(intent) = build_clip(
                &input.market,
                geom.expensive_leg,
                geom.expensive_bid,
                geom.expensive_ask,
                clip,
                "core",
                cfg_improve,
                cfg_min_order,
                input.now_ms,
            ) {
                if self.should_emit(&market_id, geom.expensive_leg, "core", intent.limit_price, intent.quantity) {
                    intents.push(intent);
                }
            }
        }

        if cheap_gap >= cfg_min_order {
            let clip = cfg_hedge_clip.min(cheap_gap).max(cfg_min_order);
            if let Some(intent) = build_clip(
                &input.market,
                geom.cheap_leg,
                geom.cheap_bid,
                geom.cheap_ask,
                clip,
                "hedge",
                cfg_improve,
                cfg_min_order,
                input.now_ms,
            ) {
                if self.should_emit(&market_id, geom.cheap_leg, "hedge", intent.limit_price, intent.quantity) {
                    intents.push(intent);
                }
            }
        }

        if intents.is_empty() {
            StrategyDecision::Noop { notes }
        } else {
            StrategyDecision::QuoteSet { intents, notes }
        }
    }

    fn on_fill(&mut self, _input: StrategyFillInput<M>) -> StrategyDecision {
        // P2: trigger merge planner when paired_qty crosses threshold.
        StrategyDecision::Noop {
            notes: vec!["core_hedge on_fill not yet wired".to_string()],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::BookLevel;
    use crate::types::QuoteSnapshot;

    fn snap_with_quotes(yes_bid: f64, yes_ask: f64, no_bid: f64, no_ask: f64) -> PairedMarketSnapshot {
        let yes_quote = QuoteSnapshot {
            best_bid: Some(BookLevel::new(yes_bid, 100.0)),
            best_ask: Some(BookLevel::new(yes_ask, 100.0)),
            ..QuoteSnapshot::default()
        };
        let no_quote = QuoteSnapshot {
            best_bid: Some(BookLevel::new(no_bid, 100.0)),
            best_ask: Some(BookLevel::new(no_ask, 100.0)),
            ..QuoteSnapshot::default()
        };
        PairedMarketSnapshot {
            market_id: "m1".into(),
            yes_instrument_id: "yes".into(),
            no_instrument_id: "no".into(),
            yes_quote,
            no_quote,
        }
    }

    #[test]
    fn classifier_picks_yes_as_favorite_when_yes_ask_is_higher() {
        let cfg = CoreHedgeMmConfig::default();
        let snap = snap_with_quotes(0.69, 0.70, 0.29, 0.30);
        let geom = classify_legs(&snap, &cfg).expect("geometry should fit");
        assert_eq!(geom.expensive_leg, LadderLeg::Yes);
        assert_eq!(geom.cheap_leg, LadderLeg::No);
        assert!((geom.expensive_ask - 0.70).abs() < 1e-9);
        assert!((geom.cheap_ask - 0.30).abs() < 1e-9);
    }

    #[test]
    fn classifier_rejects_when_gap_too_small() {
        let cfg = CoreHedgeMmConfig::default();
        let snap = snap_with_quotes(0.49, 0.50, 0.49, 0.50);
        assert!(classify_legs(&snap, &cfg).is_none());
    }

    #[test]
    fn classifier_rejects_when_cheap_leg_too_expensive() {
        let cfg = CoreHedgeMmConfig::default();
        let snap = snap_with_quotes(0.59, 0.60, 0.39, 0.40);
        assert!(classify_legs(&snap, &cfg).is_none());
    }
}
