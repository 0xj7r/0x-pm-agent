use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::types::{EpochMillis, MarketId};

/// One row in the operator-calibration dataset used to compare strategy
/// behavior against known profitable wallets and our own live/paper fills.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OperatorCalibrationRow {
    pub observed_at_ms: EpochMillis,
    pub market_id: MarketId,
    pub strategy: String,
    pub regime: String,
    pub maker_fill_usd: f64,
    pub taker_fill_usd: f64,
    pub merge_count: u64,
    pub merge_profit_usd: f64,
    pub rescue_count: u64,
    pub post_fill_markout_bps: f64,
}

impl OperatorCalibrationRow {
    pub fn partition_date_hour(&self) -> (String, String) {
        let observed_at = DateTime::<Utc>::from_timestamp_millis(self.observed_at_ms as i64)
            .unwrap_or(DateTime::<Utc>::UNIX_EPOCH);
        (
            observed_at.format("%Y-%m-%d").to_string(),
            observed_at.format("%H").to_string(),
        )
    }
}
