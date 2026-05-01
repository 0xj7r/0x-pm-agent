//! Signal module namespace for BTC regime, market activity, fair value,
//! and unlawful gate logic.

pub mod btc_regime;
pub mod fair_value;
pub mod incentives;
pub mod market_activity;
pub mod unlawful_gate;

pub use btc_regime::{BtcRegime, BtcRegimeSnapshot};
pub use fair_value::{
    estimate_fair_value, estimate_fair_value_with_momentum, FairValueCalibration,
    FairValueEstimate, FairValueModel,
};
pub use incentives::{IncentiveSignal, RewardScoringState};
pub use market_activity::MarketActivitySignal;
pub use unlawful_gate::{
    evaluate_unlawful_mode, PairedBookSignal, SessionBucket, UnlawfulAggressionTier,
    UnlawfulExecutionMode, UnlawfulGateConfig, UnlawfulGateInputs, UnlawfulSignalSnapshot,
};
