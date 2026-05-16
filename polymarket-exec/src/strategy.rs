//! Runtime strategy adapter seam.
//!
//! The legacy strategy implementations have been removed from this file.
//! Runtime still calls `crate::strategy::Strategy`; this module now adapts
//! those calls into the modular strategy implementations in `crate::strategies`.

use std::collections::HashMap;

use crate::inventory::InventorySnapshot;
use crate::market_context::MarketContextRecord;
use crate::market_making::paired_mm::{
    PairCostTracker, PairedInventorySnapshot, PairedMarketSnapshot,
};
use crate::markets::{BinaryOutcomeMarket, MarketDescriptor, MarketTenor, UnderlyingAsset};
use crate::signals::fair_value::NoSignalReason;
use crate::signals::{estimate_fair_value_with_momentum, FairValueEstimate, FairValueModel};
use crate::strategies::bonereaper_mm::BonereaperMmStrategy;
use crate::strategies::paired_mm::PairedMmStrategy;
use crate::strategies::traits::{
    PairedOpenOrderExposure, StrategyDirectionalInventorySnapshot, StrategyFillInput,
    StrategyInput, StrategyOpenOrderSnapshot, TradingStrategy,
};
use crate::strategies::unlawful_mm::UnlawfulMmStrategy;
pub use crate::strategy_profile::*;
use crate::types::{
    EpochMillis, FillReport, InstrumentId, IntentKind, MarketId, MarketLedgerState, MarketSnapshot,
    OrderIntent, QuoteSnapshot, RuntimeCommand, RuntimeStatus, SuppressionScope,
};

#[derive(Debug, Clone)]
pub struct BtcRegimeSnapshot {
    pub last_price: Option<f64>,
    pub realized_vol_5m_bps: Option<f64>,
    pub realized_vol_15m_bps: Option<f64>,
    pub trade_count_5m: u64,
    pub trade_count_15m: u64,
    pub return_30s_bps: Option<f64>,
    pub return_60s_bps: Option<f64>,
    pub observed_at_ms: u64,
}

impl Default for BtcRegimeSnapshot {
    fn default() -> Self {
        Self {
            last_price: None,
            realized_vol_5m_bps: None,
            realized_vol_15m_bps: None,
            trade_count_5m: 0,
            trade_count_15m: 0,
            return_30s_bps: None,
            return_60s_bps: None,
            observed_at_ms: 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum StrategyDecisionSuppressionKind {
    SoftPause,
    HardRiskOff,
}

#[derive(Clone, Debug, PartialEq)]
pub enum StrategyDecision {
    Noop {
        notes: Vec<String>,
    },
    QuoteSet {
        intents: Vec<OrderIntent>,
        notes: Vec<String>,
    },
    Reactive {
        intents: Vec<OrderIntent>,
        notes: Vec<String>,
    },
    Commands {
        commands: Vec<RuntimeCommand>,
        notes: Vec<String>,
    },
    Mixed {
        intents: Vec<OrderIntent>,
        commands: Vec<RuntimeCommand>,
        reactive: bool,
        notes: Vec<String>,
    },
    Suppress {
        kind: StrategyDecisionSuppressionKind,
        preserve_quotes: bool,
        notes: Vec<String>,
    },
}

impl StrategyDecision {
    pub fn none() -> Self {
        Self::Noop { notes: Vec::new() }
    }

    pub fn single(intent: OrderIntent) -> Self {
        Self::QuoteSet {
            intents: vec![intent],
            notes: Vec::new(),
        }
    }

    pub fn quote_set(intents: Vec<OrderIntent>, notes: Vec<String>) -> Self {
        Self::QuoteSet { intents, notes }
    }

    pub fn reactive(intents: Vec<OrderIntent>, notes: Vec<String>) -> Self {
        Self::Reactive { intents, notes }
    }

    pub fn commands(commands: Vec<RuntimeCommand>, notes: Vec<String>) -> Self {
        Self::Commands { commands, notes }
    }

    pub fn mixed(
        intents: Vec<OrderIntent>,
        commands: Vec<RuntimeCommand>,
        reactive: bool,
        notes: Vec<String>,
    ) -> Self {
        Self::Mixed {
            intents,
            commands,
            reactive,
            notes,
        }
    }

    pub fn suppress(
        kind: StrategyDecisionSuppressionKind,
        preserve_quotes: bool,
        notes: Vec<String>,
    ) -> Self {
        Self::Suppress {
            kind,
            preserve_quotes,
            notes,
        }
    }

    pub fn intents(&self) -> &[OrderIntent] {
        match self {
            Self::QuoteSet { intents, .. }
            | Self::Reactive { intents, .. }
            | Self::Mixed { intents, .. } => intents,
            Self::Noop { .. } | Self::Commands { .. } | Self::Suppress { .. } => &[],
        }
    }

    pub fn notes(&self) -> Vec<String> {
        match self {
            Self::Noop { notes }
            | Self::QuoteSet { notes, .. }
            | Self::Reactive { notes, .. }
            | Self::Commands { notes, .. }
            | Self::Mixed { notes, .. }
            | Self::Suppress { notes, .. } => notes.clone(),
        }
    }

    pub fn suppress_kind(&self) -> Option<&StrategyDecisionSuppressionKind> {
        match self {
            Self::Suppress { kind, .. } => Some(kind),
            _ => None,
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Self::Noop { notes } => notes.is_empty(),
            Self::QuoteSet { intents, notes } | Self::Reactive { intents, notes } => {
                intents.is_empty() && notes.is_empty()
            }
            Self::Commands { commands, notes } => commands.is_empty() && notes.is_empty(),
            Self::Mixed {
                intents,
                commands,
                notes,
                ..
            } => intents.is_empty() && commands.is_empty() && notes.is_empty(),
            Self::Suppress { .. } => false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct StrategyContext {
    pub now_ms: EpochMillis,
    pub runtime_status: RuntimeStatus,
    pub inventory: InventorySnapshot,
    pub paired_core_inventory: Option<PairedInventorySnapshot>,
    pub directional_inventory: Vec<StrategyDirectionalInventorySnapshot>,
    pub late_fav_inventory: Vec<StrategyDirectionalInventorySnapshot>,
    pub cheap_tail_inventory: Vec<StrategyDirectionalInventorySnapshot>,
    pub open_orders: Vec<StrategyOpenOrderSnapshot>,
    pub open_orders_total: usize,
    pub open_orders_for_market: usize,
    pub market_ledger_state: MarketLedgerState,
    pub market_context: Option<MarketContextRecord>,
    pub btc_regime: crate::signals::BtcRegimeSnapshot,
    pub momentum: crate::signals::MomentumSignal,
    pub venue_rules: Option<VenueMarketRules>,
}

#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct VenueMarketRules {
    pub minimum_order_size: f64,
    pub minimum_tick_size: f64,
    pub neg_risk: bool,
}

pub trait Strategy {
    fn name(&self) -> &str;

    fn on_start(&mut self, _context: &StrategyContext) -> StrategyDecision {
        StrategyDecision::none()
    }

    fn on_market_snapshot(
        &mut self,
        _context: &StrategyContext,
        _snapshot: &MarketSnapshot,
    ) -> StrategyDecision {
        StrategyDecision::none()
    }

    fn on_fill(&mut self, _context: &StrategyContext, _fill: &FillReport) -> StrategyDecision {
        StrategyDecision::none()
    }

    fn checkpoint_state(&self) -> Option<serde_json::Value> {
        None
    }

    fn restore_checkpoint_state(
        &mut self,
        _state: &serde_json::Value,
    ) -> std::result::Result<(), String> {
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct NoopStrategy;

impl Strategy for NoopStrategy {
    fn name(&self) -> &str {
        "noop"
    }
}

#[derive(Debug)]
pub enum StrategyMode {
    Hybrid(HybridStrategy),
    Noop(NoopStrategy),
}

impl StrategyMode {
    pub fn try_from_name(
        name: &str,
        profile: Option<&StrategyProfile>,
    ) -> std::result::Result<Self, String> {
        let requested = parse_strategy_names(name);
        if requested.is_empty() {
            return Err("at least one strategy must be configured".to_string());
        }
        if requested.iter().all(|name| name == "noop") {
            return Ok(Self::Noop(NoopStrategy));
        }
        Ok(Self::Hybrid(HybridStrategy::new(profile, &requested)?))
    }

    pub fn taker_fee_coeff(&self) -> f64 {
        match self {
            Self::Hybrid(_) => 0.072,
            Self::Noop(_) => 0.0,
        }
    }
}

impl Strategy for StrategyMode {
    fn name(&self) -> &str {
        match self {
            Self::Hybrid(strategy) => strategy.name(),
            Self::Noop(strategy) => strategy.name(),
        }
    }

    fn on_start(&mut self, context: &StrategyContext) -> StrategyDecision {
        match self {
            Self::Hybrid(strategy) => strategy.on_start(context),
            Self::Noop(strategy) => strategy.on_start(context),
        }
    }

    fn on_market_snapshot(
        &mut self,
        context: &StrategyContext,
        snapshot: &MarketSnapshot,
    ) -> StrategyDecision {
        match self {
            Self::Hybrid(strategy) => strategy.on_market_snapshot(context, snapshot),
            Self::Noop(strategy) => strategy.on_market_snapshot(context, snapshot),
        }
    }

    fn on_fill(&mut self, context: &StrategyContext, fill: &FillReport) -> StrategyDecision {
        match self {
            Self::Hybrid(strategy) => strategy.on_fill(context, fill),
            Self::Noop(strategy) => strategy.on_fill(context, fill),
        }
    }
}

fn parse_strategy_names(raw: &str) -> Vec<String> {
    fn canonicalize(name: &str) -> Vec<String> {
        match name {
            "unlawful_mm" => {
                vec!["unlawful_mm".to_string()]
            }
            "bonereaper_mm" => vec!["bonereaper_mm".to_string()],
            "core_hedge_mm" | "core_hedge" => vec!["unlawful_mm".to_string()],
            "late_favorite_directional" => vec!["bonereaper_mm".to_string()],
            other => vec![other.to_string()],
        }
    }

    raw.split([',', '+'])
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .flat_map(|name| canonicalize(&name.to_ascii_lowercase()).into_iter())
        .collect()
}

#[derive(Debug)]
pub struct HybridStrategy {
    name: String,
    paired_mm: Option<PairedMmStrategy>,
    unlawful_mm: Option<UnlawfulMmStrategy>,
    bonereaper_mm: Option<BonereaperMmStrategy>,
    quotes_by_market: HashMap<MarketId, HashMap<InstrumentId, QuoteSnapshot>>,
    momentum_weight: f64,
    mm_overlay_capital_pct: f64,
    mm_overlay_max_levels_per_side: usize,
}

impl HybridStrategy {
    fn new(
        profile: Option<&StrategyProfile>,
        requested: &[String],
    ) -> std::result::Result<Self, String> {
        let default_profile = StrategyProfile::default();
        let profile = profile.unwrap_or(&default_profile);
        let mut paired_mm = None;
        let mut unlawful_mm = None;
        let mut bonereaper_mm = None;

        for name in requested {
            match name.as_str() {
                "paired_mm" => {
                    paired_mm = Some(PairedMmStrategy::new(profile.paired_mm_config()));
                }
                "unlawful_mm" => {
                    unlawful_mm = Some(UnlawfulMmStrategy::new(profile.unlawful_mm_config()));
                }
                "bonereaper_mm" => {
                    bonereaper_mm = Some(BonereaperMmStrategy::new(profile.bonereaper_mm_config()));
                }
                "noop" => {}
                other => return Err(format!("unsupported strategy '{other}'")),
            }
        }
        if paired_mm.is_none() && unlawful_mm.is_none() && bonereaper_mm.is_none() {
            return Err("configured strategy set contains no active strategy".to_string());
        }
        let name = requested.join(",");
        Ok(Self {
            name,
            paired_mm,
            unlawful_mm,
            bonereaper_mm,
            quotes_by_market: HashMap::new(),
            momentum_weight: profile.momentum_weight(),
            mm_overlay_capital_pct: 1.0,
            mm_overlay_max_levels_per_side: usize::MAX,
        })
    }

    fn name(&self) -> &str {
        self.name.as_str()
    }

    fn update_quote_cache(&mut self, snapshot: &MarketSnapshot) {
        self.quotes_by_market
            .entry(snapshot.market_id.clone())
            .or_default()
            .insert(snapshot.instrument_id.clone(), snapshot.quote.clone());
    }

    fn build_input(
        &self,
        context: &StrategyContext,
        market_id: &MarketId,
    ) -> Option<StrategyInput<BinaryOutcomeMarket>> {
        let quotes = self.quotes_by_market.get(market_id)?;
        if quotes.len() < 2 {
            return None;
        }
        let (yes_id, no_id) = infer_yes_no_ids(context.market_context.as_ref(), quotes)?;
        let yes_quote = quotes.get(&yes_id)?.clone();
        let no_quote = quotes.get(&no_id)?.clone();
        let mut market =
            BinaryOutcomeMarket::btc_5m(market_id.clone(), yes_id.clone(), no_id.clone());
        if let Some(record) = context.market_context.as_ref() {
            market = market.with_context(record);
            market.tenor = infer_tenor(record).unwrap_or(MarketTenor::Minutes(5));
        }
        if let Some(rules) = context.venue_rules {
            if rules.minimum_tick_size.is_finite() && rules.minimum_tick_size > 0.0 {
                market.tick_size = rules.minimum_tick_size;
            }
            if rules.minimum_order_size.is_finite() && rules.minimum_order_size > 0.0 {
                market.min_order_size = rules.minimum_order_size;
            }
        }
        let snapshot = PairedMarketSnapshot {
            market_id: market_id.clone(),
            yes_instrument_id: yes_id.clone(),
            no_instrument_id: no_id.clone(),
            yes_quote,
            no_quote,
        };
        let inventory =
            paired_inventory_from_context(&context.inventory, market_id, &yes_id, &no_id);
        let paired_core_inventory = context.paired_core_inventory.unwrap_or_else(|| {
            paired_core_inventory_from_context(
                inventory,
                &context.directional_inventory,
                market_id,
                &yes_id,
                &no_id,
            )
        });
        let late_fav_inventory = paired_inventory_from_directional_snapshots(
            &context.late_fav_inventory,
            market_id,
            &yes_id,
            &no_id,
        );
        let cheap_tail_inventory = paired_inventory_from_directional_snapshots(
            &context.cheap_tail_inventory,
            market_id,
            &yes_id,
            &no_id,
        );
        let open_convex_order_exposure =
            paired_convex_open_order_exposure(&context.open_orders, market_id, &yes_id, &no_id);
        let open_late_fav_order_exposure = paired_tagged_open_order_exposure(
            &context.open_orders,
            market_id,
            &yes_id,
            &no_id,
            "late-fav",
        );
        let open_paired_core_order_exposure = paired_tagged_open_order_exposure(
            &context.open_orders,
            market_id,
            &yes_id,
            &no_id,
            "paired-core:",
        );
        let pair_cost = PairCostTracker::from_inventory(&paired_core_inventory);
        let fair_value = fair_value_from_context(context, &market, self.momentum_weight);
        let order_book_pressure =
            crate::signals::OrderBookPressureEngine::default().compute(&snapshot);
        let open_orders_for_market: Vec<StrategyOpenOrderSnapshot> = context
            .open_orders
            .iter()
            .filter(|o| &o.market_id == market_id)
            .cloned()
            .collect();
        Some(StrategyInput {
            market,
            snapshot,
            inventory,
            paired_core_inventory,
            late_fav_inventory,
            cheap_tail_inventory,
            open_convex_order_exposure,
            open_late_fav_order_exposure,
            open_paired_core_order_exposure,
            open_orders: open_orders_for_market,
            pair_cost,
            fair_value,
            btc_regime: context.btc_regime.clone(),
            momentum: context.momentum.clone(),
            order_book_pressure,
            now_ms: context.now_ms,
        })
    }

    fn convert(decision: crate::types::StrategyDecision) -> StrategyDecision {
        match decision {
            crate::types::StrategyDecision::QuoteSet { intents, notes } => {
                StrategyDecision::quote_set(intents, notes)
            }
            crate::types::StrategyDecision::CapitalRecycle { intents, notes }
            | crate::types::StrategyDecision::Rescue { intents, notes } => {
                StrategyDecision::reactive(intents, notes)
            }
            crate::types::StrategyDecision::Merge { intent, notes } => {
                StrategyDecision::commands(vec![RuntimeCommand::Merge(intent)], notes)
            }
            crate::types::StrategyDecision::Mixed {
                intents,
                commands,
                notes,
            } => StrategyDecision::mixed(intents, commands, true, notes),
            crate::types::StrategyDecision::Suppress {
                scope,
                reason,
                preserve_quotes,
                notes,
            } => {
                let kind = match scope {
                    SuppressionScope::AllActions => StrategyDecisionSuppressionKind::HardRiskOff,
                    SuppressionScope::PairedOnly | SuppressionScope::AllEntry => {
                        StrategyDecisionSuppressionKind::SoftPause
                    }
                };
                let mut notes = notes;
                notes.push(format!("strategy suppressed: {reason:?} scope={scope:?}"));
                StrategyDecision::suppress(kind, preserve_quotes, notes)
            }
            crate::types::StrategyDecision::Noop { notes } => StrategyDecision::Noop { notes },
        }
    }

    fn combine(decisions: Vec<StrategyDecision>) -> StrategyDecision {
        let mut notes = Vec::new();
        let mut quote_intents = Vec::new();
        let mut reactive_intents = Vec::new();
        let mut commands = Vec::new();
        let mut has_reactive_mixed = false;
        let mut hard_suppressed = false;
        let mut soft_suppressed = false;
        let mut soft_preserve_quotes = true;

        for decision in decisions {
            match decision {
                StrategyDecision::Noop {
                    notes: decision_notes,
                } => notes.extend(decision_notes),
                StrategyDecision::QuoteSet {
                    intents,
                    notes: decision_notes,
                } => {
                    quote_intents.extend(intents);
                    notes.extend(decision_notes);
                }
                StrategyDecision::Reactive {
                    intents,
                    notes: decision_notes,
                } => {
                    reactive_intents.extend(intents);
                    notes.extend(decision_notes);
                }
                StrategyDecision::Commands {
                    commands: decision_commands,
                    notes: decision_notes,
                } => {
                    commands.extend(decision_commands);
                    notes.extend(decision_notes);
                }
                StrategyDecision::Mixed {
                    intents,
                    commands: decision_commands,
                    reactive,
                    notes: decision_notes,
                } => {
                    if reactive {
                        has_reactive_mixed = true;
                        reactive_intents.extend(intents);
                    } else {
                        quote_intents.extend(intents);
                    }
                    commands.extend(decision_commands);
                    notes.extend(decision_notes);
                }
                StrategyDecision::Suppress {
                    kind,
                    preserve_quotes,
                    notes: decision_notes,
                } => {
                    match kind {
                        StrategyDecisionSuppressionKind::HardRiskOff => hard_suppressed = true,
                        StrategyDecisionSuppressionKind::SoftPause => {
                            soft_suppressed = true;
                            soft_preserve_quotes &= preserve_quotes;
                        }
                    }
                    notes.extend(decision_notes);
                }
            }
        }

        if hard_suppressed {
            return StrategyDecision::suppress(
                StrategyDecisionSuppressionKind::HardRiskOff,
                false,
                notes,
            );
        }
        if !reactive_intents.is_empty() {
            reactive_intents.extend(quote_intents);
            if !commands.is_empty() {
                return StrategyDecision::mixed(reactive_intents, commands, true, notes);
            }
            return StrategyDecision::reactive(reactive_intents, notes);
        }
        if !quote_intents.is_empty() {
            if !commands.is_empty() {
                return StrategyDecision::mixed(quote_intents, commands, has_reactive_mixed, notes);
            }
            return StrategyDecision::quote_set(quote_intents, notes);
        }
        if !commands.is_empty() {
            return StrategyDecision::commands(commands, notes);
        }
        if soft_suppressed {
            return StrategyDecision::suppress(
                StrategyDecisionSuppressionKind::SoftPause,
                soft_preserve_quotes,
                notes,
            );
        }
        StrategyDecision::Noop { notes }
    }

    fn cap_mm_overlay_decision(
        &self,
        decision: StrategyDecision,
        free_cash_usd: f64,
        pair_cost_active: bool,
    ) -> StrategyDecision {
        let StrategyDecision::QuoteSet { intents, mut notes } = decision else {
            return decision;
        };
        if !pair_cost_active && self.mm_overlay_capital_pct >= 1.0 {
            return StrategyDecision::QuoteSet { intents, notes };
        }

        let cap_usd = (free_cash_usd * self.mm_overlay_capital_pct).max(0.0);
        let mut used_usd = 0.0;
        let mut levels_by_side: HashMap<(InstrumentId, crate::types::TradeSide), usize> =
            HashMap::new();
        let original_count = intents.len();
        let mut kept = Vec::with_capacity(original_count);

        for intent in intents {
            let notional = intent.limit_price.max(0.0) * intent.quantity.max(0.0);
            let side_key = (intent.instrument_id.clone(), intent.side);
            let side_levels = levels_by_side.entry(side_key).or_insert(0);
            if *side_levels >= self.mm_overlay_max_levels_per_side {
                continue;
            }
            if used_usd + notional > cap_usd + 1e-9 {
                continue;
            }
            used_usd += notional;
            *side_levels += 1;
            kept.push(intent);
        }

        if kept.len() < original_count {
            notes.push(format!(
                "hybrid mm overlay budget clipped kept={} dropped={} cap_usd={cap_usd:.4} used_usd={used_usd:.4}",
                kept.len(),
                original_count - kept.len(),
            ));
        }
        StrategyDecision::QuoteSet {
            intents: kept,
            notes,
        }
    }

    fn filter_for_market_state(
        decision: StrategyDecision,
        context: &StrategyContext,
    ) -> StrategyDecision {
        let fresh_entry_allowed = context.runtime_status == RuntimeStatus::Running
            && context.market_ledger_state.allows_fresh_entry();
        if fresh_entry_allowed {
            return decision;
        }

        let reason = format!(
            "strategy adapter close-side-only: runtime_status={:?} market_ledger_state={:?}",
            context.runtime_status, context.market_ledger_state
        );

        match decision {
            StrategyDecision::QuoteSet { mut notes, .. } => {
                notes.push(format!("{reason}; dropped fresh quote set"));
                StrategyDecision::suppress(StrategyDecisionSuppressionKind::SoftPause, false, notes)
            }
            StrategyDecision::Reactive { intents, mut notes } => {
                let original_count = intents.len();
                let kept = intents
                    .into_iter()
                    .filter(|intent| intent.kind == IntentKind::Close || intent.reduce_only)
                    .collect::<Vec<_>>();
                if kept.is_empty() {
                    notes.push(format!(
                        "{reason}; dropped {original_count} non-close reactive intents"
                    ));
                    StrategyDecision::suppress(
                        StrategyDecisionSuppressionKind::SoftPause,
                        false,
                        notes,
                    )
                } else {
                    if kept.len() < original_count {
                        notes.push(format!(
                            "{reason}; kept {} close-side intents and dropped {} entry intents",
                            kept.len(),
                            original_count - kept.len()
                        ));
                    } else {
                        notes.push(format!("{reason}; allowing close-side repair"));
                    }
                    StrategyDecision::reactive(kept, notes)
                }
            }
            StrategyDecision::Commands {
                commands,
                mut notes,
            } => {
                if commands.is_empty() {
                    notes.push(format!("{reason}; no close-side commands"));
                    StrategyDecision::Noop { notes }
                } else {
                    notes.push(format!("{reason}; allowing close-side command"));
                    StrategyDecision::Commands { commands, notes }
                }
            }
            StrategyDecision::Mixed {
                intents,
                commands,
                reactive,
                mut notes,
            } => {
                let original_count = intents.len();
                let kept = intents
                    .into_iter()
                    .filter(|intent| intent.kind == IntentKind::Close || intent.reduce_only)
                    .collect::<Vec<_>>();
                if kept.len() < original_count {
                    notes.push(format!(
                        "{reason}; kept {} close-side mixed intents and dropped {} entry intents",
                        kept.len(),
                        original_count - kept.len()
                    ));
                }
                if kept.is_empty() && commands.is_empty() {
                    StrategyDecision::Noop { notes }
                } else {
                    StrategyDecision::Mixed {
                        intents: kept,
                        commands,
                        reactive,
                        notes,
                    }
                }
            }
            StrategyDecision::Noop { mut notes } => {
                notes.push(format!("{reason}; no repair action"));
                StrategyDecision::Noop { notes }
            }
            StrategyDecision::Suppress { .. } => decision,
        }
    }
}

impl Strategy for HybridStrategy {
    fn name(&self) -> &str {
        self.name()
    }

    fn on_start(&mut self, _context: &StrategyContext) -> StrategyDecision {
        StrategyDecision::Noop {
            notes: vec![format!("strategy adapter active: {}", self.name)],
        }
    }

    fn on_market_snapshot(
        &mut self,
        context: &StrategyContext,
        snapshot: &MarketSnapshot,
    ) -> StrategyDecision {
        self.update_quote_cache(snapshot);
        let Some(input) = self.build_input(context, &snapshot.market_id) else {
            return StrategyDecision::Noop {
                notes: vec!["strategy adapter waiting for paired yes/no books".to_string()],
            };
        };
        let mut decisions = Vec::new();
        if let Some(strategy) = self.paired_mm.as_mut() {
            let decision = Self::convert(strategy.on_tick(input.clone()));
            let decision =
                self.cap_mm_overlay_decision(decision, context.inventory.free_cash_usd, false);
            decisions.push(Self::filter_for_market_state(decision, context));
        }
        if let Some(strategy) = self.unlawful_mm.as_mut() {
            let decision = Self::convert(strategy.on_tick(input.clone()));
            decisions.push(Self::filter_for_market_state(decision, context));
        }
        if let Some(strategy) = self.bonereaper_mm.as_mut() {
            let decision = Self::convert(strategy.on_tick(input));
            decisions.push(Self::filter_for_market_state(decision, context));
        }
        Self::combine(decisions)
    }

    fn on_fill(&mut self, context: &StrategyContext, fill: &FillReport) -> StrategyDecision {
        let Some(input) = self.build_input(context, &fill.market_id) else {
            return StrategyDecision::none();
        };
        let fill_input = StrategyFillInput {
            market: input.market,
            snapshot: input.snapshot,
            fair_value: input.fair_value,
            fill: fill.clone(),
        };
        let mut decisions = Vec::new();
        if let Some(strategy) = self.paired_mm.as_mut() {
            decisions.push(Self::convert(strategy.on_fill(fill_input.clone())));
        }
        if let Some(strategy) = self.unlawful_mm.as_mut() {
            decisions.push(Self::convert(strategy.on_fill(fill_input.clone())));
        }
        if let Some(strategy) = self.bonereaper_mm.as_mut() {
            decisions.push(Self::convert(strategy.on_fill(fill_input)));
        }
        Self::combine(decisions)
    }
}

fn infer_yes_no_ids(
    context: Option<&MarketContextRecord>,
    quotes: &HashMap<InstrumentId, QuoteSnapshot>,
) -> Option<(InstrumentId, InstrumentId)> {
    if let Some(record) = context {
        if record.instrument_ids.len() >= 2 {
            let yes = InstrumentId::from(record.instrument_ids[0].clone());
            let no = InstrumentId::from(record.instrument_ids[1].clone());
            if quotes.contains_key(&yes) && quotes.contains_key(&no) {
                return Some((yes, no));
            }
        }
    }
    let mut ids = quotes.keys().cloned().collect::<Vec<_>>();
    ids.sort();
    Some((ids.first()?.clone(), ids.get(1)?.clone()))
}

fn infer_tenor(record: &MarketContextRecord) -> Option<MarketTenor> {
    let start = record.event_start_time_ms?;
    let end = record.event_end_time_ms?;
    let minutes = end.saturating_sub(start) / 60_000;
    Some(MarketTenor::Minutes(
        minutes.max(1).min(u16::MAX as u64) as u16
    ))
}

fn paired_inventory_from_context(
    inventory: &InventorySnapshot,
    market_id: &MarketId,
    yes_id: &InstrumentId,
    no_id: &InstrumentId,
) -> PairedInventorySnapshot {
    let mut paired = PairedInventorySnapshot {
        free_cash_usd: inventory.free_cash_usd,
        equity_usd: inventory.total_cash_usd + inventory.gross_exposure_usd,
        ..Default::default()
    };
    for position in inventory
        .positions
        .iter()
        .filter(|p| &p.market_id == market_id)
    {
        if &position.instrument_id == yes_id {
            paired.yes_qty = position.quantity.max(0.0);
            paired.yes_avg_cost = position.avg_price.max(0.0);
        } else if &position.instrument_id == no_id {
            paired.no_qty = position.quantity.max(0.0);
            paired.no_avg_cost = position.avg_price.max(0.0);
        }
    }
    paired
}

fn paired_core_inventory_from_context(
    total: PairedInventorySnapshot,
    directional_inventory: &[StrategyDirectionalInventorySnapshot],
    market_id: &MarketId,
    yes_id: &InstrumentId,
    no_id: &InstrumentId,
) -> PairedInventorySnapshot {
    let mut paired_core = total;
    for position in directional_inventory
        .iter()
        .filter(|position| &position.market_id == market_id)
    {
        if &position.instrument_id == yes_id {
            paired_core.yes_qty = (paired_core.yes_qty - position.quantity.max(0.0)).max(0.0);
        } else if &position.instrument_id == no_id {
            paired_core.no_qty = (paired_core.no_qty - position.quantity.max(0.0)).max(0.0);
        }
    }
    paired_core
}

fn paired_inventory_from_directional_snapshots(
    directional_inventory: &[StrategyDirectionalInventorySnapshot],
    market_id: &MarketId,
    yes_id: &InstrumentId,
    no_id: &InstrumentId,
) -> PairedInventorySnapshot {
    let mut paired = PairedInventorySnapshot::default();
    for position in directional_inventory
        .iter()
        .filter(|position| &position.market_id == market_id)
    {
        if &position.instrument_id == yes_id {
            let next_qty = paired.yes_qty + position.quantity.max(0.0);
            paired.yes_avg_cost = weighted_avg_cost(
                paired.yes_qty,
                paired.yes_avg_cost,
                position.quantity.max(0.0),
                position.avg_cost,
            );
            paired.yes_qty = next_qty;
        } else if &position.instrument_id == no_id {
            let next_qty = paired.no_qty + position.quantity.max(0.0);
            paired.no_avg_cost = weighted_avg_cost(
                paired.no_qty,
                paired.no_avg_cost,
                position.quantity.max(0.0),
                position.avg_cost,
            );
            paired.no_qty = next_qty;
        }
    }
    paired
}

fn weighted_avg_cost(existing_qty: f64, existing_avg: f64, add_qty: f64, add_avg: f64) -> f64 {
    let existing_qty = existing_qty.max(0.0);
    let add_qty = add_qty.max(0.0);
    let next_qty = existing_qty + add_qty;
    if next_qty <= f64::EPSILON {
        0.0
    } else {
        ((existing_qty * existing_avg.max(0.0)) + (add_qty * add_avg.max(0.0))) / next_qty
    }
}

fn paired_tagged_open_order_exposure(
    open_orders: &[StrategyOpenOrderSnapshot],
    market_id: &MarketId,
    yes_id: &InstrumentId,
    no_id: &InstrumentId,
    tag_prefix: &str,
) -> PairedOpenOrderExposure {
    let mut exposure = PairedOpenOrderExposure::default();
    for order in open_orders.iter().filter(|order| {
        &order.market_id == market_id
            && order.side == crate::types::TradeSide::Buy
            && !order.reduce_only
            && order.remaining_qty > 1e-9
            && order
                .quote_level_tag
                .as_deref()
                .map(|tag| tag.to_ascii_lowercase().starts_with(tag_prefix))
                .unwrap_or(false)
    }) {
        let qty = order.remaining_qty.max(0.0);
        let notional = order.limit_price.max(0.0) * qty;
        if &order.instrument_id == yes_id {
            exposure.yes_qty += qty;
            exposure.yes_notional_usd += notional;
            exposure.yes_count += 1;
        } else if &order.instrument_id == no_id {
            exposure.no_qty += qty;
            exposure.no_notional_usd += notional;
            exposure.no_count += 1;
        }
    }
    exposure
}

fn paired_convex_open_order_exposure(
    open_orders: &[StrategyOpenOrderSnapshot],
    market_id: &MarketId,
    yes_id: &InstrumentId,
    no_id: &InstrumentId,
) -> PairedOpenOrderExposure {
    let mut exposure = PairedOpenOrderExposure::default();
    for order in open_orders.iter().filter(|order| {
        &order.market_id == market_id
            && order.side == crate::types::TradeSide::Buy
            && !order.reduce_only
            && order.remaining_qty > 1e-9
            && {
                let tag = order.quote_level_tag.as_deref().unwrap_or_default();
                tag.to_ascii_lowercase().contains("cheap-tail")
                    || order
                        .quote_level_tag
                        .as_deref()
                        .and_then(crate::types::MmQuoteKind::from_quote_level_tag)
                        == Some(crate::types::MmQuoteKind::ConvexAccumulation)
            }
    }) {
        let notional = order.limit_price.max(0.0) * order.remaining_qty.max(0.0);
        if &order.instrument_id == yes_id {
            exposure.yes_qty += order.remaining_qty.max(0.0);
            exposure.yes_notional_usd += notional;
            exposure.yes_count += 1;
        } else if &order.instrument_id == no_id {
            exposure.no_qty += order.remaining_qty.max(0.0);
            exposure.no_notional_usd += notional;
            exposure.no_count += 1;
        }
    }
    exposure
}

fn fair_value_from_context(
    context: &StrategyContext,
    market: &BinaryOutcomeMarket,
    momentum_weight: f64,
) -> FairValueEstimate {
    let no_signal = |reason| FairValueEstimate {
        p_up: 0.5,
        p_down: 0.5,
        log_moneyness: f64::NAN,
        sigma_remaining: f64::NAN,
        time_remaining_s: market.time_remaining_fraction(context.now_ms) * 300.0,
        model: FairValueModel::NoSignal(reason),
    };
    let Some(spot) = context.btc_regime.last_price else {
        return no_signal(NoSignalReason::SpotInvalid);
    };
    let Some(strike) = market.price_to_beat else {
        return no_signal(NoSignalReason::StrikeInvalid);
    };
    let tau = market.time_remaining_fraction(context.now_ms);
    let Some(vol_bps) = context.btc_regime.realized_vol_5m_bps else {
        return no_signal(NoSignalReason::VolInvalid);
    };
    let sigma_return = vol_bps / 10_000.0;
    let momentum_return = context.btc_regime.return_60s_bps.unwrap_or(0.0) / 10_000.0
        * momentum_weight.clamp(0.0, 5.0);
    estimate_fair_value_with_momentum(spot, strike, tau, sigma_return, momentum_return)
}

#[allow(dead_code)]
fn _underlying_for_context(_record: Option<&MarketContextRecord>) -> UnderlyingAsset {
    UnderlyingAsset::Btc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directional_lane_snapshots_preserve_average_cost() {
        let market_id = MarketId::from("m1");
        let yes_id = InstrumentId::from("yes");
        let no_id = InstrumentId::from("no");
        let snapshots = vec![
            StrategyDirectionalInventorySnapshot {
                market_id: market_id.clone(),
                instrument_id: yes_id.clone(),
                quantity: 10.0,
                avg_cost: 0.80,
                quote_level_tag: Some("late-fav".to_string()),
            },
            StrategyDirectionalInventorySnapshot {
                market_id: market_id.clone(),
                instrument_id: yes_id.clone(),
                quantity: 5.0,
                avg_cost: 0.90,
                quote_level_tag: Some("late-fav".to_string()),
            },
            StrategyDirectionalInventorySnapshot {
                market_id: market_id.clone(),
                instrument_id: no_id.clone(),
                quantity: 20.0,
                avg_cost: 0.05,
                quote_level_tag: Some("cheap-tail".to_string()),
            },
        ];

        let paired =
            paired_inventory_from_directional_snapshots(&snapshots, &market_id, &yes_id, &no_id);

        assert!((paired.yes_qty - 15.0).abs() < 1e-9);
        assert!((paired.yes_avg_cost - ((10.0 * 0.80 + 5.0 * 0.90) / 15.0)).abs() < 1e-9);
        assert!((paired.no_qty - 20.0).abs() < 1e-9);
        assert!((paired.no_avg_cost - 0.05).abs() < 1e-9);
    }
}
