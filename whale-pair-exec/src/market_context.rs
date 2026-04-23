use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::types::MarketId;

#[derive(Clone, Debug, Default, Deserialize)]
pub struct MarketContextRecord {
    pub market_id: String,
    #[serde(default)]
    pub instrument_ids: Vec<String>,
    #[serde(default)]
    pub price_to_beat: Option<f64>,
    #[serde(default)]
    pub final_price: Option<f64>,
    #[serde(default)]
    pub event_start_time_ms: Option<u64>,
    #[serde(default)]
    pub event_end_time_ms: Option<u64>,
}

#[derive(Clone, Debug, Default)]
pub struct MarketContextStore {
    by_market_id: HashMap<MarketId, MarketContextRecord>,
}

impl MarketContextStore {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn load_json(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read market context file {}", path.display()))?;
        let records: Vec<MarketContextRecord> = serde_json::from_str(&raw)
            .with_context(|| format!("failed to parse market context JSON {}", path.display()))?;
        let by_market_id = records
            .into_iter()
            .filter(|record| !record.market_id.trim().is_empty())
            .map(|record| (MarketId::from(record.market_id.clone()), record))
            .collect();
        Ok(Self { by_market_id })
    }

    pub fn get(&self, market_id: &MarketId) -> Option<&MarketContextRecord> {
        self.by_market_id.get(market_id)
    }

    pub fn len(&self) -> usize {
        self.by_market_id.len()
    }
}
