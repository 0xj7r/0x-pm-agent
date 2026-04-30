//! Bonereaper strategy: breadth-first paired-bid mid-tracker.
//!
//! Emulates the on-chain wallet 0xeebde7a0e019a63e6b476eb425505b7b3e6eba30
//! ("bonereaper"), a maker-active wallet that runs paired BUY bids across
//! 50+ short-duration crypto markets concurrently, holds to resolution
//! (no merges), and earns the bulk of P&L through Polymarket maker rebates.
//!
//! Behavior:
//! - On every market snapshot, post a BUY at `mid - 1 tick` for that leg.
//! - Quantity = clip_usd / price (≈$20 per leg by default), floored at the
//!   venue minimum order size.
//! - When mid drifts, the reconciler cancel-and-replaces under the fixed
//!   QuoteMatchKey for this strategy, producing the "bid walks with mid"
//!   fill distribution observed in the live wallet.
//! - Never emits SELL, MERGE, or reduce-only intents. REDEEM on resolution
//!   is handled by the runtime via the existing path.

use std::env;

use crate::core::types::{
    EpochMillis, IntentKind, MarketSnapshot, OrderIntent, RuntimeStatus, TradeSide,
};
use crate::strategy::{deterministic_client_order_id, Strategy, StrategyContext, StrategyDecision};

const DEFAULT_CLIP_USD: f64 = 20.0;
const DEFAULT_MIN_TICK: f64 = 0.01;
const DEFAULT_MIN_QTY: f64 = 5.0;
const DEFAULT_MAX_CONCURRENT_MARKETS: usize = 100;
const QUOTE_LEVEL_TAG: &str = "bonereaper-mid";

#[derive(Debug, Clone)]
pub struct BonereaperConfig {
    pub clip_usd: f64,
    pub max_concurrent_markets: usize,
    pub min_qty: f64,
    pub default_tick: f64,
}

impl Default for BonereaperConfig {
    fn default() -> Self {
        Self {
            clip_usd: DEFAULT_CLIP_USD,
            max_concurrent_markets: DEFAULT_MAX_CONCURRENT_MARKETS,
            min_qty: DEFAULT_MIN_QTY,
            default_tick: DEFAULT_MIN_TICK,
        }
    }
}

impl BonereaperConfig {
    pub fn from_env() -> Self {
        Self {
            clip_usd: env_f64("WHALE_PAIR_BONEREAPER_CLIP_USD", DEFAULT_CLIP_USD),
            max_concurrent_markets: env_usize(
                "WHALE_PAIR_BONEREAPER_MAX_CONCURRENT_MARKETS",
                DEFAULT_MAX_CONCURRENT_MARKETS,
            ),
            min_qty: env_f64("WHALE_PAIR_BONEREAPER_MIN_QTY", DEFAULT_MIN_QTY),
            default_tick: env_f64("WHALE_PAIR_BONEREAPER_DEFAULT_TICK", DEFAULT_MIN_TICK),
        }
    }
}

#[derive(Debug, Default)]
pub struct BonereaperStrategy {
    config: BonereaperConfig,
}

impl BonereaperStrategy {
    pub fn new(config: BonereaperConfig) -> Self {
        Self { config }
    }

    pub fn with_defaults() -> Self {
        Self::new(BonereaperConfig::from_env())
    }

    pub fn taker_fee_coeff(&self) -> f64 {
        0.0
    }

    pub fn config(&self) -> &BonereaperConfig {
        &self.config
    }

    fn build_intent(
        &self,
        snapshot: &MarketSnapshot,
        bid_price: f64,
        qty: f64,
        now_ms: EpochMillis,
    ) -> OrderIntent {
        OrderIntent {
            client_order_id: deterministic_client_order_id(
                "bonereaper",
                &snapshot.market_id,
                &snapshot.instrument_id,
                TradeSide::Buy,
                false,
                QUOTE_LEVEL_TAG,
                bid_price,
                qty,
            ),
            market_id: snapshot.market_id.clone(),
            instrument_id: snapshot.instrument_id.clone(),
            side: TradeSide::Buy,
            limit_price: bid_price,
            quantity: qty,
            reduce_only: false,
            reason: "bonereaper-mid-track".to_string(),
            quote_level_tag: Some(QUOTE_LEVEL_TAG.to_string()),
            created_at_ms: now_ms,
            pair_id: None,
            kind: IntentKind::Entry,
        }
    }
}

impl Strategy for BonereaperStrategy {
    fn name(&self) -> &str {
        "bonereaper"
    }

    fn on_market_snapshot(
        &mut self,
        context: &StrategyContext,
        snapshot: &MarketSnapshot,
    ) -> StrategyDecision {
        if !matches!(context.runtime_status, RuntimeStatus::Running) {
            return StrategyDecision::none();
        }

        let mid = match snapshot.quote.mid_price() {
            Some(m) if m > 0.0 && m < 1.0 => m,
            _ => return StrategyDecision::none(),
        };

        let tick = context
            .venue_rules
            .map(|r| r.minimum_tick_size)
            .filter(|t| *t > 0.0)
            .unwrap_or(self.config.default_tick);

        let bid_price = (mid - tick).max(tick);
        if !(bid_price.is_finite() && bid_price > 0.0 && bid_price < 1.0) {
            return StrategyDecision::none();
        }

        let raw_qty = self.config.clip_usd / bid_price;
        let min_qty = context
            .venue_rules
            .map(|r| r.minimum_order_size)
            .filter(|q| *q > 0.0)
            .unwrap_or(self.config.min_qty);
        let qty = raw_qty.max(min_qty).round();
        if qty <= 0.0 {
            return StrategyDecision::none();
        }

        StrategyDecision::single(self.build_intent(snapshot, bid_price, qty, context.now_ms))
    }
}

fn env_f64(key: &str, default: f64) -> f64 {
    env::var(key)
        .ok()
        .and_then(|raw| raw.parse::<f64>().ok())
        .unwrap_or(default)
}

fn env_usize(key: &str, default: usize) -> usize {
    env::var(key)
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::inventory::InventorySnapshot;
    use crate::core::types::{BookLevel, InstrumentId, MarketId, QuoteSnapshot, RuntimeStatus};
    use crate::strategy::VenueMarketRules;

    fn empty_inventory() -> InventorySnapshot {
        InventorySnapshot {
            free_cash_usd: 0.0,
            reserved_cash_usd: 0.0,
            total_cash_usd: 0.0,
            realized_pnl_usd: 0.0,
            gross_exposure_usd: 0.0,
            positions: Vec::new(),
        }
    }

    fn snap(mid: f64) -> MarketSnapshot {
        let bid = mid - 0.005;
        let ask = mid + 0.005;
        MarketSnapshot {
            market_id: MarketId::from("m1"),
            instrument_id: InstrumentId::from("up"),
            quote: QuoteSnapshot {
                best_bid: Some(BookLevel::new(bid, 100.0)),
                best_ask: Some(BookLevel::new(ask, 100.0)),
                bid_levels: vec![BookLevel::new(bid, 100.0)],
                ask_levels: vec![BookLevel::new(ask, 100.0)],
                depth_observed_at_ms: Some(1_000),
                last_trade_price: Some(mid),
                taker_buy_qty_60s: 0.0,
                taker_sell_qty_60s: 0.0,
                observed_at_ms: 1_000,
            },
        }
    }

    fn ctx_running(now_ms: EpochMillis) -> StrategyContext {
        StrategyContext {
            now_ms,
            runtime_status: RuntimeStatus::Running,
            inventory: empty_inventory(),
            open_orders_total: 0,
            open_orders_for_market: 0,
            market_context: None,
            unlawful_signal: None,
            btc_regime: crate::signals::BtcRegimeSnapshot::default(),
            venue_rules: Some(VenueMarketRules {
                minimum_order_size: 5.0,
                minimum_tick_size: 0.01,
                neg_risk: false,
            }),
        }
    }

    #[test]
    fn emits_bid_one_tick_below_mid() {
        let mut s = BonereaperStrategy::with_defaults();
        let dec = s.on_market_snapshot(&ctx_running(2_000), &snap(0.50));
        assert_eq!(dec.intents().len(), 1);
        let intent = &dec.intents()[0];
        assert_eq!(intent.side, TradeSide::Buy);
        assert!((intent.limit_price - 0.49).abs() < 1e-9);
        assert_eq!(intent.kind, IntentKind::Entry);
        assert!(!intent.reduce_only);
        assert_eq!(intent.quote_level_tag.as_deref(), Some("bonereaper-mid"));
    }

    #[test]
    fn skips_when_mid_is_unresolvable() {
        let mut s = BonereaperStrategy::with_defaults();
        let mut snapshot = snap(0.50);
        snapshot.quote.best_bid = None;
        snapshot.quote.best_ask = None;
        snapshot.quote.last_trade_price = None;
        let dec = s.on_market_snapshot(&ctx_running(2_000), &snapshot);
        assert!(dec.is_empty());
    }

    #[test]
    fn skips_when_runtime_not_running() {
        let mut s = BonereaperStrategy::with_defaults();
        let mut ctx = ctx_running(2_000);
        ctx.runtime_status = RuntimeStatus::RiskOff;
        let dec = s.on_market_snapshot(&ctx, &snap(0.50));
        assert!(dec.is_empty());
    }

    #[test]
    fn quantity_scales_inversely_with_price() {
        let mut s = BonereaperStrategy::new(BonereaperConfig {
            clip_usd: 20.0,
            ..BonereaperConfig::default()
        });
        let cheap = s.on_market_snapshot(&ctx_running(2_000), &snap(0.10));
        let expensive = s.on_market_snapshot(&ctx_running(2_000), &snap(0.90));
        assert!(cheap.intents()[0].quantity > expensive.intents()[0].quantity);
    }

    #[test]
    fn drift_changes_emitted_price() {
        let mut s = BonereaperStrategy::with_defaults();
        let a = s.on_market_snapshot(&ctx_running(2_000), &snap(0.50));
        let b = s.on_market_snapshot(&ctx_running(2_001), &snap(0.55));
        assert!(a.intents()[0].limit_price < b.intents()[0].limit_price);
    }
}
