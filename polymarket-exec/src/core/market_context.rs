//! Static/dynamic market metadata loading used by strategy context and runtime decisions.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::types::MarketId;

const DEFAULT_MARKET_CONTEXT_VERSION: &str = "btc_5m_mm_v1";

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum MarketContextPayload {
    Versioned(MarketContextEnvelope),
    Legacy(Vec<MarketContextRecord>),
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
struct MarketContextEnvelope {
    #[serde(default)]
    version: String,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    source_generated_at_ms: Option<u64>,
    #[serde(default)]
    #[serde(alias = "market_contexts")]
    markets: Vec<MarketContextRecord>,
    #[serde(default)]
    active_btc_5m_windows: Vec<MarketContextRecord>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
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

impl MarketContextRecord {
    pub fn is_active_btc_5m_window(&self, now_ms: u64) -> bool {
        let Some(start_ms) = self.event_start_time_ms else {
            return false;
        };
        let Some(end_ms) = self.event_end_time_ms else {
            return false;
        };
        start_ms <= now_ms && now_ms <= end_ms
    }
}

#[derive(Clone, Debug, Default)]
pub struct MarketContextStore {
    by_market_id: HashMap<MarketId, MarketContextRecord>,
    pub version: String,
    pub source: Option<String>,
    pub source_generated_at_ms: Option<u64>,
}

impl MarketContextStore {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn from_records(
        records: Vec<MarketContextRecord>,
        source: Option<String>,
        source_generated_at_ms: Option<u64>,
    ) -> Self {
        let by_market_id = records
            .into_iter()
            .filter(|record| !record.market_id.trim().is_empty())
            .map(|record| (MarketId::from(record.market_id.clone()), record))
            .collect();

        Self {
            by_market_id,
            version: DEFAULT_MARKET_CONTEXT_VERSION.to_string(),
            source,
            source_generated_at_ms,
        }
    }

    pub fn load_json(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read market context file {}", path.display()))?;
        let payload: MarketContextPayload = serde_json::from_str(&raw)
            .with_context(|| format!("failed to parse market context JSON {}", path.display()))?;

        let (records, version, source, source_generated_at_ms) = match payload {
            MarketContextPayload::Versioned(envelope) => {
                let mut records = envelope.markets;
                records.extend(envelope.active_btc_5m_windows);
                (
                    records,
                    if envelope.version.is_empty() {
                        DEFAULT_MARKET_CONTEXT_VERSION.to_string()
                    } else {
                        envelope.version
                    },
                    envelope.source,
                    envelope.source_generated_at_ms,
                )
            }
            MarketContextPayload::Legacy(records) => (
                records,
                DEFAULT_MARKET_CONTEXT_VERSION.to_string(),
                None,
                None,
            ),
        };

        let by_market_id = records
            .into_iter()
            .filter(|record| !record.market_id.trim().is_empty())
            .map(|record| (MarketId::from(record.market_id.clone()), record))
            .collect();

        Ok(Self {
            by_market_id,
            version,
            source,
            source_generated_at_ms,
        })
    }

    pub fn get(&self, market_id: &MarketId) -> Option<&MarketContextRecord> {
        self.by_market_id.get(market_id)
    }

    pub fn active_btc_5m_windows(&self) -> Vec<&MarketContextRecord> {
        let now_ms = now_unix_ms();
        self.by_market_id
            .values()
            .filter(|record| record.is_active_btc_5m_window(now_ms))
            .collect()
    }

    pub fn records(&self) -> Vec<MarketContextRecord> {
        self.sorted_records()
    }

    pub fn asset_ids(&self) -> Vec<String> {
        let mut asset_ids = self
            .sorted_records()
            .into_iter()
            .flat_map(|record| record.instrument_ids)
            .filter(|asset_id| !asset_id.trim().is_empty())
            .collect::<Vec<_>>();
        asset_ids.dedup();
        asset_ids
    }

    pub fn market_ids(&self) -> Vec<String> {
        self.sorted_records()
            .into_iter()
            .map(|record| record.market_id)
            .filter(|market_id| !market_id.trim().is_empty())
            .collect()
    }

    pub fn asset_market_map(&self) -> HashMap<String, String> {
        let mut mapping = HashMap::new();
        for record in self.sorted_records() {
            for asset_id in record.instrument_ids {
                if !asset_id.trim().is_empty() {
                    mapping.insert(asset_id, record.market_id.clone());
                }
            }
        }
        mapping
    }

    pub fn len(&self) -> usize {
        self.by_market_id.len()
    }

    pub fn to_json_string(&self) -> Result<String> {
        let records = self.sorted_records();
        let source_generated_at_ms = self.source_generated_at_ms.or_else(|| Some(now_unix_ms()));
        let active_btc_5m_windows = records
            .iter()
            .filter(|record| {
                source_generated_at_ms.is_some_and(|now_ms| record.is_active_btc_5m_window(now_ms))
            })
            .cloned()
            .collect::<Vec<_>>();
        let envelope = MarketContextEnvelope {
            version: self.version.clone(),
            source: self.source.clone(),
            source_generated_at_ms,
            markets: records,
            active_btc_5m_windows,
        };
        Ok(serde_json::to_string_pretty(&envelope)?)
    }

    pub fn save_json(&self, path: &Path) -> Result<()> {
        let raw = self.to_json_string()?;
        fs::write(path, raw)
            .with_context(|| format!("failed to write market context file {}", path.display()))?;
        Ok(())
    }

    fn sorted_records(&self) -> Vec<MarketContextRecord> {
        let mut records = self.by_market_id.values().cloned().collect::<Vec<_>>();
        records.sort_by(|left, right| left.market_id.cmp(&right.market_id));
        records
    }
}

fn now_unix_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
