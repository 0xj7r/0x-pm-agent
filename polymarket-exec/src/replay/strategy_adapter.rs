//! Adapter that drives a `StrategyRegistry` from canonical `Event` records.
//!
//! Replaces `NoopStrategy` in `backtest_runner`: each event flows through a
//! minimal book/regime aggregator, the strategy is asked for decisions, and
//! emitted `OrderIntent`s are converted into `StrategyOrderIntent`s the
//! `FillSimulator` understands. Fills returned by the simulator are
//! converted to `FillReport` records and fed back into the registry's
//! `on_fill` path so the strategy can react.
//!
//! Determinism: all maps are `BTreeMap`. No wall-clock reads. All inputs
//! flow from event payloads, so two replays of the same input stream
//! produce byte-identical decisions.
//!
//! Out of scope (deferred to live trader): runtime risk gates, execution
//! adapter calls, journal/checkpoint side effects. The replay path applies
//! the strategy decisions directly to the simulator.

use std::collections::{BTreeMap, HashMap, VecDeque};

use serde_json::json;
use serde_yaml::Value as YamlValue;

use crate::collector::schema::{Event, EventType, Source};
use crate::core::types::{
    BookLevel, ClientOrderId, FillLiquidity, FillReport, InstrumentId, IntentKind, MarketId,
    OrderIntent, QuoteSnapshot, StrategyDecision, TradeSide,
};
use crate::inventory::InventoryState as RuntimeInventoryState;
use crate::market_making::pairing::pair_cost_tracker::PairCostTracker;
use crate::market_making::pairing::types::{PairedInventorySnapshot, PairedMarketSnapshot};
use crate::market_making::quote_reconciler::{QuoteAction, QuoteReconciler};
use crate::markets::{BinaryOutcomeMarket, MarketDescriptor, MarketRegistry, UnderlyingAsset};
use crate::quote_engine::{DesiredQuote, DesiredQuoteSet};
use crate::replay::fill_sim::{Side, SimulatedFill, StrategyOrderIntent};
use crate::replay::journal::JournalEvent;
use crate::replay::risk_trace::RiskRejection;
use crate::replay::runner::{ReplayDecision, ReplayStrategy};
use crate::risk::{RiskContext, RiskEngine};
use crate::runtime::state_store::{InMemoryRuntimeStateStore, RuntimeStateStore};
use crate::runtime::types::ManagedOrder;
use crate::signals::fair_value::NoSignalReason;
use crate::signals::{BtcRegimeSnapshot, FairValueEstimate, FairValueModel};
use crate::strategies::traits::{StrategyFillInput, StrategyInput};
use crate::strategies::{PairCostArbStrategyConfig, PairedMmStrategyConfig, StrategyRegistry};
use crate::strategy_profile::StrategyProfile;

const DEFAULT_STARTING_CASH_USD: f64 = 1_000.0;

/// Per-asset book aggregation. Held by `BookAggregator`.
#[derive(Clone, Debug, Default)]
struct AssetBook {
    bids: BTreeMap<i64, f64>,
    asks: BTreeMap<i64, f64>,
    last_trade_price: Option<f64>,
    observed_at_ms: u64,
}

impl AssetBook {
    fn snapshot(&self) -> QuoteSnapshot {
        let mut bid_levels: Vec<BookLevel> = self
            .bids
            .iter()
            .filter(|(_, sz)| **sz > 0.0)
            .map(|(p, sz)| BookLevel::new(ticks_to_price(*p), *sz))
            .collect();
        bid_levels.sort_by(|a, b| {
            b.price
                .partial_cmp(&a.price)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut ask_levels: Vec<BookLevel> = self
            .asks
            .iter()
            .filter(|(_, sz)| **sz > 0.0)
            .map(|(p, sz)| BookLevel::new(ticks_to_price(*p), *sz))
            .collect();
        ask_levels.sort_by(|a, b| {
            a.price
                .partial_cmp(&b.price)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let best_bid = bid_levels.first().cloned();
        let best_ask = ask_levels.first().cloned();
        QuoteSnapshot {
            best_bid,
            best_ask,
            bid_levels,
            ask_levels,
            depth_observed_at_ms: Some(self.observed_at_ms),
            last_trade_price: self.last_trade_price,
            taker_buy_qty_60s: 0.0,
            taker_sell_qty_60s: 0.0,
            observed_at_ms: self.observed_at_ms,
        }
    }
}

#[derive(Clone, Debug, Default)]
struct BookAggregator {
    books: BTreeMap<String, AssetBook>,
}

impl BookAggregator {
    fn apply(&mut self, event: &Event) {
        let Some(asset_id) = event.asset_id.as_deref() else {
            return;
        };
        let event_ms = (event.received_ns / 1_000_000) as u64;
        let entry = self.books.entry(asset_id.to_string()).or_default();
        entry.observed_at_ms = event_ms;
        match event.event_type {
            EventType::BookSnapshot => {
                if let (Some(price), Some(size), Some(side)) = (
                    parse_f64(event.price.as_deref()),
                    parse_f64(event.size.as_deref()),
                    side_of(event.side.as_deref()),
                ) {
                    let map = match side {
                        TradeSide::Buy => &mut entry.bids,
                        TradeSide::Sell => &mut entry.asks,
                    };
                    let key = price_to_ticks(price);
                    if size <= 0.0 {
                        map.remove(&key);
                    } else {
                        map.insert(key, size);
                    }
                }
            }
            EventType::BookDelta => {
                if let (Some(price), Some(size), Some(side)) = (
                    parse_f64(event.price.as_deref()),
                    parse_f64(event.size.as_deref()),
                    side_of(event.side.as_deref()),
                ) {
                    let map = match side {
                        TradeSide::Buy => &mut entry.bids,
                        TradeSide::Sell => &mut entry.asks,
                    };
                    let key = price_to_ticks(price);
                    if size <= 0.0 {
                        map.remove(&key);
                    } else {
                        map.insert(key, size);
                    }
                }
            }
            EventType::Trade => {
                if let Some(price) = parse_f64(event.price.as_deref()) {
                    entry.last_trade_price = Some(price);
                }
            }
            _ => {}
        }
    }

    fn snapshot(&self, asset_id: &str) -> Option<QuoteSnapshot> {
        self.books.get(asset_id).map(AssetBook::snapshot)
    }
}

/// Lightweight BTC regime tracker built from `btc_tick` events. Mirrors the
/// shape live runtime fills out (last_price plus realized vol approximation
/// over a 5m window).
#[derive(Clone, Debug)]
struct BtcRegimeAggregator {
    last_price: Option<f64>,
    observed_at_ms: u64,
    window_30s: BtcRollingWindow,
    window_60s: BtcRollingWindow,
    window_120s: BtcRollingWindow,
    window_180s: BtcRollingWindow,
    window_5m: BtcRollingWindow,
    window_15m: BtcRollingWindow,
    cached_snapshot: Option<BtcRegimeSnapshot>,
}

#[derive(Clone, Debug)]
struct BtcRollingWindow {
    window_ms: u64,
    prices: VecDeque<(u64, f64)>,
    returns: VecDeque<(u64, f64)>,
    sum_return: f64,
    sum_sq_return: f64,
}

impl BtcRollingWindow {
    fn new(window_ms: u64) -> Self {
        Self {
            window_ms,
            prices: VecDeque::new(),
            returns: VecDeque::new(),
            sum_return: 0.0,
            sum_sq_return: 0.0,
        }
    }

    fn push(&mut self, now_ms: u64, price: f64, previous_price: Option<f64>) {
        self.prices.push_back((now_ms, price));
        if let Some(prev) = previous_price {
            if prev > 0.0 && price > 0.0 {
                let ret = (price / prev).ln();
                if ret.is_finite() {
                    self.returns.push_back((now_ms, ret));
                    self.sum_return += ret;
                    self.sum_sq_return += ret * ret;
                }
            }
        }
        self.evict(now_ms);
    }

    fn evict(&mut self, now_ms: u64) {
        let cutoff = now_ms.saturating_sub(self.window_ms);
        while self
            .prices
            .front()
            .map(|(ts, _)| *ts < cutoff)
            .unwrap_or(false)
        {
            self.prices.pop_front();
        }
        while self
            .returns
            .front()
            .map(|(ts, _)| *ts < cutoff)
            .unwrap_or(false)
        {
            if let Some((_, ret)) = self.returns.pop_front() {
                self.sum_return -= ret;
                self.sum_sq_return -= ret * ret;
            }
        }
    }

    fn trade_count(&self) -> u64 {
        self.prices.len() as u64
    }

    fn realized_vol_bps(&self) -> Option<f64> {
        let count = self.returns.len();
        if count == 0 {
            return None;
        }
        let mean = self.sum_return / count as f64;
        let variance = (self.sum_sq_return / count as f64 - mean * mean).max(0.0);
        Some(variance.sqrt() * 10_000.0)
    }

    fn return_bps(&self) -> Option<f64> {
        let earliest = self.prices.front()?.1;
        let latest = self.prices.back()?.1;
        if earliest <= 0.0 || latest <= 0.0 {
            return None;
        }
        Some((latest / earliest).ln() * 10_000.0)
    }
}

impl Default for BtcRegimeAggregator {
    fn default() -> Self {
        Self {
            last_price: None,
            observed_at_ms: 0,
            window_30s: BtcRollingWindow::new(Self::WINDOW_MS_30S),
            window_60s: BtcRollingWindow::new(Self::WINDOW_MS_60S),
            window_120s: BtcRollingWindow::new(Self::WINDOW_MS_120S),
            window_180s: BtcRollingWindow::new(Self::WINDOW_MS_180S),
            window_5m: BtcRollingWindow::new(Self::WINDOW_MS_5M),
            window_15m: BtcRollingWindow::new(Self::WINDOW_MS_15M),
            cached_snapshot: None,
        }
    }
}

impl BtcRegimeAggregator {
    const WINDOW_MS_5M: u64 = 5 * 60 * 1_000;
    const WINDOW_MS_15M: u64 = 15 * 60 * 1_000;
    const WINDOW_MS_30S: u64 = 30 * 1_000;
    const WINDOW_MS_60S: u64 = 60 * 1_000;
    const WINDOW_MS_120S: u64 = 120 * 1_000;
    const WINDOW_MS_180S: u64 = 180 * 1_000;

    fn apply(&mut self, event: &Event) {
        if event.event_type != EventType::BtcTick {
            return;
        }
        let Some(price) = parse_f64(event.price.as_deref()) else {
            return;
        };
        let now_ms = (event.received_ns / 1_000_000) as u64;
        let previous_price = self.last_price;
        self.observed_at_ms = now_ms;
        self.last_price = Some(price);
        self.window_30s.push(now_ms, price, previous_price);
        self.window_60s.push(now_ms, price, previous_price);
        self.window_120s.push(now_ms, price, previous_price);
        self.window_180s.push(now_ms, price, previous_price);
        self.window_5m.push(now_ms, price, previous_price);
        self.window_15m.push(now_ms, price, previous_price);
        self.cached_snapshot = Some(self.compute_snapshot());
    }

    fn last_price(&self) -> Option<f64> {
        self.last_price
    }

    fn snapshot(&self) -> BtcRegimeSnapshot {
        self.cached_snapshot
            .clone()
            .unwrap_or_else(|| self.compute_snapshot())
    }

    fn compute_snapshot(&self) -> BtcRegimeSnapshot {
        BtcRegimeSnapshot {
            last_price: self.last_price(),
            realized_vol_5m_bps: self.window_5m.realized_vol_bps(),
            realized_vol_15m_bps: self.window_15m.realized_vol_bps(),
            trade_count_5m: self.window_5m.trade_count(),
            trade_count_15m: self.window_15m.trade_count(),
            return_30s_bps: self.window_30s.return_bps(),
            return_60s_bps: self.window_60s.return_bps(),
            return_120s_bps: self.window_120s.return_bps(),
            return_180s_bps: self.window_180s.return_bps(),
            observed_at_ms: self.observed_at_ms,
        }
    }
}

/// Per-market inventory accumulator. Mirrors the live runtime's
/// `PairedInventorySnapshot` updates: buys add cost-weighted quantity, sells
/// remove at cost basis.
#[derive(Clone, Debug)]
struct InventoryState {
    yes_qty: f64,
    yes_avg_cost: f64,
    no_qty: f64,
    no_avg_cost: f64,
    free_cash_usd: f64,
}

impl InventoryState {
    fn new(starting_cash_usd: f64) -> Self {
        Self {
            yes_qty: 0.0,
            yes_avg_cost: 0.0,
            no_qty: 0.0,
            no_avg_cost: 0.0,
            free_cash_usd: starting_cash_usd,
        }
    }

    fn snapshot(&self) -> PairedInventorySnapshot {
        PairedInventorySnapshot {
            yes_qty: self.yes_qty,
            no_qty: self.no_qty,
            yes_avg_cost: self.yes_avg_cost,
            no_avg_cost: self.no_avg_cost,
            free_cash_usd: self.free_cash_usd,
            equity_usd: self.free_cash_usd
                + self.yes_qty * self.yes_avg_cost
                + self.no_qty * self.no_avg_cost,
        }
    }

    fn apply_fill(&mut self, leg: Leg, side: TradeSide, price: f64, qty: f64) {
        if qty <= 0.0 || price <= 0.0 {
            return;
        }
        let notional = price * qty;
        match (leg, side) {
            (Leg::Yes, TradeSide::Buy) => {
                let cost = self.yes_qty * self.yes_avg_cost + notional;
                self.yes_qty += qty;
                self.yes_avg_cost = if self.yes_qty > 0.0 {
                    cost / self.yes_qty
                } else {
                    0.0
                };
                self.free_cash_usd -= notional;
            }
            (Leg::Yes, TradeSide::Sell) => {
                self.yes_qty = (self.yes_qty - qty).max(0.0);
                self.free_cash_usd += notional;
                if self.yes_qty <= 0.0 {
                    self.yes_avg_cost = 0.0;
                }
            }
            (Leg::No, TradeSide::Buy) => {
                let cost = self.no_qty * self.no_avg_cost + notional;
                self.no_qty += qty;
                self.no_avg_cost = if self.no_qty > 0.0 {
                    cost / self.no_qty
                } else {
                    0.0
                };
                self.free_cash_usd -= notional;
            }
            (Leg::No, TradeSide::Sell) => {
                self.no_qty = (self.no_qty - qty).max(0.0);
                self.free_cash_usd += notional;
                if self.no_qty <= 0.0 {
                    self.no_avg_cost = 0.0;
                }
            }
        }
    }

    fn apply_merge(&mut self, qty: f64, cash_usd: f64, fee_usd: f64, gas_usd: f64) -> bool {
        if qty <= 0.0 || cash_usd < 0.0 || fee_usd < 0.0 || gas_usd < 0.0 {
            return false;
        }
        if self.yes_qty + 1e-9 < qty || self.no_qty + 1e-9 < qty {
            return false;
        }
        self.yes_qty -= qty;
        self.no_qty -= qty;
        if self.yes_qty <= 1e-9 {
            self.yes_qty = 0.0;
            self.yes_avg_cost = 0.0;
        }
        if self.no_qty <= 1e-9 {
            self.no_qty = 0.0;
            self.no_avg_cost = 0.0;
        }
        self.free_cash_usd += cash_usd - fee_usd - gas_usd;
        true
    }
}

#[derive(Clone, Copy, Debug)]
enum Leg {
    Yes,
    No,
}

/// A `ReplayStrategy` that drives a real `StrategyRegistry` over the event
/// stream. Constructed via `ReplayStrategyAdapter::from_profile`.
pub struct ReplayStrategyAdapter {
    profile: StrategyProfile,
    registry: StrategyRegistry,
    markets: MarketRegistry,
    books: BookAggregator,
    btc_regime: BtcRegimeAggregator,
    inventories: BTreeMap<MarketId, InventoryState>,
    /// Live runtime inventory mirror per market. Mirrors the live trader's
    /// `core::inventory::InventoryState` so the `RiskEngine` evaluates
    /// against the same shapes the live trader sees.
    runtime_inventories: BTreeMap<MarketId, RuntimeInventoryState>,
    risk: RiskEngine,
    /// Per-market open-intent counters, updated as the simulator accepts
    /// intents and decremented on fills. Feeds `RiskContext` so the
    /// `max_open_orders_*` caps fire correctly.
    open_orders_total: usize,
    open_orders_per_market: BTreeMap<MarketId, usize>,
    /// Whether risk-engine evaluation runs in-band on every intent. Enabled
    /// by default so replay cannot submit orders that the live runtime would
    /// reject for cash, gross exposure, or open-order caps. Unit scenarios
    /// that need the old direct-to-simulator behavior can opt out.
    risk_evaluation_enabled: bool,
    /// Track open intents so we can map `SimulatedFill.client_order_id` back
    /// to the originating `OrderIntent`'s leg, side, and market.
    open_intents: BTreeMap<String, IntentRecord>,
    runtime_state: InMemoryRuntimeStateStore,
    quote_reconciler: QuoteReconciler,
    /// Map from the STRATEGY's stable client_order_id → the adapter's
    /// last-issued unique simulator coid for that slot. Re-submits of
    /// the same strategy slot emit an implicit cancel of the prior
    /// adapter coid, mirroring the live runtime's replace semantics.
    coid_by_strategy_slot: BTreeMap<String, String>,
    starting_cash_usd: f64,
    /// Last on_tick time per market, used to throttle on_tick calls. Without
    /// throttling we would call the strategy on every event which is the
    /// live-runtime contract, but it produces duplicate decisions on tight
    /// streams. We follow live: call on every (book_delta | book_snapshot |
    /// trade) for the relevant asset, plus every btc_tick.
    sequence: u64,
}

#[derive(Clone, Debug)]
struct IntentRecord {
    market_id: MarketId,
    instrument_id: InstrumentId,
    side: TradeSide,
    limit_price: f64,
    quantity: f64,
    reduce_only: bool,
    quote_level_tag: Option<String>,
    pair_id: Option<String>,
    kind: IntentKind,
    leg: Leg,
}

impl IntentRecord {
    fn from_intent(intent: &OrderIntent, leg: Leg) -> Self {
        Self {
            market_id: intent.market_id.clone(),
            instrument_id: intent.instrument_id.clone(),
            side: intent.side,
            limit_price: intent.limit_price,
            quantity: intent.quantity,
            reduce_only: intent.reduce_only,
            quote_level_tag: intent.quote_level_tag.clone(),
            pair_id: intent.pair_id.clone(),
            kind: intent.kind,
            leg,
        }
    }
}

impl ReplayStrategyAdapter {
    /// Build the adapter from a parsed profile. The strategy registry is
    /// populated lazily, when `market_meta` events are observed (so each
    /// market in the input stream gets the strategy attached). This mirrors
    /// the live runtime's per-market binding via `register_paired_mm` /
    /// `register_pair_cost_arb`.
    pub fn from_profile(profile: StrategyProfile) -> Self {
        let limits = profile.risk_limits();
        Self {
            profile,
            registry: StrategyRegistry::new(),
            markets: MarketRegistry::new(),
            books: BookAggregator::default(),
            btc_regime: BtcRegimeAggregator::default(),
            inventories: BTreeMap::new(),
            runtime_inventories: BTreeMap::new(),
            risk: RiskEngine::new(limits),
            open_orders_total: 0,
            open_orders_per_market: BTreeMap::new(),
            risk_evaluation_enabled: true,
            open_intents: BTreeMap::new(),
            runtime_state: InMemoryRuntimeStateStore::new(),
            quote_reconciler: QuoteReconciler::default(),
            coid_by_strategy_slot: BTreeMap::new(),
            starting_cash_usd: DEFAULT_STARTING_CASH_USD,
            sequence: 0,
        }
    }

    /// Override the replay cash baseline used by strategy input snapshots,
    /// runtime inventory reservations, and risk evaluation.
    pub fn with_starting_cash_usd(mut self, starting_cash_usd: f64) -> Self {
        self.starting_cash_usd = if starting_cash_usd.is_finite() {
            starting_cash_usd
        } else {
            DEFAULT_STARTING_CASH_USD
        };
        self
    }

    /// Enable or disable in-band `RiskEngine` evaluation of every intent.
    pub fn with_risk_evaluation(mut self, enabled: bool) -> Self {
        self.risk_evaluation_enabled = enabled;
        self
    }

    /// Test/inspection accessor.
    pub fn market_count(&self) -> usize {
        self.markets.len()
    }

    /// Test/inspection accessor.
    pub fn strategy_count(&self) -> usize {
        self.registry.len()
    }

    /// Test/inspection accessor: returns the most recently observed
    /// `StrategyInput` for a market. Returns None if no quotes/markets are
    /// registered yet. Used by debugging tests to verify wiring.
    pub fn build_input_snapshot(
        &self,
        market_id: &MarketId,
        now_ms: u64,
    ) -> Option<StrategyInput<BinaryOutcomeMarket>> {
        let market = self.markets.get(market_id)?.clone();
        self.build_input_for_market(&market, now_ms)
    }

    fn enabled_strategy(&self) -> EnabledStrategy {
        match self.profile.strategy.as_deref() {
            Some("pair_cost_arb") => EnabledStrategy::PairCostArb,
            _ => EnabledStrategy::PairedMm,
        }
    }

    fn register_market_if_needed(&mut self, market: BinaryOutcomeMarket) {
        if self.markets.get(&market.market_id).is_some() {
            return;
        }
        let market_id = market.market_id.clone();
        self.markets.insert(market.clone());
        match self.enabled_strategy() {
            EnabledStrategy::PairedMm => {
                let cfg: PairedMmStrategyConfig = self.profile.paired_mm_config();
                self.registry.register_paired_mm(market_id.as_str(), cfg);
            }
            EnabledStrategy::PairCostArb => {
                let cfg: PairCostArbStrategyConfig = self.profile.pair_cost_arb_config();
                self.registry
                    .register_pair_cost_arb(market_id.as_str(), cfg);
            }
        }
    }

    /// Apply a synthesised `price_to_beat` to the matching registered market
    /// so the fair-value model has a finite strike to plug into the BSM
    /// formula. Without this the late-asymmetric-convex overlay never fires
    /// because every fair-value evaluation falls back to NoSignal::StrikeInvalid.
    fn handle_price_to_beat(&mut self, event: &Event) {
        let Some(market_slug) = event.market_slug.as_deref() else {
            return;
        };
        let raw = &event.raw;
        // Prefer an explicit `strike` (live data API path); fall back to the
        // synthesizer's `btc_price_at_window_open_usd` because btc-updown-5m
        // markets do not carry a fixed strike in the metadata - the strike IS
        // the BTC price at window open.
        let strike = raw
            .get("strike")
            .and_then(|v| v.as_f64())
            .or_else(|| {
                raw.get("btc_price_at_window_open_usd")
                    .and_then(|v| match v {
                        serde_json::Value::Number(n) => n.as_f64(),
                        serde_json::Value::String(s) => s.parse::<f64>().ok(),
                        _ => None,
                    })
            });
        let Some(strike) = strike.filter(|p| p.is_finite() && *p > 0.0) else {
            return;
        };
        let market_id = MarketId::new(market_slug);
        if let Some(market) = self.markets.get_mut(&market_id) {
            market.price_to_beat = Some(strike);
            crate::market_making::paired_mm::engine::PRICE_TO_BEAT_DELIVERED
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        } else {
            crate::market_making::paired_mm::engine::PRICE_TO_BEAT_NO_MARKET
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn handle_market_meta(&mut self, event: &Event) {
        let Some(market_slug) = event.market_slug.as_deref() else {
            return;
        };
        let market_id = MarketId::new(market_slug);
        let raw = &event.raw;
        let asset_ids = raw
            .get("asset_ids")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if asset_ids.len() < 2 {
            return;
        }
        let yes_id = InstrumentId::new(asset_ids[0].as_str().unwrap_or_default());
        let no_id = InstrumentId::new(asset_ids[1].as_str().unwrap_or_default());
        if yes_id.is_empty() || no_id.is_empty() {
            return;
        }
        let mut market = match event.market_type.as_str() {
            "btc_15m" => BinaryOutcomeMarket::btc_15m(market_id.clone(), yes_id, no_id),
            "eth_5m" => BinaryOutcomeMarket::eth_5m(market_id.clone(), yes_id, no_id),
            "eth_15m" => BinaryOutcomeMarket::eth_15m(market_id.clone(), yes_id, no_id),
            _ => BinaryOutcomeMarket::btc_5m(market_id.clone(), yes_id, no_id),
        };
        market.price_to_beat = raw.get("strike").and_then(|v| v.as_f64());
        market.event_end_ms = raw
            .get("end_time_ms")
            .and_then(|v| v.as_i64())
            .map(|v| v as u64);
        if let Some(end) = market.event_end_ms {
            market.event_start_ms = Some(end.saturating_sub(market.tenor.window_ms()));
        }
        self.register_market_if_needed(market);
    }

    /// All markets the adapter knows about. Used to drive on_tick across the
    /// registry on every relevant event. We do not gate by `active_at` here:
    /// the adapter's job is to drive the strategy whenever it has an input,
    /// and the strategy itself owns end-of-bar/late-window logic.
    fn all_markets(&self) -> Vec<BinaryOutcomeMarket> {
        self.markets.iter().cloned().collect()
    }

    fn build_input_for_market(
        &self,
        market: &BinaryOutcomeMarket,
        now_ms: u64,
    ) -> Option<StrategyInput<BinaryOutcomeMarket>> {
        let yes_quote = self.books.snapshot(market.yes_instrument_id.as_str())?;
        let no_quote = self.books.snapshot(market.no_instrument_id.as_str())?;
        let snapshot = PairedMarketSnapshot {
            market_id: market.market_id.clone(),
            yes_instrument_id: market.yes_instrument_id.clone(),
            no_instrument_id: market.no_instrument_id.clone(),
            yes_quote,
            no_quote,
        };
        let inventory = self
            .inventories
            .get(&market.market_id)
            .map(InventoryState::snapshot)
            .unwrap_or_else(|| PairedInventorySnapshot {
                free_cash_usd: self.starting_cash_usd,
                equity_usd: self.starting_cash_usd,
                ..Default::default()
            });
        let mut open_convex_order_exposure =
            crate::strategies::traits::PairedOpenOrderExposure::default();
        for managed in self.managed_open_orders().values().filter(|managed| {
            managed.intent.market_id == market.market_id
                && managed.intent.side == TradeSide::Buy
                && !managed.intent.reduce_only
                && managed.remaining_qty() > 1e-9
                && managed
                    .intent
                    .quote_level_tag
                    .as_deref()
                    .and_then(crate::types::MmQuoteKind::from_quote_level_tag)
                    == Some(crate::types::MmQuoteKind::ConvexAccumulation)
        }) {
            let qty = managed.remaining_qty();
            let notional = managed.intent.limit_price.max(0.0) * qty;
            if managed.intent.instrument_id == market.yes_instrument_id {
                open_convex_order_exposure.yes_qty += qty;
                open_convex_order_exposure.yes_notional_usd += notional;
                open_convex_order_exposure.yes_count += 1;
            } else if managed.intent.instrument_id == market.no_instrument_id {
                open_convex_order_exposure.no_qty += qty;
                open_convex_order_exposure.no_notional_usd += notional;
                open_convex_order_exposure.no_count += 1;
            }
        }
        let pair_cost = PairCostTracker::from_inventory(&inventory);
        let btc_regime = self.btc_regime.snapshot();
        let fair_value = self.fair_value_for(market, &btc_regime, now_ms);
        let order_book_pressure =
            crate::signals::OrderBookPressureEngine::default().compute(&snapshot);
        Some(StrategyInput {
            market: market.clone(),
            snapshot,
            inventory,
            open_convex_order_exposure,
            pair_cost,
            fair_value,
            btc_regime,
            momentum: crate::signals::MomentumSignal::default(),
            order_book_pressure,
            now_ms,
        })
    }

    fn fair_value_for(
        &self,
        market: &BinaryOutcomeMarket,
        regime: &BtcRegimeSnapshot,
        now_ms: u64,
    ) -> FairValueEstimate {
        let no_signal = |reason: NoSignalReason| FairValueEstimate {
            p_up: 0.5,
            p_down: 0.5,
            log_moneyness: f64::NAN,
            sigma_remaining: f64::NAN,
            time_remaining_s: market.time_remaining_fraction(now_ms)
                * (market.window_ms() as f64 / 1_000.0),
            model: FairValueModel::NoSignal(reason),
        };
        let underlying_matches = matches!(market.underlying, UnderlyingAsset::Btc);
        if !underlying_matches {
            return no_signal(NoSignalReason::SpotInvalid);
        }
        let Some(spot) = regime.last_price else {
            return no_signal(NoSignalReason::SpotInvalid);
        };
        let Some(strike) = market.price_to_beat else {
            return no_signal(NoSignalReason::StrikeInvalid);
        };
        let Some(vol_bps) = regime.realized_vol_5m_bps else {
            return no_signal(NoSignalReason::VolInvalid);
        };
        let tau = market.time_remaining_fraction(now_ms);
        let momentum_weight = self.profile.momentum_weight().clamp(0.0, 5.0);
        let momentum_return = regime.return_60s_bps.unwrap_or(0.0) / 10_000.0 * momentum_weight;
        crate::signals::fair_value::estimate_fair_value_with_momentum(
            spot,
            strike,
            tau,
            vol_bps / 10_000.0,
            momentum_return,
        )
    }

    fn convert_intent(
        &mut self,
        intent: OrderIntent,
        market: &BinaryOutcomeMarket,
        now_ms: u64,
    ) -> Option<StrategyOrderIntent> {
        let leg = if intent.instrument_id == market.yes_instrument_id {
            Leg::Yes
        } else if intent.instrument_id == market.no_instrument_id {
            Leg::No
        } else {
            return None;
        };
        // Stable, deterministic id: `<original>:<sequence>`. The strategy
        // sometimes emits duplicate `client_order_id`s across ticks (resting
        // ladder is rebuilt). Suffix avoids collisions inside the simulator
        // without changing strategy behavior.
        self.sequence = self.sequence.wrapping_add(1);
        let coid = format!("{}:{}", intent.client_order_id, self.sequence);
        let side = match intent.side {
            TradeSide::Buy => Side::Buy,
            TradeSide::Sell => Side::Sell,
        };
        let asset_id = intent.instrument_id.as_str().to_string();
        let mut runtime_intent = intent.clone();
        runtime_intent.client_order_id = ClientOrderId::new(coid.clone());
        runtime_intent.created_at_ms = if runtime_intent.created_at_ms > 0 {
            runtime_intent.created_at_ms
        } else {
            now_ms
        };
        let runtime_inventory = self
            .runtime_inventories
            .entry(runtime_intent.market_id.clone())
            .or_insert_with(|| RuntimeInventoryState::new(self.starting_cash_usd));
        if runtime_inventory
            .reserve_for_order(&runtime_intent)
            .is_err()
        {
            return None;
        }
        if self
            .runtime_state
            .submit_order(runtime_intent.clone(), runtime_intent.created_at_ms)
            .is_err()
        {
            let _ = runtime_inventory.release_reservation(
                &runtime_intent.client_order_id,
                runtime_intent.created_at_ms,
            );
            return None;
        }
        self.open_intents.insert(
            coid.clone(),
            IntentRecord::from_intent(&runtime_intent, leg),
        );
        // Live/replay parity: `OrderIntent` does not carry a typed
        // post_only flag yet, so the adapter derives it from
        // `IntentKind`. Entry intents (paired-mm bids/asks, capital
        // recycle, convex accumulation) are resting maker quotes and the
        // live trader sets `post_only=true` on the venue request to keep
        // the maker rebate. Close intents (hedge rescue, reduce-only
        // sells) are FAK-style aggressive lifts and must NOT be
        // post-only. This mirrors the live runtime's path through the
        // execution adapter, where the same IntentKind drives the same
        // venue-side flags.
        let aggressive = matches!(intent.kind, IntentKind::Close);
        let post_only = matches!(intent.kind, IntentKind::Entry);
        Some(StrategyOrderIntent {
            client_order_id: coid,
            asset_id,
            side,
            price: intent.limit_price,
            size: intent.quantity,
            placed_ms: if intent.created_at_ms > 0 {
                intent.created_at_ms
            } else {
                now_ms
            },
            aggressive,
            post_only,
        })
    }

    fn handle_decision(
        &mut self,
        decision: StrategyDecision,
        market: &BinaryOutcomeMarket,
        now_ms: u64,
        out: &mut ReplayDecision,
    ) {
        let now_ns = (now_ms as i64).saturating_mul(1_000_000);
        let market_slug = market.market_id.as_str().to_string();
        let decision_label = strategy_decision_label(&decision);
        let reason_tag = strategy_decision_reason_tag(&decision);
        let inputs_hash = self.build_input_hash(market, now_ms);
        out.journal_events.push(JournalEvent::StrategyDecision {
            ts_ns: now_ns,
            market_slug,
            asset_id: None,
            decision_type: decision_label,
            raw_inputs_hash: inputs_hash,
            reason_tag,
        });
        match decision {
            StrategyDecision::QuoteSet { intents, .. } => {
                self.reconcile_and_emit_quote_set(intents, market, now_ms, out);
            }
            StrategyDecision::CapitalRecycle { intents, .. }
            | StrategyDecision::Rescue { intents, .. } => {
                for intent in intents {
                    if matches!(intent.kind, IntentKind::Close) && intent.reduce_only {
                        // Reduce-only sells: only emit if we actually have the
                        // inventory. Without this guard the simulator would
                        // post a phantom sell that fills against trade flow,
                        // double-counting the position.
                        let inv = self.inventories.get(&market.market_id);
                        let qty_avail =
                            match (inv, intent.instrument_id == market.yes_instrument_id) {
                                (Some(state), true) => state.yes_qty,
                                (Some(state), false)
                                    if intent.instrument_id == market.no_instrument_id =>
                                {
                                    state.no_qty
                                }
                                _ => 0.0,
                            };
                        if qty_avail < intent.quantity {
                            continue;
                        }
                    }
                    self.evaluate_and_emit(intent, market, now_ms, out);
                }
            }
            StrategyDecision::Merge { intent, .. } => {
                self.apply_merge_decision(intent, market, now_ms, out);
            }
            StrategyDecision::Suppress { .. } | StrategyDecision::Noop { .. } => {}
        }
    }

    fn apply_merge_decision(
        &mut self,
        intent: crate::types::MergeIntent,
        market: &BinaryOutcomeMarket,
        now_ms: u64,
        out: &mut ReplayDecision,
    ) {
        let qty = intent.quantity.max(0.0);
        if qty <= f64::EPSILON {
            return;
        }
        let fee_usd = intent.expected_fee_usd.max(0.0);
        let gas_usd = intent.expected_gas_usd.max(0.0);
        let gross_cash_usd = intent.expected_cash_usd.max(0.0);
        let cost_usd = intent.expected_cost_usd.max(0.0);
        let runtime_inventory = self
            .runtime_inventories
            .entry(intent.market_id.clone())
            .or_insert_with(|| RuntimeInventoryState::new(self.starting_cash_usd));
        if runtime_inventory
            .apply_merge(
                &intent.market_id,
                &intent.yes_instrument_id,
                &intent.no_instrument_id,
                qty,
                cost_usd,
                gross_cash_usd,
                fee_usd + gas_usd,
                now_ms,
            )
            .is_err()
        {
            return;
        }

        let inventory = self
            .inventories
            .entry(intent.market_id.clone())
            .or_insert_with(|| InventoryState::new(self.starting_cash_usd));
        if !inventory.apply_merge(qty, gross_cash_usd, fee_usd, gas_usd) {
            return;
        }

        let ts_ns = (now_ms as i64).saturating_mul(1_000_000);
        let market_slug = market.market_id.as_str().to_string();
        let net_credit = (gross_cash_usd - fee_usd - gas_usd).max(0.0);
        out.journal_events.push(JournalEvent::MergeEvent {
            ts_ns,
            market_slug: market_slug.clone(),
            qty_yes_burned: qty,
            qty_no_burned: qty,
            usd_credited: net_credit,
        });
        if fee_usd > 0.0 {
            out.journal_events.push(JournalEvent::AccountingEvent {
                ts_ns,
                kind: "fee".to_string(),
                market_slug: Some(market_slug.clone()),
                asset_id: None,
                usd_amount: fee_usd,
            });
        }
        if gas_usd > 0.0 {
            out.journal_events.push(JournalEvent::AccountingEvent {
                ts_ns,
                kind: "gas".to_string(),
                market_slug: Some(market_slug.clone()),
                asset_id: None,
                usd_amount: gas_usd,
            });
        }
        out.accounting_events.push(Event {
            v: 1,
            ts_ns,
            received_ns: ts_ns,
            event_type: EventType::UserOrder,
            market_type: "btc_5m".to_string(),
            market_slug: Some(market_slug),
            asset_id: None,
            side: None,
            price: None,
            size: Some(qty.to_string()),
            sequence: Some(ts_ns),
            source: Source::Synthesizer,
            raw: json!({
                "type": "merge",
                "status": "confirmed",
                "size": qty,
                "fee_usd": fee_usd,
                "gas_usd": gas_usd
            }),
        });
    }

    /// Stable, content-addressed hash of the `(market, snapshot, fair_value,
    /// btc_regime, inventory)` inputs handed to the strategy on this tick.
    /// Lets downstream audit consumers verify that two replays of the same
    /// stream saw byte-identical strategy inputs without persisting the
    /// full input payload.
    fn build_input_hash(&self, market: &BinaryOutcomeMarket, now_ms: u64) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        let yes_q = self
            .books
            .snapshot(market.yes_instrument_id.as_str())
            .map(|q| {
                (
                    q.best_bid.as_ref().map(|l| (l.price, l.quantity)),
                    q.best_ask.as_ref().map(|l| (l.price, l.quantity)),
                    q.last_trade_price,
                )
            });
        let no_q = self
            .books
            .snapshot(market.no_instrument_id.as_str())
            .map(|q| {
                (
                    q.best_bid.as_ref().map(|l| (l.price, l.quantity)),
                    q.best_ask.as_ref().map(|l| (l.price, l.quantity)),
                    q.last_trade_price,
                )
            });
        let inv = self
            .inventories
            .get(&market.market_id)
            .map(|s| (s.yes_qty, s.no_qty, s.free_cash_usd));
        let regime = self.btc_regime.snapshot();
        let payload = format!(
            "{}|{}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
            market.market_id.as_str(),
            now_ms,
            yes_q,
            no_q,
            inv,
            regime.last_price,
            regime.realized_vol_5m_bps,
            regime.return_180s_bps,
        );
        hasher.update(payload.as_bytes());
        let digest = hasher.finalize();
        let mut hex = String::with_capacity(16);
        for b in digest.iter().take(8) {
            hex.push_str(&format!("{:02x}", b));
        }
        hex
    }

    fn reconcile_and_emit_quote_set(
        &mut self,
        intents: Vec<OrderIntent>,
        market: &BinaryOutcomeMarket,
        now_ms: u64,
        out: &mut ReplayDecision,
    ) {
        let desired = DesiredQuoteSet {
            quotes: intents
                .into_iter()
                .enumerate()
                .map(|(level, intent)| DesiredQuote {
                    intent,
                    level,
                    is_cleanup: false,
                    suppress_if_stale: false,
                    expires_at_ms: None,
                })
                .collect(),
            stale_quote_max_age_ms: None,
            quote_expiry_ms: None,
        };
        // `quote_reconciler::plan` expects `&HashMap<...>`. The reconciler
        // sorts every output path explicitly (per-key vec on line 374 and
        // unmatched_ids on line 572 of `quote_reconciler.rs`, plus a final
        // `plan.actions.sort_by` near the end), so the input map's
        // iteration order does not leak. The conversion here is the seam
        // between the `BTreeMap`-only adapter world and the live live
        // reconciler signature.
        let open_orders: HashMap<ClientOrderId, ManagedOrder> =
            self.managed_open_orders().into_iter().collect();
        let plan = self.quote_reconciler.plan(desired, &open_orders, now_ms);

        let now_ns = (now_ms as i64).saturating_mul(1_000_000);
        for action in plan.actions {
            match action {
                QuoteAction::Keep(_) => {}
                QuoteAction::Cancel {
                    client_order_id,
                    reason,
                } => {
                    out.journal_events.push(JournalEvent::IntentCancel {
                        ts_ns: now_ns,
                        intent_id: client_order_id.as_str().to_string(),
                        reason: format!("{:?}", reason),
                    });
                    out.cancels.push(client_order_id.as_str().to_string());
                    self.cancel_runtime_order(&client_order_id, now_ms);
                    self.remove_strategy_slot_for_sim_coid(client_order_id.as_str());
                }
                QuoteAction::Replace {
                    existing_client_order_id,
                    replacement,
                    cancel_reason: _,
                } => {
                    let old_id = existing_client_order_id.as_str().to_string();
                    out.cancels.push(old_id.clone());
                    self.cancel_runtime_order(&existing_client_order_id, now_ms);
                    self.remove_strategy_slot_for_sim_coid(existing_client_order_id.as_str());
                    let pre_submit_len = out.submits.len();
                    self.evaluate_and_emit(replacement, market, now_ms, out);
                    if let Some(new_intent) = out.submits.get(pre_submit_len) {
                        out.journal_events.push(JournalEvent::IntentReplace {
                            ts_ns: now_ns,
                            old_intent_id: old_id,
                            new_intent_id: new_intent.client_order_id.clone(),
                        });
                    } else {
                        // Replacement was rejected by risk; record a cancel.
                        out.journal_events.push(JournalEvent::IntentCancel {
                            ts_ns: now_ns,
                            intent_id: old_id,
                            reason: "replace_rejected".to_string(),
                        });
                    }
                }
                QuoteAction::Submit(intent) => {
                    self.evaluate_and_emit(intent, market, now_ms, out);
                }
            }
        }
    }

    /// Returns open orders in `BTreeMap` order so every downstream
    /// iteration (floating-point sums, exposure accumulators) is fed
    /// orders in a stable, key-sorted sequence. The runtime state store
    /// internally keys by `ClientOrderId` in a `HashMap`; converting at
    /// this seam keeps the determinism invariant local to the replay
    /// adapter.
    fn managed_open_orders(&self) -> BTreeMap<ClientOrderId, ManagedOrder> {
        self.runtime_state.open_orders().into_iter().collect()
    }

    fn remove_strategy_slot_for_sim_coid(&mut self, sim_coid: &str) {
        self.coid_by_strategy_slot
            .retain(|_, coid| coid.as_str() != sim_coid);
    }

    fn cancel_runtime_order(&mut self, client_order_id: &ClientOrderId, now_ms: u64) {
        if let Some(record) = self.open_intents.get(client_order_id.as_str()) {
            if let Some(inventory) = self.runtime_inventories.get_mut(&record.market_id) {
                let _ = inventory.release_reservation(client_order_id, now_ms);
            }
        }
        let _ = self.runtime_state.cancel_order(client_order_id, now_ms);
        self.open_intents.remove(client_order_id.as_str());
    }

    /// Route a single intent through the live `RiskEngine`. Accepted
    /// intents are converted into `StrategyOrderIntent`s and pushed onto
    /// `out.submits`; rejected intents append a typed `RiskRejection`
    /// onto `out.risk_rejections` and DO NOT submit.
    fn evaluate_and_emit(
        &mut self,
        intent: OrderIntent,
        market: &BinaryOutcomeMarket,
        now_ms: u64,
        out: &mut ReplayDecision,
    ) {
        if !self.risk_evaluation_enabled {
            // Bypass: convert and submit without risk evaluation. Mirrors
            // the prior Phase 3a behaviour. Scenarios that need the risk
            // strand opt in via `with_risk_evaluation(true)`.
            let strategy_slot = intent.client_order_id.as_str().to_string();
            if let Some(prior) = self.coid_by_strategy_slot.get(&strategy_slot).cloned() {
                if self
                    .open_intents
                    .get(&prior)
                    .is_some_and(|record| record_matches_intent(record, &intent))
                {
                    return;
                }
                out.cancels.push(prior.clone());
                self.cancel_runtime_order(&ClientOrderId::new(prior), now_ms);
            }
            let market_slug = market.market_id.as_str().to_string();
            let reason_tag = reason_tag_for_intent(&intent);
            let ladder_position = intent
                .quote_level_tag
                .as_deref()
                .and_then(ladder_position_for_tag);
            let side_str = match intent.side {
                TradeSide::Buy => "buy",
                TradeSide::Sell => "sell",
            }
            .to_string();
            let limit_price = intent.limit_price;
            let quantity = intent.quantity;
            let intent_kind = intent.kind;
            if let Some(sim_intent) = self.convert_intent(intent, market, now_ms) {
                self.coid_by_strategy_slot
                    .insert(strategy_slot, sim_intent.client_order_id.clone());
                out.journal_events.push(JournalEvent::IntentSubmit {
                    ts_ns: (now_ms as i64).saturating_mul(1_000_000),
                    market_slug,
                    asset_id: sim_intent.asset_id.clone(),
                    intent_id: sim_intent.client_order_id.clone(),
                    side: side_str,
                    price: limit_price,
                    size: quantity,
                    post_only: matches!(intent_kind, IntentKind::Entry),
                    ladder_position,
                    reason_tag,
                });
                out.submits.push(sim_intent);
            }
            return;
        }
        // Replay open-order counters come from the shared runtime state
        // store. This is the same state shape consumed by the live quote
        // reconciler, not a reconstructed replay-only view. Routed through
        // `managed_open_orders` so the iteration order below feeding the
        // floating-point sums is `BTreeMap`-stable rather than HashMap-
        // randomised; otherwise rehash order can flip risk decisions for
        // bit-identical input.
        let managed_open_orders = self.managed_open_orders();
        let live_total = managed_open_orders.len();
        let live_for_market = managed_open_orders
            .values()
            .filter(|managed| managed.intent.market_id == market.market_id)
            .count();
        let open_buy_notional_total_usd = managed_open_orders
            .values()
            .filter(|managed| matches!(managed.intent.side, TradeSide::Buy))
            .map(|managed| managed.intent.limit_price * managed.remaining_qty())
            .sum();
        let open_signed_notional_for_market_usd = managed_open_orders
            .values()
            .filter(|managed| managed.intent.market_id == market.market_id)
            .map(|managed| {
                managed.intent.limit_price * managed.remaining_qty() * managed.intent.side.sign()
            })
            .sum();
        let open_position_qty_for_instrument = managed_open_orders
            .values()
            .filter(|managed| managed.intent.instrument_id == intent.instrument_id)
            .map(|managed| managed.remaining_qty() * managed.intent.side.sign())
            .sum();
        let ctx = RiskContext {
            open_orders_total: live_total,
            open_orders_for_market: live_for_market,
            open_buy_notional_total_usd,
            open_signed_notional_for_market_usd,
            open_position_qty_for_instrument,
            starting_cash_usd: self.starting_cash_usd,
            now_ms,
        };
        let runtime_inv = self
            .runtime_inventories
            .entry(market.market_id.clone())
            .or_insert_with(|| RuntimeInventoryState::new(self.starting_cash_usd));
        let decision = self.risk.evaluate(runtime_inv, &intent, &ctx);
        if !decision.accepted {
            let reason = decision
                .reject_reason
                .expect("rejected decisions carry a typed reason");
            let caps = serde_json::json!({
                "max_order_notional_usd": self.risk.limits().max_order_notional_usd,
                "max_gross_notional_usd": self.risk.limits().max_gross_notional_usd,
                "max_net_notional_per_market_usd": self.risk.limits().max_net_notional_per_market_usd,
                "max_open_orders_total": self.risk.limits().max_open_orders_total,
                "max_open_orders_per_market": self.risk.limits().max_open_orders_per_market,
                "projected_free_cash_usd": decision.projected_free_cash_usd,
                "projected_gross_notional_usd": decision.projected_gross_notional_usd,
                "projected_market_net_notional_usd": decision.projected_market_net_notional_usd,
            });
            let side_str = match intent.side {
                TradeSide::Buy => "buy",
                TradeSide::Sell => "sell",
            };
            let intent_kind_str = match intent.kind {
                IntentKind::Entry => "place",
                IntentKind::Close => "place_close",
            };
            out.risk_rejections.push(RiskRejection::new(
                (now_ms as i64).saturating_mul(1_000_000),
                intent.client_order_id.as_str().to_string(),
                market.market_id.as_str().to_string(),
                intent.instrument_id.as_str().to_string(),
                intent_kind_str,
                side_str,
                intent.limit_price,
                intent.quantity,
                reason,
                decision.message.clone(),
                caps,
            ));
            return;
        }
        let strategy_slot = intent.client_order_id.as_str().to_string();
        // If the strategy is re-submitting the same logical slot, the live
        // runtime would keep an unchanged working order, or cancel/replace a
        // materially changed order. Preserve that invariant here; otherwise
        // replay turns every tick into a synthetic cancel/submit even when
        // the live quote reconciler would have emitted Keep.
        if let Some(prior) = self.coid_by_strategy_slot.get(&strategy_slot).cloned() {
            if self
                .open_intents
                .get(&prior)
                .is_some_and(|record| record_matches_intent(record, &intent))
            {
                return;
            }
            out.cancels.push(prior.clone());
            self.cancel_runtime_order(&ClientOrderId::new(prior), now_ms);
        }
        let market_slug = market.market_id.as_str().to_string();
        let reason_tag = reason_tag_for_intent(&intent);
        let ladder_position = intent
            .quote_level_tag
            .as_deref()
            .and_then(ladder_position_for_tag);
        let side_str = match intent.side {
            TradeSide::Buy => "buy",
            TradeSide::Sell => "sell",
        }
        .to_string();
        let limit_price = intent.limit_price;
        let quantity = intent.quantity;
        let intent_kind = intent.kind;
        if let Some(sim_intent) = self.convert_intent(intent, market, now_ms) {
            self.coid_by_strategy_slot
                .insert(strategy_slot, sim_intent.client_order_id.clone());
            self.open_orders_total = self.open_orders_total.saturating_add(1);
            *self
                .open_orders_per_market
                .entry(market.market_id.clone())
                .or_insert(0) += 1;
            out.journal_events.push(JournalEvent::IntentSubmit {
                ts_ns: (now_ms as i64).saturating_mul(1_000_000),
                market_slug,
                asset_id: sim_intent.asset_id.clone(),
                intent_id: sim_intent.client_order_id.clone(),
                side: side_str,
                price: limit_price,
                size: quantity,
                post_only: matches!(intent_kind, IntentKind::Entry),
                ladder_position,
                reason_tag,
            });
            out.submits.push(sim_intent);
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum EnabledStrategy {
    PairedMm,
    PairCostArb,
}

impl ReplayStrategy for ReplayStrategyAdapter {
    fn on_event(&mut self, event: &Event) -> ReplayDecision {
        let now_ms = (event.received_ns / 1_000_000) as u64;
        match event.event_type {
            EventType::MarketMeta => {
                self.handle_market_meta(event);
                return ReplayDecision::default();
            }
            EventType::PriceToBeat => {
                self.handle_price_to_beat(event);
                return ReplayDecision::default();
            }
            EventType::BookSnapshot | EventType::BookDelta | EventType::Trade => {
                self.books.apply(event);
            }
            EventType::BtcTick => {
                self.btc_regime.apply(event);
            }
            _ => return ReplayDecision::default(),
        }

        if self.markets.is_empty() {
            return ReplayDecision::default();
        }

        let mut decision = ReplayDecision::default();
        let markets = self.all_markets();
        for market in markets {
            // Ensure inventory state exists so the strategy sees a coherent
            // free-cash baseline before any fills.
            self.inventories
                .entry(market.market_id.clone())
                .or_insert_with(|| InventoryState::new(self.starting_cash_usd));
            let Some(input) = self.build_input_for_market(&market, now_ms) else {
                continue;
            };
            let registry_decisions = self.registry.on_tick(market.market_id.as_str(), input);
            for d in registry_decisions {
                self.handle_decision(d, &market, now_ms, &mut decision);
            }
        }
        decision
    }

    fn on_fill(&mut self, fill: &SimulatedFill) -> ReplayDecision {
        let Some(record) = self.open_intents.get(&fill.client_order_id).cloned() else {
            return ReplayDecision::default();
        };
        let now_ms = fill.fill_ms;
        let Some(market) = self.markets.get(&record.market_id).cloned() else {
            return ReplayDecision::default();
        };
        let Some(input) = self.build_input_for_market(&market, now_ms) else {
            return ReplayDecision::default();
        };
        let liquidity = match fill.maker_or_taker {
            crate::replay::fill_sim::MakerOrTaker::Maker => FillLiquidity::Maker,
            crate::replay::fill_sim::MakerOrTaker::Taker => FillLiquidity::Taker,
        };
        let fill_report = FillReport {
            order_id: None,
            client_order_id: Some(ClientOrderId::new(fill.client_order_id.clone())),
            market_id: record.market_id.clone(),
            instrument_id: record.instrument_id.clone(),
            side: record.side,
            price: fill.price,
            quantity: fill.size,
            fee_usd: 0.0,
            liquidity,
            close_method: None,
            observed_at_ms: now_ms,
        };
        // The fill simulator produces candidate fills. Only accept them if
        // the live-style runtime inventory can apply the fill against its
        // reservation/cash state. This prevents replay PnL/accounting from
        // counting fills the live runtime could not settle.
        if let Some(runtime_inv) = self.runtime_inventories.get_mut(&record.market_id) {
            if let Err(err) = runtime_inv.apply_fill(&fill_report) {
                tracing::warn!(
                    target: "replay.strategy_adapter",
                    client_order_id = %fill.client_order_id,
                    market = %record.market_id,
                    error = %err,
                    "replay rejected simulator fill against runtime inventory"
                );
                return ReplayDecision {
                    rejected_fills: vec![fill.client_order_id.clone()],
                    ..ReplayDecision::default()
                };
            }
        }

        let inventory = self
            .inventories
            .entry(record.market_id.clone())
            .or_insert_with(|| InventoryState::new(self.starting_cash_usd));
        inventory.apply_fill(record.leg, record.side, fill.price, fill.size);
        let post_fill_yes_qty = inventory.yes_qty;
        let post_fill_yes_avg = inventory.yes_avg_cost;
        let post_fill_no_qty = inventory.no_qty;
        let post_fill_no_avg = inventory.no_avg_cost;

        let fill_client_order_id = ClientOrderId::new(fill.client_order_id.clone());
        let _ = self
            .runtime_state
            .apply_fill(&fill_client_order_id, fill.size, now_ms);
        let fill_is_terminal = self
            .runtime_state
            .get_order(&fill_client_order_id)
            .is_none_or(|managed| managed.remaining_qty() <= 1e-9 || managed.status.is_terminal());
        if fill_is_terminal {
            self.open_orders_total = self.open_orders_total.saturating_sub(1);
            if let Some(c) = self.open_orders_per_market.get_mut(&record.market_id) {
                *c = c.saturating_sub(1);
            }
            self.open_intents.remove(&fill.client_order_id);
            self.coid_by_strategy_slot
                .retain(|_, coid| coid != &fill.client_order_id);
        }

        // Build StrategyFillInput and notify the strategy.
        let fill_input = StrategyFillInput {
            market: market.clone(),
            snapshot: input.snapshot,
            fair_value: input.fair_value,
            fill: fill_report,
        };
        let mut decision = ReplayDecision::default();
        let now_ns = (now_ms as i64).saturating_mul(1_000_000);
        let market_slug = market.market_id.as_str().to_string();
        decision
            .journal_events
            .push(JournalEvent::InventorySnapshot {
                ts_ns: now_ns,
                market_slug: market_slug.clone(),
                asset_id: market.yes_instrument_id.as_str().to_string(),
                qty: post_fill_yes_qty,
                avg_cost: post_fill_yes_avg,
            });
        decision
            .journal_events
            .push(JournalEvent::InventorySnapshot {
                ts_ns: now_ns,
                market_slug,
                asset_id: market.no_instrument_id.as_str().to_string(),
                qty: post_fill_no_qty,
                avg_cost: post_fill_no_avg,
            });
        let decisions = self.registry.on_fill(market.market_id.as_str(), fill_input);
        for d in decisions {
            self.handle_decision(d, &market, now_ms, &mut decision);
        }
        decision
    }

    fn on_ioc_expired(&mut self, client_order_id: &str, now_ms: u64) -> ReplayDecision {
        let client_order_id = ClientOrderId::new(client_order_id.to_string());
        self.cancel_runtime_order(&client_order_id, now_ms);
        ReplayDecision::default()
    }
}

/// Map a `StrategyDecision` variant to its journal `decision_type` label.
fn strategy_decision_label(decision: &StrategyDecision) -> String {
    match decision {
        StrategyDecision::QuoteSet { .. } => "quote_set".to_string(),
        StrategyDecision::CapitalRecycle { .. } => "capital_recycle".to_string(),
        StrategyDecision::Rescue { .. } => "rescue".to_string(),
        StrategyDecision::Merge { .. } => "merge".to_string(),
        StrategyDecision::Suppress { reason, .. } => format!("suppress:{:?}", reason),
        StrategyDecision::Noop { .. } => "noop".to_string(),
    }
}

/// Pull the most descriptive reason tag the live strategy emitted out of
/// the decision's notes. The strategy already encodes mode/decision_label
/// strings on its notes (see paired_mm.rs and engine.rs); we surface the
/// first decision_label hit so downstream audit can pivot on the same
/// label that live operators see in logs.
fn strategy_decision_reason_tag(decision: &StrategyDecision) -> String {
    let notes: &[String] = match decision {
        StrategyDecision::QuoteSet { notes, .. }
        | StrategyDecision::CapitalRecycle { notes, .. }
        | StrategyDecision::Rescue { notes, .. }
        | StrategyDecision::Merge { notes, .. }
        | StrategyDecision::Suppress { notes, .. }
        | StrategyDecision::Noop { notes } => notes,
    };
    for note in notes {
        if let Some(label) = extract_decision_label(note) {
            return label;
        }
    }
    // Fallback: first note is the strategy's headline diagnostic.
    notes
        .first()
        .cloned()
        .unwrap_or_else(|| "unlabeled".to_string())
}

fn extract_decision_label(note: &str) -> Option<String> {
    let key = "decision_label=";
    let idx = note.find(key)?;
    let rest = &note[idx + key.len()..];
    let end = rest
        .find(|c: char| c == ' ' || c == ',')
        .unwrap_or(rest.len());
    let label = rest[..end].trim();
    if label.is_empty() {
        None
    } else {
        Some(label.to_string())
    }
}

/// Resolve the `reason_tag` field for an `intent_submit` journal row from
/// the originating `OrderIntent`. The strategy already populates either
/// `quote_level_tag` (e.g. `mm-paired-bid:yes:l1:PairedEntry`) or `reason`
/// (e.g. `paired-mm ladder yes level 1`); we prefer the structured tag.
fn reason_tag_for_intent(intent: &OrderIntent) -> String {
    intent
        .quote_level_tag
        .clone()
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| intent.reason.clone())
}

fn ladder_position_for_tag(tag: &str) -> Option<i32> {
    // Existing tags carry `:l<N>:` (e.g. `mm-paired-bid:yes:l1:PairedEntry`).
    let lower = tag.to_ascii_lowercase();
    let mut iter = lower.split(':');
    while let Some(seg) = iter.next() {
        if let Some(stripped) = seg.strip_prefix('l') {
            if let Ok(n) = stripped.parse::<i32>() {
                return Some(n);
            }
        }
    }
    None
}

fn record_matches_intent(record: &IntentRecord, intent: &OrderIntent) -> bool {
    record.instrument_id == intent.instrument_id
        && record.side == intent.side
        && record.reduce_only == intent.reduce_only
        && record.quote_level_tag == intent.quote_level_tag
        && record.pair_id == intent.pair_id
        && record.kind == intent.kind
        && (record.limit_price - intent.limit_price).abs() <= 1e-9
        && (record.quantity - intent.quantity).abs() <= 1e-9
}

fn parse_f64(s: Option<&str>) -> Option<f64> {
    s.and_then(|raw| raw.parse::<f64>().ok())
        .filter(|v| v.is_finite())
}

fn side_of(s: Option<&str>) -> Option<TradeSide> {
    match s.map(|v| v.to_ascii_lowercase())?.as_str() {
        "buy" | "bid" => Some(TradeSide::Buy),
        "sell" | "ask" => Some(TradeSide::Sell),
        _ => None,
    }
}

fn price_to_ticks(price: f64) -> i64 {
    (price * 1_000_000.0).round() as i64
}

fn ticks_to_price(ticks: i64) -> f64 {
    ticks as f64 / 1_000_000.0
}

/// Hint helper kept for consumers reading the YAML directly.
pub fn parse_profile_yaml(yaml: &str) -> anyhow::Result<StrategyProfile> {
    let value: YamlValue = serde_yaml::from_str(yaml)?;
    let json_value: serde_json::Value = serde_json::to_value(value)?;
    Ok(serde_json::from_value(json_value)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collector::schema::Source;
    use crate::replay::fill_sim::{FillSimConfig, LatencyPreset};
    use crate::replay::runner::{run_window, RunnerConfig, WindowStatus};
    use serde_json::json;

    fn evt(received_ns: i64, event_type: EventType) -> Event {
        Event {
            v: 1,
            ts_ns: received_ns - 1,
            received_ns,
            event_type,
            market_type: "btc_5m".into(),
            market_slug: Some("btc-up".into()),
            asset_id: None,
            side: None,
            price: None,
            size: None,
            sequence: Some(received_ns),
            source: Source::PolymarketMarketWs,
            raw: json!({}),
        }
    }

    fn market_meta_event(received_ns: i64, slug: &str, yes: &str, no: &str) -> Event {
        Event {
            v: 1,
            ts_ns: received_ns - 1,
            received_ns,
            event_type: EventType::MarketMeta,
            market_type: "btc_5m".into(),
            market_slug: Some(slug.into()),
            asset_id: Some(yes.into()),
            side: None,
            price: None,
            size: None,
            sequence: None,
            source: Source::PolymarketDataApi,
            raw: json!({
                "slug": slug,
                "market_type": "btc_5m",
                "asset_ids": [yes, no],
                "strike": 60_000.0,
                "end_time_ms": (received_ns / 1_000_000) + 300_000,
            }),
        }
    }

    fn replay_intent(client_order_id: &str, price: f64) -> OrderIntent {
        OrderIntent {
            client_order_id: ClientOrderId::from(client_order_id),
            market_id: MarketId::from("market-1"),
            instrument_id: InstrumentId::from("yes-1"),
            side: TradeSide::Buy,
            limit_price: price,
            quantity: 10.0,
            reduce_only: false,
            reason: "test".to_string(),
            quote_level_tag: Some("lvl-1".to_string()),
            created_at_ms: 1_000,
            pair_id: None,
            kind: IntentKind::Entry,
        }
    }

    #[test]
    fn replay_quote_set_uses_live_reconciler_keep_for_unchanged_quote() {
        let mut adapter = ReplayStrategyAdapter::from_profile(StrategyProfile::default());
        let intent = replay_intent("slot-1", 0.42);
        adapter.open_intents.insert(
            "slot-1:1".to_string(),
            IntentRecord {
                market_id: intent.market_id.clone(),
                instrument_id: intent.instrument_id.clone(),
                side: intent.side,
                limit_price: intent.limit_price,
                quantity: intent.quantity,
                reduce_only: intent.reduce_only,
                quote_level_tag: intent.quote_level_tag.clone(),
                pair_id: intent.pair_id.clone(),
                kind: intent.kind,
                leg: Leg::Yes,
            },
        );
        adapter
            .coid_by_strategy_slot
            .insert("slot-1".to_string(), "slot-1:1".to_string());
        let market = BinaryOutcomeMarket::btc_5m(
            MarketId::from("market-1"),
            InstrumentId::from("yes-1"),
            InstrumentId::from("no-1"),
        );
        let mut decision = ReplayDecision::default();

        adapter.reconcile_and_emit_quote_set(vec![intent], &market, 2_000, &mut decision);

        assert!(decision.submits.is_empty());
        assert!(decision.cancels.is_empty());
        assert_eq!(adapter.open_intents.len(), 1);
    }

    #[test]
    fn registry_populated_from_market_meta_events() {
        let profile = StrategyProfile::default();
        let mut adapter = ReplayStrategyAdapter::from_profile(profile);
        let stream = vec![
            market_meta_event(1_000_000, "btc-up-a", "yes-a", "no-a"),
            market_meta_event(2_000_000, "btc-up-b", "yes-b", "no-b"),
        ];
        for event in &stream {
            adapter.on_event(event);
        }
        assert_eq!(adapter.market_count(), 2);
        assert_eq!(adapter.strategy_count(), 2);
    }

    #[test]
    fn replay_rejects_simulator_fill_that_exceeds_runtime_cash() {
        let mut profile = StrategyProfile::default();
        profile.inventory.max_net_notional_per_market_usd = Some(2_000.0);
        profile.inventory.max_gross_notional_usd = Some(2_000.0);
        let mut adapter = ReplayStrategyAdapter::from_profile(profile);
        let market = BinaryOutcomeMarket::btc_5m(
            MarketId::from("market-1"),
            InstrumentId::from("yes-1"),
            InstrumentId::from("no-1"),
        );
        adapter.handle_market_meta(&market_meta_event(1_000_000, "market-1", "yes-1", "no-1"));
        for (asset_id, side, price, size, ts) in [
            ("yes-1", "buy", "0.49", "100", 1_100_000),
            ("yes-1", "sell", "0.51", "100", 1_200_000),
            ("no-1", "buy", "0.49", "100", 1_300_000),
            ("no-1", "sell", "0.51", "100", 1_400_000),
        ] {
            let mut event = evt(ts, EventType::BookSnapshot);
            event.asset_id = Some(asset_id.to_string());
            event.side = Some(side.to_string());
            event.price = Some(price.to_string());
            event.size = Some(size.to_string());
            adapter.books.apply(&event);
        }

        let mut intent = replay_intent("slot-oversize", 0.50);
        intent.quantity = 500.0;
        let mut submit_decision = ReplayDecision::default();
        adapter.evaluate_and_emit(intent, &market, 2_000, &mut submit_decision);
        let submitted = submit_decision
            .submits
            .first()
            .expect("order accepted and reserved");

        let fill = SimulatedFill {
            client_order_id: submitted.client_order_id.clone(),
            asset_id: submitted.asset_id.clone(),
            side: Side::Buy,
            price: 0.50,
            size: 3_000.0,
            fill_ms: 3_000,
            maker_or_taker: crate::replay::fill_sim::MakerOrTaker::Maker,
        };
        let fill_decision = adapter.on_fill(&fill);

        assert_eq!(
            fill_decision.rejected_fills,
            vec![submitted.client_order_id.clone()]
        );
        let paired_inventory = adapter.inventories.get(&market.market_id);
        assert_eq!(paired_inventory.map(|inv| inv.yes_qty).unwrap_or(0.0), 0.0);
        assert_eq!(paired_inventory.map(|inv| inv.no_qty).unwrap_or(0.0), 0.0);
        let runtime_inventory = adapter
            .runtime_inventories
            .get(&market.market_id)
            .expect("runtime inventory exists from reservation");
        assert!((runtime_inventory.free_cash_usd() - 750.0).abs() < 1e-9);
        assert!((runtime_inventory.reserved_cash_usd() - 250.0).abs() < 1e-9);
    }

    #[test]
    fn replay_ioc_close_expiry_releases_unfilled_reservation() {
        let mut profile = StrategyProfile::default();
        profile.inventory.min_free_cash_usd = Some(0.0);
        profile.inventory.min_free_cash_bps = Some(0.0);
        profile.inventory.max_net_notional_per_market_usd = Some(2_000.0);
        profile.inventory.max_gross_notional_usd = Some(2_000.0);
        let mut adapter = ReplayStrategyAdapter::from_profile(profile);
        let market = BinaryOutcomeMarket::btc_5m(
            MarketId::from("market-1"),
            InstrumentId::from("yes-1"),
            InstrumentId::from("no-1"),
        );
        adapter.handle_market_meta(&market_meta_event(1_000_000, "market-1", "yes-1", "no-1"));

        let mut intent = replay_intent("close-buy", 0.50);
        intent.kind = IntentKind::Close;
        intent.quote_level_tag = Some("mm-capital-recycle:yes:CapitalRecycle".to_string());
        let mut submit_decision = ReplayDecision::default();
        adapter.evaluate_and_emit(intent, &market, 2_000, &mut submit_decision);
        let submitted = submit_decision
            .submits
            .first()
            .expect("close order accepted and reserved");

        let runtime_inventory = adapter
            .runtime_inventories
            .get(&market.market_id)
            .expect("runtime inventory exists from reservation");
        assert!((runtime_inventory.free_cash_usd() - 995.0).abs() < 1e-9);
        assert!((runtime_inventory.reserved_cash_usd() - 5.0).abs() < 1e-9);

        adapter.on_ioc_expired(&submitted.client_order_id, 2_000);

        let runtime_inventory = adapter
            .runtime_inventories
            .get(&market.market_id)
            .expect("runtime inventory remains");
        assert!((runtime_inventory.free_cash_usd() - 1_000.0).abs() < 1e-9);
        assert!(runtime_inventory.reserved_cash_usd().abs() < 1e-9);
        assert!(adapter.open_intents.is_empty());
        assert_eq!(adapter.managed_open_orders().len(), 0);
    }

    #[test]
    fn inventory_snapshot_equity_uses_free_cash_plus_cost_basis() {
        let mut inventory = InventoryState::new(1_000.0);

        inventory.apply_fill(Leg::Yes, TradeSide::Buy, 0.40, 10.0);
        inventory.apply_fill(Leg::No, TradeSide::Buy, 0.55, 10.0);
        let snapshot = inventory.snapshot();

        assert!((snapshot.free_cash_usd - 990.5).abs() < 1e-9);
        assert!((snapshot.equity_usd - 1_000.0).abs() < 1e-9);
    }

    #[test]
    fn inventory_merge_can_reduce_cash_when_fees_exceed_credit() {
        let mut inventory = InventoryState::new(1_000.0);
        inventory.apply_fill(Leg::Yes, TradeSide::Buy, 0.40, 1.0);
        inventory.apply_fill(Leg::No, TradeSide::Buy, 0.55, 1.0);

        assert!(inventory.apply_merge(1.0, 1.0, 0.75, 0.50));

        assert_eq!(inventory.yes_qty, 0.0);
        assert_eq!(inventory.no_qty, 0.0);
        assert!((inventory.free_cash_usd - 998.80).abs() < 1e-9);
    }

    #[test]
    fn book_aggregator_rebuilds_quote_snapshot_from_deltas() {
        let mut agg = BookAggregator::default();
        for (received_ns, side, price, size) in [
            (1_000_000_000, "buy", "0.40", "10"),
            (1_500_000_000, "sell", "0.60", "8"),
            (2_000_000_000, "buy", "0.42", "5"),
        ] {
            let mut e = evt(received_ns, EventType::BookSnapshot);
            e.asset_id = Some("asset-x".into());
            e.side = Some(side.into());
            e.price = Some(price.into());
            e.size = Some(size.into());
            agg.apply(&e);
        }
        let snap = agg.snapshot("asset-x").expect("snapshot present");
        assert_eq!(snap.bid_levels.len(), 2);
        assert!((snap.bid_levels[0].price - 0.42).abs() < 1e-9);
        assert!((snap.ask_levels[0].price - 0.60).abs() < 1e-9);
    }

    #[test]
    fn adapter_runs_through_runner_without_panic_when_no_strategy_active() {
        // No market_meta events → no strategy registered → fills empty
        // but the adapter must still run cleanly through the event loop.
        let profile = StrategyProfile::default();
        let mut adapter = ReplayStrategyAdapter::from_profile(profile);
        let mut events = Vec::new();
        for i in 0..5 {
            let mut e = evt(i * 1_000_000_000, EventType::Trade);
            e.asset_id = Some("asset-y".into());
            e.side = Some("buy".into());
            e.price = Some("0.5".into());
            e.size = Some("1".into());
            events.push(e);
        }
        let cfg = RunnerConfig {
            window_id: "w1".into(),
            fill_sim: FillSimConfig {
                latency: LatencyPreset::Instant,
                ..Default::default()
            },
            max_window_failures: 0,
            starting_cash_usd: 1_000.0,
            maker_rebate_bps: 0.0,
        };
        let summary = run_window(&mut adapter, &events, &cfg);
        assert_eq!(summary.status, WindowStatus::Ok);
        assert_eq!(summary.fills.len(), 0);
    }
}
