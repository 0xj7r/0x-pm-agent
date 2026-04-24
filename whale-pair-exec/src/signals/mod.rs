pub mod btc_regime;
pub mod market_activity;
pub mod unlawful_gate;

pub use btc_regime::BtcRegimeSnapshot;
pub use market_activity::MarketActivitySignal;
pub use unlawful_gate::{
    evaluate_unlawful_mode, PairedBookSignal, SessionBucket, UnlawfulAggressionTier,
    UnlawfulExecutionMode, UnlawfulGateConfig, UnlawfulGateInputs, UnlawfulSignalSnapshot,
};
