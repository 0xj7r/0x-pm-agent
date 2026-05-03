//! Runtime market registry for multi-market execution.

use std::collections::BTreeMap;

use crate::markets::descriptor::BinaryOutcomeMarket;
use crate::types::MarketId;

#[derive(Clone, Debug, Default)]
pub struct MarketRegistry {
    markets: BTreeMap<MarketId, BinaryOutcomeMarket>,
}

impl MarketRegistry {
    pub fn new() -> Self {
        Self {
            markets: BTreeMap::new(),
        }
    }

    pub fn insert(&mut self, market: BinaryOutcomeMarket) {
        self.markets.insert(market.market_id.clone(), market);
    }

    pub fn get(&self, market_id: &MarketId) -> Option<&BinaryOutcomeMarket> {
        self.markets.get(market_id)
    }

    pub fn active_at(&self, now_ms: u64) -> Vec<&BinaryOutcomeMarket> {
        self.markets
            .values()
            .filter(|market| {
                let start_ok = market.event_start_ms.is_none_or(|start| start <= now_ms);
                let end_ok = market.event_end_ms.is_none_or(|end| now_ms <= end);
                start_ok && end_ok
            })
            .collect()
    }

    /// Iterate every registered market regardless of bar-window state.
    /// Used by callers (e.g. the replay adapter) that need to enumerate
    /// markets the strategy is bound to without imposing a time gate.
    pub fn iter(&self) -> impl Iterator<Item = &BinaryOutcomeMarket> {
        self.markets.values()
    }

    pub fn len(&self) -> usize {
        self.markets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.markets.is_empty()
    }
}
