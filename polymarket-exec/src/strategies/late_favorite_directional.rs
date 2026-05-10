//! Late-favorite directional accumulation with convex tail hedge,
//! modeled on bonereaper's observed live behavior.
//!
//! Bonereaper structure (live evidence 2026-05-10, eth_5m + btc_5m):
//!
//! Phase A — late-bar favorite climb (last ~3-4 min of bar):
//!   load the expensive leg progressively as its ask rises 0.88 → 0.97.
//!   Many small clips, each chasing the climbing price.
//!   Example (eth 5:35-5:40): Down @ 0.88, 0.89, 0.90, 0.91, 0.92,
//!   0.93, 0.94, 0.95, 0.96, 0.96, 0.97, 0.97 — ~$240 over ~3 min.
//!
//! Phase B — final-tick convex tail hedge:
//!   when the cheap leg compresses to ~0.01, buy massive share count
//!   for a tiny dollar amount. Captures the 3% upset case for ~30x
//!   leverage on the tail outlay.
//!   Same window: Up @ 0.01, 5 clips totaling 886 shares for $9.41.
//!
//! Combined economics: 97% case → near-flat (paired Down+Up close to
//! $1.00 sum). 3% case → +$600+ from convex tail. Edge from maker
//! rebates and probability mispricing.

use crate::core::types::{ClientOrderId, EpochMillis, IntentKind, OrderIntent};
use crate::market_making::pairing::types::{LadderLeg, PairedMarketSnapshot};
use crate::markets::MarketDescriptor;
use crate::strategies::traits::{StrategyFillInput, StrategyInput, TradingStrategy};
use crate::types::StrategyDecision;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FavoriteClimbConfig {
    pub enabled: bool,
    /// Minimum favorite ask to qualify as "loadable favorite".
    pub min_favorite_ask: f64,
    /// Maximum favorite ask (no point loading at 0.999 — no edge left).
    pub max_favorite_ask: f64,
    /// Only fire in the last `window_sec` of the bar.
    pub window_sec: u64,
    /// Per-tick clip size (USD) on the favorite leg.
    pub clip_usd: f64,
    /// Hard cap on total directional notional per market.
    pub max_load_usd: f64,
    pub maker_improve_ticks: f64,
    pub min_order_usd: f64,
}

impl Default for FavoriteClimbConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_favorite_ask: 0.85,
            max_favorite_ask: 0.99,
            window_sec: 180,
            clip_usd: 20.0,
            max_load_usd: 200.0,
            maker_improve_ticks: 0.0,
            min_order_usd: 1.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ConvexTailConfig {
    pub enabled: bool,
    /// Maximum cheap-leg ask that qualifies. ≤ 0.02 captures the
    /// "compressed to a penny" tier where bonereaper buys.
    pub max_cheap_ask: f64,
    /// Only fire in the final `window_sec` of the bar.
    pub window_sec: u64,
    /// Per-tick clip size (USD) on the cheap leg.
    pub clip_usd: f64,
    /// Hard cap on total convex tail notional per market.
    pub max_load_usd: f64,
    pub maker_improve_ticks: f64,
    pub min_order_usd: f64,
}

impl Default for ConvexTailConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_cheap_ask: 0.02,
            window_sec: 60,
            clip_usd: 2.0,
            max_load_usd: 15.0,
            maker_improve_ticks: 0.0,
            min_order_usd: 0.5,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LateFavoriteStrategyConfig {
    pub favorite_climb: FavoriteClimbConfig,
    pub convex_tail: ConvexTailConfig,
}

impl Default for LateFavoriteStrategyConfig {
    fn default() -> Self {
        Self {
            favorite_climb: FavoriteClimbConfig::default(),
            convex_tail: ConvexTailConfig::default(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct LateFavoriteStrategy {
    config: LateFavoriteStrategyConfig,
}

impl LateFavoriteStrategy {
    pub fn new(config: LateFavoriteStrategyConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &LateFavoriteStrategyConfig {
        &self.config
    }
}

#[derive(Clone, Copy, Debug)]
struct LegQuotes {
    favorite_leg: LadderLeg,
    cheap_leg: LadderLeg,
    favorite_ask: f64,
    favorite_bid: f64,
    cheap_ask: f64,
    cheap_bid: f64,
}

fn read_legs(snapshot: &PairedMarketSnapshot) -> Option<LegQuotes> {
    let yes_ask = snapshot.yes_quote.best_ask.as_ref()?.price;
    let no_ask = snapshot.no_quote.best_ask.as_ref()?.price;
    let yes_bid = snapshot.yes_quote.best_bid.as_ref()?.price;
    let no_bid = snapshot.no_quote.best_bid.as_ref()?.price;
    let (favorite_leg, cheap_leg, favorite_ask, favorite_bid, cheap_ask, cheap_bid) =
        if yes_ask >= no_ask {
            (LadderLeg::Yes, LadderLeg::No, yes_ask, yes_bid, no_ask, no_bid)
        } else {
            (LadderLeg::No, LadderLeg::Yes, no_ask, no_bid, yes_ask, yes_bid)
        };
    Some(LegQuotes {
        favorite_leg,
        cheap_leg,
        favorite_ask,
        favorite_bid,
        cheap_ask,
        cheap_bid,
    })
}

fn maker_limit_price(bid: f64, ask: f64, tick: f64, improve_ticks: f64) -> Option<f64> {
    let mut px = bid + improve_ticks * tick;
    let max_passive = (ask - tick).max(tick);
    if px > max_passive {
        px = max_passive;
    }
    if px <= 0.0 || px >= 1.0 {
        return None;
    }
    Some(px)
}

fn build_intent<M: MarketDescriptor>(
    market: &M,
    leg: LadderLeg,
    limit_price: f64,
    qty: f64,
    tag: &str,
    reason: String,
    now_ms: EpochMillis,
) -> OrderIntent {
    let instrument_id = match leg {
        LadderLeg::Yes => market.yes_instrument_id().clone(),
        LadderLeg::No => market.no_instrument_id().clone(),
    };
    let coid = ClientOrderId::from(format!(
        "late-fav:{}:{}:{:?}:{}",
        tag,
        market.market_id(),
        leg,
        now_ms / 1000
    ));
    let mut intent = OrderIntent::new_buy(
        coid,
        market.market_id().clone(),
        instrument_id,
        limit_price,
        qty,
        reason,
        now_ms,
    );
    intent.kind = IntentKind::Entry;
    intent.quote_level_tag = Some(format!("late-fav-{}", tag));
    intent
}

impl<M> TradingStrategy<M> for LateFavoriteStrategy
where
    M: MarketDescriptor,
{
    fn name(&self) -> &'static str {
        "late_favorite_directional"
    }

    fn on_tick(&mut self, input: StrategyInput<M>) -> StrategyDecision {
        let climb_cfg = &self.config.favorite_climb;
        let tail_cfg = &self.config.convex_tail;
        if !climb_cfg.enabled && !tail_cfg.enabled {
            return StrategyDecision::Noop {
                notes: vec!["late_favorite all phases disabled".to_string()],
            };
        }

        let remaining_ms = input
            .market
            .time_remaining_ms(input.now_ms)
            .unwrap_or(input.market.window_ms());
        let Some(legs) = read_legs(&input.snapshot) else {
            return StrategyDecision::Noop {
                notes: vec!["late_favorite no quotes".to_string()],
            };
        };
        let tick = input.market.tick_size().max(0.0001);
        let mut intents = Vec::new();
        let mut notes = Vec::new();

        // Phase A — favorite climb.
        let climb_window_ms = climb_cfg.window_sec.saturating_mul(1_000);
        if climb_cfg.enabled
            && remaining_ms <= climb_window_ms
            && legs.favorite_ask >= climb_cfg.min_favorite_ask
            && legs.favorite_ask <= climb_cfg.max_favorite_ask
        {
            let (qty_have, avg_have) = match legs.favorite_leg {
                LadderLeg::Yes => (input.inventory.yes_qty, input.inventory.yes_avg_cost),
                LadderLeg::No => (input.inventory.no_qty, input.inventory.no_avg_cost),
            };
            let current_notional = qty_have.max(0.0) * avg_have.max(0.0);
            let remaining_load = (climb_cfg.max_load_usd - current_notional).max(0.0);
            if remaining_load >= climb_cfg.min_order_usd {
                if let Some(px) = maker_limit_price(
                    legs.favorite_bid,
                    legs.favorite_ask,
                    tick,
                    climb_cfg.maker_improve_ticks,
                ) {
                    let clip = climb_cfg
                        .clip_usd
                        .min(remaining_load)
                        .max(climb_cfg.min_order_usd);
                    let qty = (clip / px).max(input.market.min_order_size());
                    let reason = format!(
                        "late_favorite climb leg={:?} px={:.4} ask={:.4} clip_usd={:.2} cumulative={:.2}/{:.2} remaining_ms={}",
                        legs.favorite_leg,
                        px,
                        legs.favorite_ask,
                        clip,
                        current_notional,
                        climb_cfg.max_load_usd,
                        remaining_ms,
                    );
                    notes.push(reason.clone());
                    intents.push(build_intent(
                        &input.market,
                        legs.favorite_leg,
                        px,
                        qty,
                        "climb",
                        reason,
                        input.now_ms,
                    ));
                }
            }
        }

        // Phase B — convex tail hedge.
        let tail_window_ms = tail_cfg.window_sec.saturating_mul(1_000);
        if tail_cfg.enabled
            && remaining_ms <= tail_window_ms
            && legs.cheap_ask <= tail_cfg.max_cheap_ask
        {
            let (qty_have, avg_have) = match legs.cheap_leg {
                LadderLeg::Yes => (input.inventory.yes_qty, input.inventory.yes_avg_cost),
                LadderLeg::No => (input.inventory.no_qty, input.inventory.no_avg_cost),
            };
            let current_notional = qty_have.max(0.0) * avg_have.max(0.0);
            let remaining_load = (tail_cfg.max_load_usd - current_notional).max(0.0);
            if remaining_load >= tail_cfg.min_order_usd {
                if let Some(px) = maker_limit_price(
                    legs.cheap_bid,
                    legs.cheap_ask,
                    tick,
                    tail_cfg.maker_improve_ticks,
                ) {
                    let clip = tail_cfg
                        .clip_usd
                        .min(remaining_load)
                        .max(tail_cfg.min_order_usd);
                    let qty = (clip / px).max(input.market.min_order_size());
                    let reason = format!(
                        "late_favorite tail leg={:?} px={:.4} ask={:.4} clip_usd={:.2} cumulative={:.2}/{:.2} remaining_ms={}",
                        legs.cheap_leg,
                        px,
                        legs.cheap_ask,
                        clip,
                        current_notional,
                        tail_cfg.max_load_usd,
                        remaining_ms,
                    );
                    notes.push(reason.clone());
                    intents.push(build_intent(
                        &input.market,
                        legs.cheap_leg,
                        px,
                        qty,
                        "tail",
                        reason,
                        input.now_ms,
                    ));
                }
            }
        }

        if intents.is_empty() {
            StrategyDecision::Noop {
                notes: if notes.is_empty() {
                    vec![format!(
                        "late_favorite no fire favorite_ask={:.4} cheap_ask={:.4} remaining_ms={}",
                        legs.favorite_ask, legs.cheap_ask, remaining_ms
                    )]
                } else {
                    notes
                },
            }
        } else {
            StrategyDecision::QuoteSet { intents, notes }
        }
    }

    fn on_fill(&mut self, _input: StrategyFillInput<M>) -> StrategyDecision {
        StrategyDecision::Noop {
            notes: vec!["late_favorite on_fill noop".to_string()],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::BookLevel;
    use crate::types::QuoteSnapshot;

    fn snap(yes_bid: f64, yes_ask: f64, no_bid: f64, no_ask: f64) -> PairedMarketSnapshot {
        PairedMarketSnapshot {
            market_id: "m".into(),
            yes_instrument_id: "yes".into(),
            no_instrument_id: "no".into(),
            yes_quote: QuoteSnapshot {
                best_bid: Some(BookLevel::new(yes_bid, 100.0)),
                best_ask: Some(BookLevel::new(yes_ask, 100.0)),
                ..QuoteSnapshot::default()
            },
            no_quote: QuoteSnapshot {
                best_bid: Some(BookLevel::new(no_bid, 100.0)),
                best_ask: Some(BookLevel::new(no_ask, 100.0)),
                ..QuoteSnapshot::default()
            },
        }
    }

    #[test]
    fn read_legs_picks_higher_ask_as_favorite() {
        let s = snap(0.96, 0.97, 0.02, 0.03);
        let l = read_legs(&s).unwrap();
        assert_eq!(l.favorite_leg, LadderLeg::Yes);
        assert_eq!(l.cheap_leg, LadderLeg::No);
        assert!((l.favorite_ask - 0.97).abs() < 1e-9);
        assert!((l.cheap_ask - 0.03).abs() < 1e-9);
    }

    #[test]
    fn maker_limit_price_caps_below_ask() {
        let px = maker_limit_price(0.96, 0.97, 0.01, 0.0).unwrap();
        // Bid + 0 ticks = 0.96, but max_passive = ask - 1 tick = 0.96, so 0.96.
        assert!((px - 0.96).abs() < 1e-9);
    }
}
