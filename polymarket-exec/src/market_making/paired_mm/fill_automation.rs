//! Automatic post-fill handling for paired MM.
//!
//! This module is the migration target for the legacy on-fill rescue branch.
//! It is deliberately narrow:
//! - update per-leg FIFO lots from fills,
//! - maintain post-fill/asymmetric-fill cooldown state,
//! - produce an EV rescue candidate when a fill leaves one side stranded.
//!
//! It does not submit orders or walk venue depth. Concrete strategy/runtime
//! code must convert `AutoFillSuggestion::Rescue` into venue-safe intents via
//! `RescueIntentBuilder`.

use crate::core::lot_ledger::LotLedger;
use crate::market_making::paired_mm::fill_cooldown::{
    FillCooldown, FillCooldownConfig, FillCooldownDecision,
};
use crate::market_making::pairing::rescue_engine::{
    choose_rescue, RescueAction, RescueConfig, RescueDecision, RescueInputs,
};
use crate::market_making::pairing::types::{LadderLeg, PairedMarketSnapshot};
use crate::markets::MarketDescriptor;
use crate::signals::FairValueEstimate;
use crate::types::{CoolingReason, EpochMillis, FillReport, TradeSide};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AutoFillConfig {
    pub cooldown: FillCooldownConfig,
    pub rescue: RescueConfig,
    pub min_stranded_qty: f64,
    pub min_auto_rescue_qty: f64,
}

impl Default for AutoFillConfig {
    fn default() -> Self {
        Self {
            cooldown: FillCooldownConfig::default(),
            rescue: RescueConfig::default(),
            min_stranded_qty: 1e-6,
            min_auto_rescue_qty: 1.0,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum AutoFillSuggestion {
    None,
    Cooldown {
        reason: CoolingReason,
        note: String,
    },
    Rescue {
        inputs: RescueInputs,
        decision: RescueDecision,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct AutoFillDecision {
    pub suggestion: AutoFillSuggestion,
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AutoFillStateSnapshot {
    pub yes_qty: f64,
    pub yes_avg_cost: Option<f64>,
    pub no_qty: f64,
    pub no_avg_cost: Option<f64>,
    pub last_fill_ms: Option<EpochMillis>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AutoFillState {
    cooldown: FillCooldown,
    yes_lots: LotLedger,
    no_lots: LotLedger,
    last_fill_ms: Option<EpochMillis>,
}

impl AutoFillState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_snapshot(snapshot: AutoFillStateSnapshot) -> Self {
        let mut state = Self::new();
        if snapshot.yes_qty > 0.0 {
            state.yes_lots.push_buy(
                snapshot.yes_qty,
                snapshot.yes_avg_cost.unwrap_or(0.5),
                snapshot.last_fill_ms.unwrap_or_default(),
            );
        }
        if snapshot.no_qty > 0.0 {
            state.no_lots.push_buy(
                snapshot.no_qty,
                snapshot.no_avg_cost.unwrap_or(0.5),
                snapshot.last_fill_ms.unwrap_or_default(),
            );
        }
        state.last_fill_ms = snapshot.last_fill_ms;
        state
    }

    pub fn snapshot(&self) -> AutoFillStateSnapshot {
        AutoFillStateSnapshot {
            yes_qty: self.yes_qty(),
            yes_avg_cost: self.yes_avg_cost(),
            no_qty: self.no_qty(),
            no_avg_cost: self.no_avg_cost(),
            last_fill_ms: self.last_fill_ms,
        }
    }

    pub fn yes_qty(&self) -> f64 {
        self.yes_lots.quantity()
    }

    pub fn no_qty(&self) -> f64 {
        self.no_lots.quantity()
    }

    pub fn yes_avg_cost(&self) -> Option<f64> {
        self.yes_lots.avg_cost()
    }

    pub fn no_avg_cost(&self) -> Option<f64> {
        self.no_lots.avg_cost()
    }

    pub fn last_fill_ms(&self) -> Option<EpochMillis> {
        self.last_fill_ms
    }

    pub fn record_fill_by_leg(&mut self, leg: LadderLeg, fill: &FillReport) -> Vec<String> {
        self.apply_fill_to_lots(leg, fill);
        self.cooldown.record_fill(leg, fill.observed_at_ms);
        self.last_fill_ms = Some(fill.observed_at_ms);
        vec![format!(
            "auto-fill recorded leg={leg:?} side={:?} qty={:.4} price={:.4} yes_qty={:.4} no_qty={:.4}",
            fill.side,
            fill.quantity,
            fill.price,
            self.yes_qty(),
            self.no_qty()
        )]
    }

    pub fn evaluate_snapshot(
        &self,
        snapshot: &PairedMarketSnapshot,
        fair_value: &FairValueEstimate,
        now_ms: EpochMillis,
        config: AutoFillConfig,
    ) -> AutoFillDecision {
        let cooldown = self.cooldown.decision(now_ms, config.cooldown);
        let mut notes = Vec::new();
        if let FillCooldownDecision::SuppressPaired { reason } = cooldown {
            notes.push(reason.clone());
        }

        let Some(rescue_inputs) = self.rescue_inputs(snapshot, fair_value, config) else {
            return AutoFillDecision {
                suggestion: AutoFillSuggestion::Cooldown {
                    reason: CoolingReason::AsymmetricFillCooldown,
                    note: "post-fill state has no rescueable stranded leg".to_string(),
                },
                notes,
            };
        };

        let rescue_decision = choose_rescue(rescue_inputs, config.rescue);
        if rescue_decision.qty < config.min_auto_rescue_qty
            || rescue_decision.action == RescueAction::Hold
        {
            notes.push(format!(
                "auto-fill rescue held action={:?} qty={:.4} reason={}",
                rescue_decision.action, rescue_decision.qty, rescue_decision.reason
            ));
            return AutoFillDecision {
                suggestion: AutoFillSuggestion::Cooldown {
                    reason: CoolingReason::AsymmetricFillCooldown,
                    note: rescue_decision.reason,
                },
                notes,
            };
        }

        notes.push(format!(
            "auto-fill rescue selected action={:?} qty={:.4} reason={}",
            rescue_decision.action, rescue_decision.qty, rescue_decision.reason
        ));
        AutoFillDecision {
            suggestion: AutoFillSuggestion::Rescue {
                inputs: rescue_inputs,
                decision: rescue_decision,
            },
            notes,
        }
    }

    pub fn on_fill<M: MarketDescriptor>(
        &mut self,
        market: &M,
        fill: &FillReport,
        snapshot: &PairedMarketSnapshot,
        fair_value: &FairValueEstimate,
        config: AutoFillConfig,
    ) -> AutoFillDecision {
        let Some(leg) = classify_fill_leg(market, fill) else {
            return AutoFillDecision {
                suggestion: AutoFillSuggestion::None,
                notes: vec![format!(
                    "auto-fill ignored: fill instrument {} is not in market {}",
                    fill.instrument_id,
                    market.market_id()
                )],
            };
        };

        let mut notes = self.record_fill_by_leg(leg, fill);
        let mut decision =
            self.evaluate_snapshot(snapshot, fair_value, fill.observed_at_ms, config);
        notes.append(&mut decision.notes);
        decision.notes = notes;
        decision
    }

    fn apply_fill_to_lots(&mut self, leg: LadderLeg, fill: &FillReport) {
        let lots = match leg {
            LadderLeg::Yes => &mut self.yes_lots,
            LadderLeg::No => &mut self.no_lots,
        };
        match fill.side {
            TradeSide::Buy => lots.push_buy(fill.quantity, fill.price, fill.observed_at_ms),
            TradeSide::Sell => {
                let _ = lots.consume_fifo(fill.quantity);
            }
        }
    }

    fn rescue_inputs(
        &self,
        snapshot: &PairedMarketSnapshot,
        fair_value: &FairValueEstimate,
        config: AutoFillConfig,
    ) -> Option<RescueInputs> {
        let yes_qty = self.yes_qty();
        let no_qty = self.no_qty();
        let imbalance = (yes_qty - no_qty).abs();
        if imbalance < config.min_stranded_qty {
            return None;
        }

        if yes_qty > no_qty {
            let stranded_qty = imbalance;
            let avg_cost = self.yes_avg_cost()?;
            Some(RescueInputs {
                leg: LadderLeg::Yes,
                stranded_qty,
                avg_cost,
                fair_win_prob: fair_value.p_up,
                best_exit_bid: snapshot
                    .yes_quote
                    .best_bid
                    .as_ref()
                    .map(|level| level.price),
                opposite_best_ask: snapshot.no_quote.best_ask.as_ref().map(|level| level.price),
            })
        } else {
            let stranded_qty = imbalance;
            let avg_cost = self.no_avg_cost()?;
            Some(RescueInputs {
                leg: LadderLeg::No,
                stranded_qty,
                avg_cost,
                fair_win_prob: fair_value.p_down,
                best_exit_bid: snapshot.no_quote.best_bid.as_ref().map(|level| level.price),
                opposite_best_ask: snapshot
                    .yes_quote
                    .best_ask
                    .as_ref()
                    .map(|level| level.price),
            })
        }
    }
}

fn classify_fill_leg<M: MarketDescriptor>(market: &M, fill: &FillReport) -> Option<LadderLeg> {
    if &fill.instrument_id == market.yes_instrument_id() {
        Some(LadderLeg::Yes)
    } else if &fill.instrument_id == market.no_instrument_id() {
        Some(LadderLeg::No)
    } else {
        None
    }
}
