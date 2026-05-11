//! Late-favorite directional accumulation with conditional convex tail.
//!
//! This version keeps the late-booking behavior from whale analysis but adds
//! explicit spot-direction gating and deterministic clip ramping in the final
//! seconds.

use crate::core::types::{ClientOrderId, EpochMillis, IntentKind, OrderIntent};
use crate::market_making::pairing::types::{LadderLeg, PairedMarketSnapshot};
use crate::markets::MarketDescriptor;
use crate::strategies::traits::{StrategyFillInput, StrategyInput, TradingStrategy};
use crate::types::StrategyDecision;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FavoriteClimbConfig {
    pub enabled: bool,
    /// Minimum favorite ask to qualify as “loadable favorite”.
    pub min_favorite_ask: f64,
    /// Maximum favorite ask to qualify (avoid zero edge near 0.99+).
    pub max_favorite_ask: f64,
    /// Only fire in the last `window_sec` of bar.
    pub window_sec: u64,
    /// Optional bar-relative start fraction.
    pub start_frac: f64,
    /// Base clip size, used as floor before ramping.
    pub clip_usd: f64,
    /// Hard cap on directional notional per market.
    pub max_load_usd: f64,
    pub maker_improve_ticks: f64,
    pub min_order_usd: f64,
    /// Spot filter: require BTC move >= this threshold (bps) in the side of
    /// the loaded favorite.
    pub spot_filter_bps: f64,
    /// If true, skip favorite loading when spot filter does not match.
    pub require_spot_match: bool,
    /// Optional hard cutoff after which this phase is disabled.
    pub disable_after_ms: Option<EpochMillis>,
}

impl Default for FavoriteClimbConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_favorite_ask: 0.90,
            max_favorite_ask: 0.99,
            window_sec: 120,
            start_frac: 0.0,
            clip_usd: 20.0,
            max_load_usd: 200.0,
            maker_improve_ticks: 0.0,
            min_order_usd: 1.0,
            spot_filter_bps: 10.0,
            require_spot_match: true,
            disable_after_ms: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ConvexTailConfig {
    pub enabled: bool,
    /// Maximum cheap-leg ask that qualifies.
    pub max_cheap_ask: f64,
    /// Only fire in the final `window_sec` of the bar.
    pub window_sec: u64,
    /// Optional bar-relative start fraction.
    pub start_frac: f64,
    pub clip_usd: f64,
    pub max_load_usd: f64,
    pub maker_improve_ticks: f64,
    pub min_order_usd: f64,
    /// Optional hard cutoff after which this phase is disabled.
    pub disable_after_ms: Option<EpochMillis>,
}

impl Default for ConvexTailConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_cheap_ask: 0.02,
            window_sec: 60,
            start_frac: 0.0,
            clip_usd: 2.0,
            max_load_usd: 15.0,
            maker_improve_ticks: 0.0,
            min_order_usd: 0.5,
            disable_after_ms: None,
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
            (
                LadderLeg::Yes,
                LadderLeg::No,
                yes_ask,
                yes_bid,
                no_ask,
                no_bid,
            )
        } else {
            (
                LadderLeg::No,
                LadderLeg::Yes,
                no_ask,
                no_bid,
                yes_ask,
                yes_bid,
            )
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
        "late-fav:{}:{}:{:?}",
        tag,
        market.market_id(),
        leg,
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
    intent.quote_level_tag = Some(format!("late-fav-{tag}"));
    intent
}

fn phase_window_ms(window_sec: u64, start_frac: f64, bar_window_ms: u64) -> u64 {
    let absolute_ms = window_sec.saturating_mul(1_000);
    let fractional_ms = if start_frac.is_finite() && (0.0..1.0).contains(&start_frac) {
        ((1.0 - start_frac) * bar_window_ms as f64).round() as u64
    } else {
        0
    };
    absolute_ms.max(fractional_ms)
}

fn directional_exposure_usd(favorite_qty: f64, other_qty: f64, px: f64) -> f64 {
    let unmatched = (favorite_qty - other_qty).abs().max(0.0);
    unmatched * px.max(0.0)
}

fn remaining_directional_load_usd(
    favorite_qty: f64,
    other_qty: f64,
    px: f64,
    max_load_usd: f64,
) -> f64 {
    (max_load_usd - directional_exposure_usd(favorite_qty, other_qty, px)).max(0.0)
}

fn late_climb_clip_usd(cfg: &FavoriteClimbConfig, remaining_ms: u64) -> f64 {
    let remaining_sec = (remaining_ms as f64) / 1000.0;
    let ramp: f64 = if remaining_sec > 60.0 {
        100.0
    } else if remaining_sec > 30.0 {
        130.0
    } else if remaining_sec > 15.0 {
        200.0
    } else if remaining_sec > 5.0 {
        240.0
    } else {
        250.0
    };
    ramp.max(cfg.clip_usd)
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
        let climb_enabled = climb_cfg
            .disable_after_ms
            .map(|disable_ms| input.now_ms < disable_ms)
            .unwrap_or(true);
        let tail_enabled = tail_cfg
            .disable_after_ms
            .map(|disable_ms| input.now_ms < disable_ms)
            .unwrap_or(true);
        if !climb_enabled && !tail_enabled {
            return StrategyDecision::Noop {
                notes: vec![format!(
                    "late_favorite all phases disabled now_ms={} climb_cutoff={:?} tail_cutoff={:?}",
                    input.now_ms, climb_cfg.disable_after_ms, tail_cfg.disable_after_ms
                )],
            };
        }

        // Phase A — favorite climb.
        let climb_window_ms = phase_window_ms(
            climb_cfg.window_sec,
            climb_cfg.start_frac,
            input.market.window_ms(),
        );
        if climb_cfg.enabled
            && climb_enabled
            && remaining_ms <= climb_window_ms
            && legs.favorite_ask >= climb_cfg.min_favorite_ask
            && legs.favorite_ask <= climb_cfg.max_favorite_ask
        {
            let spot_bps = input.btc_regime.return_120s_bps;
            let direction_ok = if let Some(r) = spot_bps {
                if climb_cfg.require_spot_match {
                    let threshold = climb_cfg.spot_filter_bps.abs();
                    match legs.favorite_leg {
                        LadderLeg::Yes => r >= threshold,
                        LadderLeg::No => r <= -threshold,
                    }
                } else {
                    true
                }
            } else {
                !climb_cfg.require_spot_match
            };

            if !direction_ok {
                notes.push(format!(
                    "late_favorite blocked by spot direction filter favorite={:?} return_120s_bps={:?}",
                    legs.favorite_leg, spot_bps,
                ));
            } else {
                let (favorite_qty, other_qty) = match legs.favorite_leg {
                    LadderLeg::Yes => (input.inventory.yes_qty, input.inventory.no_qty),
                    LadderLeg::No => (input.inventory.no_qty, input.inventory.yes_qty),
                };
                let current_exposure_usd = directional_exposure_usd(
                    favorite_qty,
                    other_qty,
                    legs.favorite_ask,
                );
                let remaining_load = remaining_directional_load_usd(
                    favorite_qty,
                    other_qty,
                    legs.favorite_ask,
                    climb_cfg.max_load_usd,
                );
                if remaining_load >= climb_cfg.min_order_usd {
                    if let Some(px) = maker_limit_price(
                        legs.favorite_bid,
                        legs.favorite_ask,
                        tick,
                        climb_cfg.maker_improve_ticks,
                    ) {
                        let clip = late_climb_clip_usd(climb_cfg, remaining_ms)
                            .min(remaining_load)
                            .max(climb_cfg.min_order_usd);
                        let qty = (clip / px).max(input.market.min_order_size());
                        let reason = format!(
                            "late_favorite climb leg={:?} px={:.4} ask={:.4} clip_usd={:.2} cumulative={:.2}/{:.2} remaining_ms={remaining_ms}",
                            legs.favorite_leg,
                            px,
                            legs.favorite_ask,
                            clip,
                            current_exposure_usd,
                            climb_cfg.max_load_usd,
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
        }

        // Phase B — convex tail hedge.
        if tail_cfg.enabled
            && tail_enabled
            && remaining_ms <= phase_window_ms(tail_cfg.window_sec, tail_cfg.start_frac, input.market.window_ms())
            && legs.cheap_ask <= tail_cfg.max_cheap_ask
        {
            let (cheap_qty, other_qty) = match legs.cheap_leg {
                LadderLeg::Yes => (input.inventory.yes_qty, input.inventory.no_qty),
                LadderLeg::No => (input.inventory.no_qty, input.inventory.yes_qty),
            };
            let current_exposure_usd =
                directional_exposure_usd(cheap_qty, other_qty, legs.cheap_ask);
            let remaining_load = remaining_directional_load_usd(
                cheap_qty,
                other_qty,
                legs.cheap_ask,
                tail_cfg.max_load_usd,
            );
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
                        "late_favorite tail leg={:?} px={:.4} ask={:.4} clip_usd={:.2} cumulative={:.2}/{:.2} remaining_ms={remaining_ms}",
                        legs.cheap_leg,
                            px,
                            legs.cheap_ask,
                            clip,
                            current_exposure_usd,
                            tail_cfg.max_load_usd,
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
        // Bid + 0 ticks = 0.96, but max_passive = ask - 1 tick = 0.96.
        assert!((px - 0.96).abs() < 1e-9);
    }

    #[test]
    fn phase_window_supports_bar_relative_late_windows() {
        assert_eq!(phase_window_ms(0, 0.90, 300_000), 30_000);
        assert_eq!(phase_window_ms(0, 0.90, 900_000), 90_000);
        assert_eq!(phase_window_ms(60, 0.0, 300_000), 60_000);
    }

    #[test]
    fn remaining_directional_load_caps_to_unmatched_exposure() {
        let favorite_qty = 400.0;
        let other_qty = 300.0;
        let px = 0.98;

        assert!((directional_exposure_usd(favorite_qty, other_qty, px) - 98.0).abs() < 1e-9);
        assert!((remaining_directional_load_usd(favorite_qty, other_qty, px, 150.0) - 52.0).abs() < 1e-9);
        assert!((remaining_directional_load_usd(favorite_qty, other_qty, px, 90.0)).abs() < 1e-9);
    }
}
