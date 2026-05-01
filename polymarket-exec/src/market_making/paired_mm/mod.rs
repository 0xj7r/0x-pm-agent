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

pub mod capital_recycler;
pub mod engine;
pub mod fill_automation;
pub mod fill_cooldown;
pub mod ladder_builder;
pub mod merge_policy;
pub mod pair_cost_tracker;
pub mod rescue_engine;
pub mod risk_boundary;
pub mod risk_policy;
pub mod stoikov;
pub mod types;

pub use capital_recycler::{choose_capital_recycle, CapitalRecycleConfig, CapitalRecycleDecision};
pub use engine::{PairedMmDecision, PairedMmEngine, PairedMmEngineConfig, PairedMmInput};
pub use fill_automation::{
    AutoFillConfig, AutoFillDecision, AutoFillState, AutoFillStateSnapshot, AutoFillSuggestion,
};
pub use fill_cooldown::{FillCooldown, FillCooldownConfig, FillCooldownDecision};
pub use ladder_builder::{build_ladder, LadderBuildResult, LadderConfig, LadderDiagnostics};
pub use merge_policy::{choose_merge, MergePolicyConfig, MergePolicyDecision};
pub use pair_cost_tracker::{Leg, PairCostTracker};
pub use rescue_engine::{choose_rescue, RescueAction, RescueConfig, RescueDecision, RescueInputs};
pub use risk_boundary::{filter_entry_intents, PairedMmRiskDecision, PairedMmRiskReject};
pub use risk_policy::{
    evaluate_hard_policy, HardPolicyAction, HardPolicyConfig, HardPolicyDecision,
};
pub use stoikov::{stoikov_reservation_price, StoikovParams};
pub use types::{
    LadderLeg, LadderRegime, PairedInventorySnapshot, PairedMarketSnapshot, RunningInventoryCaps,
};
