//! Signal module namespace for BTC regime, fair value, incentives, and market activity.

pub mod btc_regime;
pub mod fair_value;
pub mod incentives;
pub mod market_activity;

pub use btc_regime::{BtcRegime, BtcRegimeSnapshot};
pub use fair_value::{
    estimate_fair_value, estimate_fair_value_with_momentum, FairValueCalibration,
    FairValueEstimate, FairValueModel,
};
pub use incentives::{IncentiveSignal, RewardScoringState};
pub use market_activity::MarketActivitySignal;
