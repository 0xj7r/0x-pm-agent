//! Signal module namespace for BTC regime, fair value, incentives, and market activity.

pub mod book_sanity;
pub mod btc_regime;
pub mod cheap_leg;
pub mod fair_value;
pub mod incentives;
pub mod market_activity;
pub mod momentum;
pub mod order_book_pressure;
pub mod reversal;
pub mod side_score;

pub use book_sanity::{BookSanityConfig, BookSanityLeg, BookSanitySignal};
pub use btc_regime::{BtcRegime, BtcRegimeSnapshot};
pub use cheap_leg::{CheapLegConfig, CheapLegSignal, CheapLegSignalEngine};
pub use fair_value::{
    estimate_fair_value, estimate_fair_value_with_momentum, FairValueCalibration,
    FairValueEstimate, FairValueModel,
};
pub use incentives::{IncentiveSignal, RewardScoringState};
pub use market_activity::MarketActivitySignal;
pub use momentum::{MomentumConfig, MomentumEngine, MomentumSignal, SignalDirection};
pub use order_book_pressure::{
    OrderBookPressureConfig, OrderBookPressureEngine, OrderBookPressureSignal,
};
pub use reversal::{ReversalConfig, ReversalSignal};
pub use side_score::{SideScoreConfig, SideScoreLeg, SideScoreSignal};
