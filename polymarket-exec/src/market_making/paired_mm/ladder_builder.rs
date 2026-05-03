//! Signal-driven paired ladder builder.
//!
//! The ladder is anchored on Stoikov reservation prices and changes shape
//! with regime, visible book depth, time-to-bar-end, and inventory imbalance.
//! It is pure: no venue calls, no state mutation, no runtime side effects.

use crate::market_making::paired_mm::risk_boundary::filter_entry_intents;
use crate::market_making::paired_mm::stoikov::{stoikov_reservation_price, StoikovParams};
use crate::market_making::pairing::pair_cost_tracker::PairCostTracker;
use crate::market_making::pairing::types::{
    LadderLeg, LadderRegime, PairedInventorySnapshot, PairedMarketSnapshot, RunningInventoryCaps,
};
use crate::markets::MarketDescriptor;
use crate::signals::{
    BtcRegime, BtcRegimeSnapshot, FairValueEstimate, FairValueModel, IncentiveSignal,
};
use crate::types::{ClientOrderId, EpochMillis, IntentKind, MmQuoteKind, OrderIntent, TradeSide};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FairValueAnchoringConfig {
    pub max_model_divergence: f64,
    pub model_influence_weight: f64,
}

impl Default for FairValueAnchoringConfig {
    fn default() -> Self {
        Self {
            max_model_divergence: 0.10,
            model_influence_weight: 0.30,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct LadderConfig {
    pub min_depth: usize,
    pub max_depth: usize,
    pub low_vol_depth: usize,
    pub high_vol_depth: usize,
    pub late_bar_depth: usize,
    pub low_vol_spacing_ticks: f64,
    pub normal_spacing_ticks: f64,
    pub high_vol_spacing_ticks: f64,
    pub imbalanced_spacing_multiplier: f64,
    pub late_bar_cutoff_ms: u64,
    pub base_clip_usd: f64,
    pub fractional_kelly: f64,
    pub max_clip_usd: f64,
    pub level_multipliers: Vec<f64>,
    pub stoikov: StoikovParams,
    pub caps: RunningInventoryCaps,
    pub incentives: IncentiveSignal,
    pub fair_value_anchoring: FairValueAnchoringConfig,
}

impl Default for LadderConfig {
    fn default() -> Self {
        Self {
            min_depth: 2,
            max_depth: 8,
            low_vol_depth: 6,
            high_vol_depth: 3,
            late_bar_depth: 2,
            low_vol_spacing_ticks: 1.0,
            normal_spacing_ticks: 2.0,
            high_vol_spacing_ticks: 4.0,
            imbalanced_spacing_multiplier: 2.0,
            late_bar_cutoff_ms: 60_000,
            base_clip_usd: 10.0,
            fractional_kelly: 0.20,
            max_clip_usd: 25.0,
            level_multipliers: vec![1.0, 1.8, 2.5, 3.5, 4.0, 4.5, 5.0, 5.5],
            stoikov: StoikovParams::default(),
            caps: RunningInventoryCaps::default(),
            incentives: IncentiveSignal::default(),
            fair_value_anchoring: FairValueAnchoringConfig::default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct LadderDiagnostics {
    pub regime: LadderRegime,
    pub depth: usize,
    pub spacing_ticks: f64,
    pub yes_reservation: f64,
    pub no_reservation: f64,
    pub base_clip_usd: f64,
    pub suppressed_yes: bool,
    pub suppressed_no: bool,
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LadderBuildResult {
    pub intents: Vec<OrderIntent>,
    pub diagnostics: LadderDiagnostics,
}

pub fn build_ladder<M: MarketDescriptor>(
    market: &M,
    snapshot: &PairedMarketSnapshot,
    inventory: &PairedInventorySnapshot,
    fair_value: &FairValueEstimate,
    btc_regime: &BtcRegimeSnapshot,
    pair_cost: &PairCostTracker,
    config: &LadderConfig,
    now_ms: EpochMillis,
) -> LadderBuildResult {
    let tau = market.time_remaining_fraction(now_ms);
    let vol_bps = btc_regime.realized_vol_5m_bps.unwrap_or_default().max(0.0);
    let market_regime = btc_regime.regime();
    let remaining_ms = market
        .time_remaining_ms(now_ms)
        .unwrap_or(market.window_ms());
    let absolute_imbalance_qty = inventory.side_imbalance_qty();
    let imbalance = if absolute_imbalance_qty >= market.min_order_size().max(1.0) {
        inventory.imbalance_ratio().max(pair_cost.imbalance_ratio())
    } else {
        0.0
    };
    let visible_depth = snapshot.total_visible_levels();

    let (ladder_regime, depth, spacing_ticks) = dynamic_ladder_shape(
        market_regime,
        visible_depth,
        remaining_ms,
        imbalance,
        config,
    );

    let yes_fair = usable_fair(fair_value, LadderLeg::Yes, snapshot, config);
    let no_fair = usable_fair(fair_value, LadderLeg::No, snapshot, config);

    let yes_reservation = stoikov_reservation_price(
        yes_fair,
        inventory.net_for_leg(LadderLeg::Yes),
        config.stoikov,
        tau,
        vol_bps,
    );
    let no_reservation = stoikov_reservation_price(
        no_fair,
        inventory.net_for_leg(LadderLeg::No),
        config.stoikov,
        tau,
        vol_bps,
    );

    let mut notes = Vec::new();
    if matches!(fair_value.model, FairValueModel::NoSignal(_)) {
        notes.push("fair-value no-signal: using book midpoint fallback".to_string());
    }
    if inventory.gross_cost_usd() >= config.caps.max_gross_cost_usd {
        notes.push(format!(
            "ladder suppressed: gross cost cap reached gross={:.4} cap={:.4}",
            inventory.gross_cost_usd(),
            config.caps.max_gross_cost_usd
        ));
    }

    let base_clip_usd = kelly_clip_size(inventory, fair_value, config, ladder_regime);
    let suppress_yes = should_suppress_leg(LadderLeg::Yes, inventory, config);
    let suppress_no = should_suppress_leg(LadderLeg::No, inventory, config);

    let mut intents = Vec::with_capacity(depth * 2);
    if inventory.gross_cost_usd() < config.caps.max_gross_cost_usd {
        let mut yes_intents = Vec::with_capacity(depth);
        let mut no_intents = Vec::with_capacity(depth);
        append_leg_ladder(
            &mut yes_intents,
            market,
            LadderLeg::Yes,
            &snapshot.yes_quote,
            yes_reservation,
            depth,
            spacing_ticks,
            base_clip_usd,
            suppress_yes,
            config,
            now_ms,
        );
        append_leg_ladder(
            &mut no_intents,
            market,
            LadderLeg::No,
            &snapshot.no_quote,
            no_reservation,
            depth,
            spacing_ticks,
            base_clip_usd,
            suppress_no,
            config,
            now_ms,
        );
        normalize_paired_entry_quantities(
            &mut yes_intents,
            &mut no_intents,
            market.min_order_size(),
        );
        interleave_leg_ladders(&mut intents, yes_intents, no_intents);
    }

    let (intents, risk_rejects) = filter_entry_intents(inventory, intents, &config.caps);
    for reject in risk_rejects {
        notes.push(reject.note);
    }

    LadderBuildResult {
        intents,
        diagnostics: LadderDiagnostics {
            regime: ladder_regime,
            depth,
            spacing_ticks,
            yes_reservation,
            no_reservation,
            base_clip_usd,
            suppressed_yes: suppress_yes,
            suppressed_no: suppress_no,
            notes,
        },
    }
}

fn dynamic_ladder_shape(
    btc_regime: Option<BtcRegime>,
    visible_depth: usize,
    remaining_ms: u64,
    imbalance_ratio: f64,
    config: &LadderConfig,
) -> (LadderRegime, usize, f64) {
    if remaining_ms <= config.late_bar_cutoff_ms {
        return (
            LadderRegime::LateBar,
            config
                .late_bar_depth
                .clamp(config.min_depth, config.max_depth),
            config.high_vol_spacing_ticks,
        );
    }

    if imbalance_ratio >= 0.65 {
        let depth = config
            .high_vol_depth
            .clamp(config.min_depth, config.max_depth);
        return (
            LadderRegime::InventoryImbalanced,
            depth,
            config.high_vol_spacing_ticks * config.imbalanced_spacing_multiplier.max(1.0),
        );
    }

    match btc_regime {
        Some(BtcRegime::Flat | BtcRegime::Whipsaw) => {
            let depth_bonus = usize::from(visible_depth >= 12);
            (
                LadderRegime::LowVolOscillating,
                (config.low_vol_depth + depth_bonus).clamp(config.min_depth, config.max_depth),
                config.low_vol_spacing_ticks.max(0.5),
            )
        }
        Some(BtcRegime::DirectionalSmooth | BtcRegime::TrendingVolatile) => (
            LadderRegime::DirectionalDefensive,
            config
                .high_vol_depth
                .clamp(config.min_depth, config.max_depth),
            config
                .high_vol_spacing_ticks
                .max(config.normal_spacing_ticks),
        ),
        None => (
            LadderRegime::Normal,
            ((config.min_depth + config.max_depth) / 2).clamp(config.min_depth, config.max_depth),
            config.normal_spacing_ticks,
        ),
    }
}

fn usable_fair(
    fair_value: &FairValueEstimate,
    leg: LadderLeg,
    snapshot: &PairedMarketSnapshot,
    config: &LadderConfig,
) -> f64 {
    let quote = match leg {
        LadderLeg::Yes => &snapshot.yes_quote,
        LadderLeg::No => &snapshot.no_quote,
    };
    let model_fair = match leg {
        LadderLeg::Yes => fair_value.p_up,
        LadderLeg::No => fair_value.p_down,
    };
    if model_fair.is_finite() && model_fair > 0.0 && model_fair < 1.0 {
        if let Some(book_mid) = quote.mid_price() {
            return anchored_model_fair(model_fair, book_mid, config.fair_value_anchoring);
        }
        return model_fair.clamp(0.01, 0.99);
    }
    quote.mid_price().unwrap_or(0.5).clamp(0.01, 0.99)
}

fn anchored_model_fair(model_fair: f64, book_mid: f64, config: FairValueAnchoringConfig) -> f64 {
    let weight = config.model_influence_weight.clamp(0.0, 1.0);
    let max_divergence = config.max_model_divergence.clamp(0.0, 0.99);
    let blended = book_mid + weight * (model_fair - book_mid);
    blended
        .clamp(book_mid - max_divergence, book_mid + max_divergence)
        .clamp(0.01, 0.99)
}

fn kelly_clip_size(
    inventory: &PairedInventorySnapshot,
    fair_value: &FairValueEstimate,
    config: &LadderConfig,
    regime: LadderRegime,
) -> f64 {
    let confidence = (fair_value.p_up - 0.5)
        .abs()
        .max((fair_value.p_down - 0.5).abs());
    let capital = inventory.equity_usd.max(inventory.free_cash_usd).max(0.0);
    let kelly_component = capital * confidence * config.fractional_kelly.max(0.0);
    let regime_scale = match regime {
        LadderRegime::LowVolOscillating => 1.0,
        LadderRegime::Normal => 0.8,
        LadderRegime::DirectionalDefensive => 0.5,
        LadderRegime::LateBar => 0.35,
        LadderRegime::InventoryImbalanced => 0.35,
    };
    config
        .base_clip_usd
        .max(kelly_component)
        .min(config.max_clip_usd)
        * regime_scale
}

fn should_suppress_leg(
    leg: LadderLeg,
    inventory: &PairedInventorySnapshot,
    config: &LadderConfig,
) -> bool {
    if inventory.side_imbalance_qty() < config.caps.max_side_imbalance_qty {
        return false;
    }
    match leg {
        LadderLeg::Yes => inventory.yes_qty > inventory.no_qty,
        LadderLeg::No => inventory.no_qty > inventory.yes_qty,
    }
}

fn append_leg_ladder<M: MarketDescriptor>(
    intents: &mut Vec<OrderIntent>,
    market: &M,
    leg: LadderLeg,
    quote: &crate::types::QuoteSnapshot,
    reservation: f64,
    depth: usize,
    spacing_ticks: f64,
    base_clip_usd: f64,
    suppress: bool,
    config: &LadderConfig,
    now_ms: EpochMillis,
) {
    if suppress {
        return;
    }

    let tick_size = market.tick_size().max(0.0001);
    let instrument_id = match leg {
        LadderLeg::Yes => market.yes_instrument_id().clone(),
        LadderLeg::No => market.no_instrument_id().clone(),
    };
    let leg_tag = match leg {
        LadderLeg::Yes => "yes",
        LadderLeg::No => "no",
    };

    for level in 0..depth {
        let multiplier = config
            .level_multipliers
            .get(level)
            .copied()
            .unwrap_or_else(|| 1.0 + level as f64);
        let raw_price = reservation - (level as f64 * spacing_ticks * tick_size);
        let maker_bid_cap = quote
            .best_ask
            .as_ref()
            .map(|ask| ask.price - tick_size)
            .unwrap_or(1.0 - tick_size);
        let limit_price = align_down_to_tick(raw_price.min(maker_bid_cap), tick_size)
            .clamp(tick_size, 1.0 - tick_size);
        if limit_price >= quote.best_ask.as_ref().map(|ask| ask.price).unwrap_or(1.0) {
            continue;
        }
        let clip_usd = (base_clip_usd * multiplier).min(config.caps.max_entry_notional_usd);
        let reward_qty = config.incentives.min_reward_quantity().unwrap_or(0.0);
        let quantity = (clip_usd / limit_price.max(tick_size))
            .max(market.min_order_size())
            .max(reward_qty);
        if quantity <= 0.0 || !quantity.is_finite() {
            continue;
        }

        intents.push(OrderIntent {
            client_order_id: ClientOrderId::from(format!(
                "paired-mm:{}:{}:l{}:{}",
                market.market_id(),
                leg_tag,
                level + 1,
                now_ms
            )),
            market_id: market.market_id().clone(),
            instrument_id: instrument_id.clone(),
            side: TradeSide::Buy,
            limit_price,
            quantity,
            reduce_only: false,
            reason: format!("paired-mm ladder {leg_tag} level {}", level + 1),
            quote_level_tag: Some(format!(
                "mm-paired-bid:{leg_tag}:l{}:{:?}",
                level + 1,
                MmQuoteKind::PairedEntry
            )),
            created_at_ms: now_ms,
            pair_id: Some(format!(
                "paired-mm:{}:l{}:{}",
                market.market_id(),
                level + 1,
                now_ms
            )),
            kind: IntentKind::Entry,
        });
    }
}

fn interleave_leg_ladders(
    intents: &mut Vec<OrderIntent>,
    yes_intents: Vec<OrderIntent>,
    no_intents: Vec<OrderIntent>,
) {
    let mut yes_iter = yes_intents.into_iter();
    let mut no_iter = no_intents.into_iter();
    loop {
        let mut pushed = false;
        if let Some(intent) = yes_iter.next() {
            intents.push(intent);
            pushed = true;
        }
        if let Some(intent) = no_iter.next() {
            intents.push(intent);
            pushed = true;
        }
        if !pushed {
            break;
        }
    }
}

fn normalize_paired_entry_quantities(
    yes_intents: &mut Vec<OrderIntent>,
    no_intents: &mut Vec<OrderIntent>,
    min_order_size: f64,
) {
    if yes_intents.is_empty() || no_intents.is_empty() {
        return;
    }

    let min_order_size = min_order_size.max(0.0);
    let mut normalized_yes = Vec::with_capacity(yes_intents.len().min(no_intents.len()));
    let mut normalized_no = Vec::with_capacity(yes_intents.len().min(no_intents.len()));

    for (mut yes, mut no) in std::mem::take(yes_intents)
        .into_iter()
        .zip(std::mem::take(no_intents).into_iter())
    {
        let paired_quantity = yes.quantity.min(no.quantity);
        if !paired_quantity.is_finite() || paired_quantity < min_order_size {
            continue;
        }
        yes.quantity = paired_quantity;
        no.quantity = paired_quantity;
        normalized_yes.push(yes);
        normalized_no.push(no);
    }

    *yes_intents = normalized_yes;
    *no_intents = normalized_no;
}

fn align_down_to_tick(price: f64, tick_size: f64) -> f64 {
    if !price.is_finite() || tick_size <= 0.0 {
        return price;
    }
    (price / tick_size).floor() * tick_size
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markets::BinaryOutcomeMarket;
    use crate::signals::FairValueModel;
    use crate::types::{
        BookLevel, ClientOrderId, InstrumentId, IntentKind, MarketId, QuoteSnapshot, TradeSide,
    };

    fn market() -> BinaryOutcomeMarket {
        let mut market = BinaryOutcomeMarket::btc_5m(
            MarketId::from("m"),
            InstrumentId::from("yes"),
            InstrumentId::from("no"),
        );
        market.event_end_ms = Some(300_000);
        market
    }

    fn snapshot() -> PairedMarketSnapshot {
        PairedMarketSnapshot {
            market_id: MarketId::from("m"),
            yes_instrument_id: InstrumentId::from("yes"),
            no_instrument_id: InstrumentId::from("no"),
            yes_quote: QuoteSnapshot::default(),
            no_quote: QuoteSnapshot::default(),
        }
    }

    fn snapshot_with_asks(yes_ask: f64, no_ask: f64) -> PairedMarketSnapshot {
        PairedMarketSnapshot {
            yes_quote: QuoteSnapshot {
                best_bid: Some(BookLevel::new((yes_ask - 0.01).max(0.01), 10.0)),
                best_ask: Some(BookLevel::new(yes_ask, 10.0)),
                ..QuoteSnapshot::default()
            },
            no_quote: QuoteSnapshot {
                best_bid: Some(BookLevel::new((no_ask - 0.01).max(0.01), 10.0)),
                best_ask: Some(BookLevel::new(no_ask, 10.0)),
                ..QuoteSnapshot::default()
            },
            ..snapshot()
        }
    }

    fn tagged_intent(tag: &str) -> OrderIntent {
        OrderIntent {
            client_order_id: ClientOrderId::from(tag),
            market_id: MarketId::from("m"),
            instrument_id: InstrumentId::from(tag),
            side: TradeSide::Buy,
            limit_price: 0.5,
            quantity: 1.0,
            reduce_only: false,
            reason: tag.to_string(),
            quote_level_tag: Some(tag.to_string()),
            created_at_ms: 0,
            pair_id: None,
            kind: IntentKind::Entry,
        }
    }

    #[test]
    fn interleaves_yes_and_no_ladders_by_level_before_risk() {
        let mut intents = Vec::new();

        interleave_leg_ladders(
            &mut intents,
            vec![
                tagged_intent("yes:l1"),
                tagged_intent("yes:l2"),
                tagged_intent("yes:l3"),
            ],
            vec![tagged_intent("no:l1"), tagged_intent("no:l2")],
        );

        let tags: Vec<&str> = intents
            .iter()
            .map(|intent| intent.quote_level_tag.as_deref().unwrap())
            .collect();
        assert_eq!(tags, vec!["yes:l1", "no:l1", "yes:l2", "no:l2", "yes:l3"]);
    }

    #[test]
    fn high_inventory_suppresses_heavy_side() {
        let inventory = PairedInventorySnapshot {
            yes_qty: 200.0,
            no_qty: 0.0,
            yes_avg_cost: 0.50,
            no_avg_cost: 0.0,
            free_cash_usd: 1_000.0,
            equity_usd: 1_000.0,
        };
        let config = LadderConfig {
            caps: RunningInventoryCaps {
                max_side_imbalance_qty: 100.0,
                ..RunningInventoryCaps::default()
            },
            ..LadderConfig::default()
        };
        let result = build_ladder(
            &market(),
            &snapshot(),
            &inventory,
            &FairValueEstimate {
                p_up: 0.50,
                p_down: 0.50,
                log_moneyness: 0.0,
                sigma_remaining: 0.0,
                time_remaining_s: 100.0,
                model: FairValueModel::BsmBinary,
            },
            &BtcRegimeSnapshot {
                realized_vol_5m_bps: Some(2.0),
                return_180s_bps: Some(1.0),
                ..BtcRegimeSnapshot::default()
            },
            &PairCostTracker::from_inventory(&inventory),
            &config,
            0,
        );
        assert!(result.diagnostics.suppressed_yes);
        assert!(!result.diagnostics.suppressed_no);
        assert!(result
            .intents
            .iter()
            .all(|intent| intent.instrument_id.as_str() == "no"));
    }

    #[test]
    fn ladder_buy_quotes_do_not_cross_best_ask() {
        let result = build_ladder(
            &market(),
            &snapshot_with_asks(0.80, 0.21),
            &PairedInventorySnapshot {
                free_cash_usd: 1_000.0,
                equity_usd: 1_000.0,
                ..Default::default()
            },
            &FairValueEstimate {
                p_up: 0.999,
                p_down: 0.001,
                log_moneyness: 0.0,
                sigma_remaining: 0.0,
                time_remaining_s: 100.0,
                model: FairValueModel::BsmBinary,
            },
            &BtcRegimeSnapshot {
                realized_vol_5m_bps: Some(2.0),
                ..BtcRegimeSnapshot::default()
            },
            &PairCostTracker::default(),
            &LadderConfig::default(),
            0,
        );

        for intent in result.intents {
            if intent.instrument_id.as_str() == "yes" {
                assert!(intent.limit_price < 0.80);
            }
            if intent.instrument_id.as_str() == "no" {
                assert!(intent.limit_price < 0.21);
            }
        }
    }

    #[test]
    fn paired_entry_levels_use_equal_share_quantity_on_both_legs() {
        let result = build_ladder(
            &market(),
            &snapshot_with_asks(0.11, 0.91),
            &PairedInventorySnapshot {
                free_cash_usd: 1_000.0,
                equity_usd: 1_000.0,
                ..Default::default()
            },
            &FairValueEstimate {
                p_up: 0.50,
                p_down: 0.50,
                log_moneyness: 0.0,
                sigma_remaining: 0.0,
                time_remaining_s: 100.0,
                model: FairValueModel::BsmBinary,
            },
            &BtcRegimeSnapshot {
                realized_vol_5m_bps: Some(2.0),
                ..BtcRegimeSnapshot::default()
            },
            &PairCostTracker::default(),
            &LadderConfig {
                max_depth: 2,
                low_vol_depth: 2,
                base_clip_usd: 5.0,
                max_clip_usd: 5.0,
                stoikov: StoikovParams {
                    gamma: 0.0,
                    k: 1.0,
                    max_skew: 0.20,
                },
                ..LadderConfig::default()
            },
            0,
        );

        let yes: Vec<&OrderIntent> = result
            .intents
            .iter()
            .filter(|intent| intent.instrument_id.as_str() == "yes")
            .collect();
        let no: Vec<&OrderIntent> = result
            .intents
            .iter()
            .filter(|intent| intent.instrument_id.as_str() == "no")
            .collect();

        assert_eq!(yes.len(), no.len());
        assert!(!yes.is_empty());
        for (yes, no) in yes.iter().zip(no.iter()) {
            assert!(
                (yes.quantity - no.quantity).abs() < 1e-9,
                "yes={yes:?} no={no:?}"
            );
        }
    }

    #[test]
    fn paired_mm_fair_is_anchored_to_book_mid_when_model_is_extreme() {
        let result = build_ladder(
            &market(),
            &snapshot_with_asks(0.58, 0.43),
            &PairedInventorySnapshot {
                free_cash_usd: 1_000.0,
                equity_usd: 1_000.0,
                ..Default::default()
            },
            &FairValueEstimate {
                p_up: 0.999,
                p_down: 0.001,
                log_moneyness: 0.0,
                sigma_remaining: 0.0,
                time_remaining_s: 100.0,
                model: FairValueModel::BsmBinary,
            },
            &BtcRegimeSnapshot {
                realized_vol_5m_bps: Some(2.0),
                ..BtcRegimeSnapshot::default()
            },
            &PairCostTracker::default(),
            &LadderConfig::default(),
            0,
        );

        assert!(result.diagnostics.yes_reservation < 0.70);
        assert!(result.diagnostics.no_reservation > 0.30);
    }

    #[test]
    fn paired_mm_fair_uses_weighted_model_pull_from_book_mid() {
        let result = build_ladder(
            &market(),
            &snapshot_with_asks(0.56, 0.46),
            &PairedInventorySnapshot {
                free_cash_usd: 1_000.0,
                equity_usd: 1_000.0,
                ..Default::default()
            },
            &FairValueEstimate {
                p_up: 0.70,
                p_down: 0.30,
                log_moneyness: 0.0,
                sigma_remaining: 0.0,
                time_remaining_s: 100.0,
                model: FairValueModel::BsmBinary,
            },
            &BtcRegimeSnapshot {
                realized_vol_5m_bps: Some(2.0),
                ..BtcRegimeSnapshot::default()
            },
            &PairCostTracker::default(),
            &LadderConfig {
                fair_value_anchoring: FairValueAnchoringConfig {
                    max_model_divergence: 0.20,
                    model_influence_weight: 0.25,
                },
                stoikov: StoikovParams {
                    gamma: 0.0,
                    k: 1.0,
                    max_skew: 0.20,
                },
                ..LadderConfig::default()
            },
            0,
        );

        assert!((result.diagnostics.yes_reservation - 0.59125).abs() < 0.0001);
        assert!((result.diagnostics.no_reservation - 0.41625).abs() < 0.0001);
    }

    #[test]
    fn paired_mm_fair_caps_mild_model_pull_at_configured_divergence() {
        let result = build_ladder(
            &market(),
            &snapshot_with_asks(0.56, 0.46),
            &PairedInventorySnapshot {
                free_cash_usd: 1_000.0,
                equity_usd: 1_000.0,
                ..Default::default()
            },
            &FairValueEstimate {
                p_up: 0.90,
                p_down: 0.10,
                log_moneyness: 0.0,
                sigma_remaining: 0.0,
                time_remaining_s: 100.0,
                model: FairValueModel::BsmBinary,
            },
            &BtcRegimeSnapshot {
                realized_vol_5m_bps: Some(2.0),
                ..BtcRegimeSnapshot::default()
            },
            &PairCostTracker::default(),
            &LadderConfig {
                fair_value_anchoring: FairValueAnchoringConfig {
                    max_model_divergence: 0.05,
                    model_influence_weight: 1.0,
                },
                stoikov: StoikovParams {
                    gamma: 0.0,
                    k: 1.0,
                    max_skew: 0.20,
                },
                ..LadderConfig::default()
            },
            0,
        );

        assert!((result.diagnostics.yes_reservation - 0.605).abs() < 0.0001);
        assert!((result.diagnostics.no_reservation - 0.405).abs() < 0.0001);
    }

    #[test]
    fn dust_fill_imbalance_does_not_force_inventory_imbalanced_ladder_shape() {
        let result = build_ladder(
            &market(),
            &snapshot_with_asks(0.56, 0.46),
            &PairedInventorySnapshot {
                yes_qty: 0.3952,
                no_qty: 0.0,
                free_cash_usd: 1_000.0,
                equity_usd: 1_000.0,
                ..Default::default()
            },
            &FairValueEstimate {
                p_up: 0.50,
                p_down: 0.50,
                log_moneyness: 0.0,
                sigma_remaining: 0.0,
                time_remaining_s: 100.0,
                model: FairValueModel::BsmBinary,
            },
            &BtcRegimeSnapshot {
                realized_vol_5m_bps: Some(2.0),
                ..BtcRegimeSnapshot::default()
            },
            &PairCostTracker::default(),
            &LadderConfig {
                stoikov: StoikovParams {
                    gamma: 0.0,
                    k: 1.0,
                    max_skew: 0.20,
                },
                ..LadderConfig::default()
            },
            0,
        );

        assert_ne!(result.diagnostics.regime, LadderRegime::InventoryImbalanced);
    }
}
