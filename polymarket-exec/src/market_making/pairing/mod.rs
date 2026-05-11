//! Shared YES/NO pairing primitives.
//!
//! These are used by both the Gabagool-style pair-cost arbitrage strategy and
//! the paired market-making overlay. They are intentionally outside
//! `paired_mm` so buy-light-side recycling, merge policy, and EV rescue are
//! not owned by the two-sided ladder strategy.

pub mod capital_recycler;
pub mod merge_executor;
pub mod merge_policy;
pub mod pair_cost_tracker;
pub mod pair_ledger;
pub mod rescue_engine;
pub mod risk_policy;
pub mod types;

pub use capital_recycler::{choose_capital_recycle, CapitalRecycleConfig, CapitalRecycleDecision};
pub use merge_executor::{MergeExecution, MergeExecutor};
pub use merge_policy::{choose_merge, MergePolicyConfig, MergePolicyDecision};
pub use pair_cost_tracker::{Leg, PairCostTracker};
pub use pair_ledger::{
    MarketPairLedger, MergeCandidate, MergePlan, ResolvedRedeemCandidate, ResolvedWinningLeg,
};
pub use rescue_engine::{choose_rescue, RescueAction, RescueConfig, RescueDecision, RescueInputs};
pub use risk_policy::{
    evaluate_hard_policy, HardPolicyAction, HardPolicyConfig, HardPolicyDecision,
};
pub use types::{
    LadderLeg, LadderRegime, PairedInventorySnapshot, PairedMarketSnapshot, RunningInventoryCaps,
};
