//! Signal module namespace for BTC regime, market activity, fair value,
//! and unlawful gate logic.

pub mod btc_regime;
pub mod fair_value;
pub mod market_activity;
pub mod unlawful_gate;

pub use btc_regime::{BtcRegime, BtcRegimeSnapshot};
pub use fair_value::{estimate_fair_value, FairValueEstimate, FairValueModel};
pub use market_activity::MarketActivitySignal;
pub use unlawful_gate::{
    evaluate_unlawful_mode, PairedBookSignal, SessionBucket, UnlawfulAggressionTier,
    UnlawfulExecutionMode, UnlawfulGateConfig, UnlawfulGateInputs, UnlawfulSignalSnapshot,
};
