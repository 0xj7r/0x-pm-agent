//! Market-agnostic paired market-making algorithm.
//!
//! This module owns the reusable strategy mechanics:
//! - signal-driven paired ladder generation,
//! - Stoikov inventory skew,
//! - pair-cost / convexity accounting,
//! - EV-gated rescue decisions.
//!
//! Runtime and venue code remain outside this module. The module is pure and
//! deterministic: callers provide snapshots/signals and receive proposed
//! intents/decisions.

pub mod engine;
pub mod fill_automation;
pub mod fill_cooldown;
pub mod ladder_builder;
pub mod risk_boundary;
pub mod stoikov;

pub use crate::market_making::pairing::{
    choose_capital_recycle, choose_merge, choose_rescue, evaluate_hard_policy,
    CapitalRecycleConfig, CapitalRecycleDecision, HardPolicyAction, HardPolicyConfig,
    HardPolicyDecision, LadderLeg, LadderRegime, Leg, MergePolicyConfig, MergePolicyDecision,
    PairCostTracker, PairedInventorySnapshot, PairedMarketSnapshot, RescueAction, RescueConfig,
    RescueDecision, RescueInputs, RunningInventoryCaps,
};
pub use engine::{PairedMmDecision, PairedMmEngine, PairedMmEngineConfig, PairedMmInput};
pub use fill_automation::{
    AutoFillConfig, AutoFillDecision, AutoFillState, AutoFillStateSnapshot, AutoFillSuggestion,
};
pub use fill_cooldown::{FillCooldown, FillCooldownConfig, FillCooldownDecision};
pub use ladder_builder::{build_ladder, LadderBuildResult, LadderConfig, LadderDiagnostics};
pub use risk_boundary::{filter_entry_intents, PairedMmRiskDecision, PairedMmRiskReject};
pub use stoikov::{stoikov_reservation_price, StoikovParams};
