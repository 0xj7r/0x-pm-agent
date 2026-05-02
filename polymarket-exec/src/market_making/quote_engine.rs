//! Deterministic quote-set builder for multi-level two-sided market-making intents.

use std::collections::{BTreeMap, HashMap};

use crate::types::{EpochMillis, InstrumentId, MarketId, OrderIntent, QuoteSnapshot, TradeSide};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct QuoteEngineConfig {
    pub max_levels_per_side: usize,
    pub skew_bps: f64,
    pub stale_quote_max_age_ms: Option<u64>,
    pub quote_expiry_ms: Option<u64>,
}

impl Default for QuoteEngineConfig {
    fn default() -> Self {
        Self {
            max_levels_per_side: 16,
            skew_bps: 7.5,
            stale_quote_max_age_ms: None,
            quote_expiry_ms: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct DesiredQuote {
    pub intent: OrderIntent,
    pub level: usize,
    pub is_cleanup: bool,
    pub suppress_if_stale: bool,
    pub expires_at_ms: Option<EpochMillis>,
}

impl DesiredQuote {
    fn with_level(mut self, level: usize) -> Self {
        self.level = level;
        self
    }

    fn with_cleanup(mut self, is_cleanup: bool) -> Self {
        self.is_cleanup = is_cleanup;
        self
    }

    fn with_suppression(
        mut self,
        suppress_if_stale: bool,
        expires_at_ms: Option<EpochMillis>,
    ) -> Self {
        self.suppress_if_stale = suppress_if_stale;
        self.expires_at_ms = expires_at_ms;
        self
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DesiredQuoteSet {
    pub quotes: Vec<DesiredQuote>,
    pub stale_quote_max_age_ms: Option<u64>,
    pub quote_expiry_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug)]
pub enum StaleMode {
    Ignore,
    Remove,
}

#[derive(Clone, Copy, Debug)]
pub enum ExpiryMode {
    Ignore,
    Remove,
}

impl DesiredQuoteSet {
    fn bucket_id(intent: &OrderIntent) -> (MarketId, InstrumentId, TradeSide, bool) {
        (
            intent.market_id.clone(),
            intent.instrument_id.clone(),
            intent.side,
            intent.reduce_only,
        )
    }

    fn normalize_price(value: f64, decimals: u32) -> f64 {
        let scale = 10_f64.powi(decimals as i32);
        (value * scale).round() / scale
    }

    fn skew_for_side(price: f64, side: TradeSide, skew_bps: f64, level: usize) -> f64 {
        if level == 0 || !price.is_finite() || skew_bps <= 0.0 {
            return price;
        }
        let skew = (level as f64) * (skew_bps / 10_000.0);
        if skew <= 0.0 {
            return price;
        }
        match side {
            TradeSide::Buy => Self::normalize_price(price * (1.0 - skew), 8),
            TradeSide::Sell => Self::normalize_price(price * (1.0 + skew), 8),
        }
    }

    fn strategy_priority(intent: &OrderIntent) -> u8 {
        let tag = intent.quote_level_tag.as_deref().unwrap_or_default();
        if tag.starts_with("pair-cost-arb") {
            return 0;
        }
        if tag.starts_with("mm-capital-recycle") || intent.kind == crate::types::IntentKind::Close {
            return 1;
        }
        if tag.starts_with("mm-paired-bid") {
            return 10;
        }
        5
    }

    pub fn from_intents(mut intents: Vec<OrderIntent>, config: &QuoteEngineConfig) -> Self {
        // Detect strategies that emit a pre-laddered intent set. Their level_tags
        // (e.g. "mm-paired-bid:l1", "mm-paired-bid:l2") already encode the level,
        // and their prices are tick-aligned by maker_bid_price_at_level. The
        // legacy auto-skew in this function (skew_for_side) would push level >= 1
        // prices off-tick, which Polymarket silently drops at submit. The legacy
        // take(max_levels) cap also collapses N-level ladders into N=3.
        //
        // For pre-laddered intents we pass prices through verbatim and only cap
        // at max_levels_per_side as a sanity bound. For legacy single-intent
        // emissions (no per-level tag) we preserve the old fan-out behavior.
        let max_levels = config.max_levels_per_side.clamp(1, 32);

        let mut buckets: BTreeMap<(MarketId, InstrumentId, TradeSide, bool), Vec<OrderIntent>> =
            BTreeMap::new();
        for intent in intents.drain(..) {
            if intent.limit_price <= 0.0 || intent.quantity <= 0.0 {
                continue;
            }
            let key = Self::bucket_id(&intent);
            buckets.entry(key).or_default().push(intent);
        }

        let mut quotes = Vec::new();
        for (_key, mut bucket) in buckets {
            if bucket.is_empty() {
                continue;
            }
            let side = bucket[0].side;
            match side {
                TradeSide::Buy => bucket.sort_by(|a, b| {
                    Self::strategy_priority(a)
                        .cmp(&Self::strategy_priority(b))
                        .then_with(|| b.limit_price.total_cmp(&a.limit_price))
                }),
                TradeSide::Sell => bucket.sort_by(|a, b| {
                    Self::strategy_priority(a)
                        .cmp(&Self::strategy_priority(b))
                        .then_with(|| a.limit_price.total_cmp(&b.limit_price))
                }),
            }

            let pre_laddered = bucket.len() > 1
                || bucket
                    .first()
                    .and_then(|i| i.quote_level_tag.as_deref())
                    .is_some_and(|t| t.contains(":l"));

            for (level, mut intent) in bucket.into_iter().take(max_levels).enumerate() {
                if !pre_laddered {
                    intent.limit_price = Self::skew_for_side(
                        Self::normalize_price(intent.limit_price, 8),
                        side,
                        config.skew_bps,
                        level,
                    );
                }
                if intent.limit_price <= 0.0 || intent.quantity <= 0.0 {
                    continue;
                }
                let current_tag = intent
                    .quote_level_tag
                    .clone()
                    .unwrap_or_else(|| format!("level-{}", (level + 1)));
                intent.quote_level_tag = Some(format!("{current_tag}:{level}"));
                quotes.push(
                    DesiredQuote {
                        intent,
                        level,
                        is_cleanup: false,
                        suppress_if_stale: false,
                        expires_at_ms: None,
                    }
                    .with_level(level)
                    .with_cleanup(false)
                    .with_suppression(false, None),
                );
            }
        }

        quotes.sort_by(|left, right| {
            let left_key = (
                &left.intent.market_id,
                &left.intent.instrument_id,
                left.intent.side,
                left.intent.reduce_only,
            );
            let right_key = (
                &right.intent.market_id,
                &right.intent.instrument_id,
                right.intent.side,
                right.intent.reduce_only,
            );
            left_key
                .cmp(&right_key)
                .then_with(|| left.level.cmp(&right.level))
                .then_with(|| left.intent.limit_price.total_cmp(&right.intent.limit_price))
                .then_with(|| left.intent.quantity.total_cmp(&right.intent.quantity))
                .then_with(|| {
                    left.intent
                        .quote_level_tag
                        .as_deref()
                        .unwrap_or_default()
                        .cmp(right.intent.quote_level_tag.as_deref().unwrap_or_default())
                })
        });

        Self {
            quotes,
            stale_quote_max_age_ms: config.stale_quote_max_age_ms,
            quote_expiry_ms: config.quote_expiry_ms,
        }
    }

    pub fn with_stale_gate<F>(
        mut self,
        now_ms: EpochMillis,
        market_quotes: &HashMap<InstrumentId, QuoteSnapshot>,
        mode: StaleMode,
        stale_check: F,
    ) -> Self
    where
        F: Fn(Option<&QuoteSnapshot>, u64) -> bool,
    {
        if matches!(mode, StaleMode::Ignore) {
            return self;
        }

        let stale_quote_max_age_ms = self.stale_quote_max_age_ms;
        let quote_expiry_ms = self.quote_expiry_ms;
        self.quotes.retain(|desired| {
            if desired.suppress_if_stale {
                return false;
            }
            if stale_quote_max_age_ms.is_some_and(|max_age_ms| {
                now_ms.saturating_sub(desired.intent.created_at_ms) > max_age_ms
            }) {
                return false;
            }
            if quote_expiry_ms.is_some_and(|expiry_ms| {
                now_ms.saturating_sub(desired.intent.created_at_ms) > expiry_ms
            }) {
                return false;
            }
            let stale = stale_check(market_quotes.get(&desired.intent.instrument_id), now_ms);
            !stale
        });
        self
    }

    pub fn with_expiry_gate<F>(
        mut self,
        now_ms: EpochMillis,
        mode: ExpiryMode,
        expiry_check: F,
    ) -> Self
    where
        F: Fn(&DesiredQuote, EpochMillis) -> bool,
    {
        if matches!(mode, ExpiryMode::Ignore) {
            return self;
        }

        self.quotes.retain(|desired| {
            if desired
                .expires_at_ms
                .is_some_and(|expires_at_ms| now_ms >= expires_at_ms)
            {
                return false;
            }
            !expiry_check(desired, now_ms)
        });
        self
    }

    pub fn cleanup_intents(self) -> Vec<OrderIntent> {
        self.quotes
            .into_iter()
            .filter(|desired| desired.is_cleanup)
            .map(|quote| quote.intent)
            .collect()
    }

    pub fn intents(self) -> Vec<OrderIntent> {
        self.quotes
            .into_iter()
            .map(|desired| desired.intent)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{DesiredQuote, DesiredQuoteSet, ExpiryMode, QuoteEngineConfig, StaleMode};
    use crate::types::{
        ClientOrderId, InstrumentId, MarketId, OrderIntent, QuoteSnapshot, TradeSide,
    };
    use std::collections::HashMap;

    fn intent(instrument: &str, side: TradeSide, price: f64, tag: Option<&str>) -> OrderIntent {
        OrderIntent {
            client_order_id: ClientOrderId::from(format!("{instrument}-{price}")),
            market_id: MarketId::from("m"),
            instrument_id: InstrumentId::from(instrument),
            side,
            limit_price: price,
            quantity: 1.0,
            reduce_only: false,
            reason: "test".to_string(),
            quote_level_tag: tag.map(ToString::to_string),
            created_at_ms: 1,
            pair_id: None,
            kind: crate::types::IntentKind::Entry,
        }
    }

    #[test]
    fn desired_quote_set_deduplicates_to_three_levels_per_side() {
        let config = QuoteEngineConfig {
            max_levels_per_side: 3,
            skew_bps: 10.0,
            ..QuoteEngineConfig::default()
        };
        let intents = vec![
            intent("inst", TradeSide::Buy, 0.10, Some("l0")),
            intent("inst", TradeSide::Buy, 0.11, Some("l1")),
            intent("inst", TradeSide::Buy, 0.12, Some("l2")),
            intent("inst", TradeSide::Buy, 0.13, Some("l3")),
        ];
        let desired = DesiredQuoteSet::from_intents(intents, &config);
        assert_eq!(desired.quotes.len(), 3);
        assert_eq!(desired.quotes[0].level, 0);
        assert_eq!(desired.quotes[1].level, 1);
        assert_eq!(desired.quotes[2].level, 2);
    }

    #[test]
    fn desired_quote_set_prioritizes_pair_cost_over_mm_overlay_within_level_cap() {
        let config = QuoteEngineConfig {
            max_levels_per_side: 3,
            skew_bps: 0.0,
            ..QuoteEngineConfig::default()
        };
        let intents = vec![
            intent("inst", TradeSide::Buy, 0.70, Some("mm-paired-bid:yes:l1")),
            intent("inst", TradeSide::Buy, 0.69, Some("mm-paired-bid:yes:l2")),
            intent("inst", TradeSide::Buy, 0.68, Some("mm-paired-bid:yes:l3")),
            intent(
                "inst",
                TradeSide::Buy,
                0.60,
                Some("pair-cost-arb:cheap-leg:yes"),
            ),
        ];

        let desired = DesiredQuoteSet::from_intents(intents, &config);
        let tags = desired
            .quotes
            .iter()
            .map(|quote| quote.intent.quote_level_tag.as_deref().unwrap_or_default())
            .collect::<Vec<_>>();

        assert_eq!(desired.quotes.len(), 3);
        assert!(
            tags.iter()
                .any(|tag| tag.starts_with("pair-cost-arb:cheap-leg:yes")),
            "{tags:?}"
        );
    }

    #[test]
    fn expiry_gate_removes_expired_instruments() {
        let desired = DesiredQuoteSet {
            quotes: vec![DesiredQuote {
                intent: intent("inst", TradeSide::Buy, 0.10, None),
                level: 0,
                is_cleanup: false,
                suppress_if_stale: false,
                expires_at_ms: Some(8),
            }],
            stale_quote_max_age_ms: None,
            quote_expiry_ms: Some(8),
        }
        .with_expiry_gate(10, ExpiryMode::Remove, |desired, now| {
            desired
                .expires_at_ms
                .is_some_and(|expires_at_ms| now >= expires_at_ms)
        });
        assert!(desired.quotes.is_empty());
    }

    #[test]
    fn stale_gate_removes_stale_instruments() {
        let mut quotes = HashMap::new();
        quotes.insert(
            InstrumentId::from("inst"),
            QuoteSnapshot {
                best_bid: None,
                best_ask: None,
                bid_levels: Vec::new(),
                ask_levels: Vec::new(),
                depth_observed_at_ms: None,
                last_trade_price: None,
                taker_buy_qty_60s: 0.0,
                taker_sell_qty_60s: 0.0,
                observed_at_ms: 1,
            },
        );
        let desired = DesiredQuoteSet::from_intents(
            vec![intent("inst", TradeSide::Buy, 0.10, None)],
            &QuoteEngineConfig::default(),
        )
        .with_stale_gate(10_000, &quotes, StaleMode::Remove, |snapshot, now| {
            snapshot.is_none_or(|snapshot| now.saturating_sub(snapshot.observed_at_ms) > 1)
        });
        assert!(desired.quotes.is_empty());
    }
}
