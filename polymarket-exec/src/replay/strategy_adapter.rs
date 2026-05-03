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

use std::collections::BTreeMap;

use serde_yaml::Value as YamlValue;

use crate::collector::schema::{Event, EventType};
use crate::core::types::{
    BookLevel, ClientOrderId, FillLiquidity, FillReport, InstrumentId, IntentKind, MarketId,
    OrderIntent, QuoteSnapshot, StrategyDecision, TradeSide,
};
use crate::inventory::InventoryState as RuntimeInventoryState;
use crate::market_making::pairing::pair_cost_tracker::PairCostTracker;
use crate::market_making::pairing::types::{PairedInventorySnapshot, PairedMarketSnapshot};
use crate::markets::{BinaryOutcomeMarket, MarketDescriptor, MarketRegistry, UnderlyingAsset};
use crate::replay::fill_sim::{Side, SimulatedFill, StrategyOrderIntent};
use crate::replay::risk_trace::RiskRejection;
use crate::replay::runner::{ReplayDecision, ReplayStrategy};
use crate::risk::{RiskContext, RiskEngine};
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
#[derive(Clone, Debug, Default)]
struct BtcRegimeAggregator {
    samples: Vec<(u64, f64)>,
    observed_at_ms: u64,
}

impl BtcRegimeAggregator {
    const WINDOW_MS_5M: u64 = 5 * 60 * 1_000;
    const WINDOW_MS_15M: u64 = 15 * 60 * 1_000;
    const WINDOW_MS_30S: u64 = 30 * 1_000;
    const WINDOW_MS_60S: u64 = 60 * 1_000;
    const WINDOW_MS_120S: u64 = 120 * 1_000;
    const WINDOW_MS_180S: u64 = 180 * 1_000;
    const MAX_SAMPLES: usize = 4_000;

    fn apply(&mut self, event: &Event) {
        if event.event_type != EventType::BtcTick {
            return;
        }
        let Some(price) = parse_f64(event.price.as_deref()) else {
            return;
        };
        let now_ms = (event.received_ns / 1_000_000) as u64;
        self.samples.push((now_ms, price));
        self.observed_at_ms = now_ms;
        let cutoff = now_ms.saturating_sub(Self::WINDOW_MS_15M + 60_000);
        self.samples.retain(|(t, _)| *t >= cutoff);
        if self.samples.len() > Self::MAX_SAMPLES {
            let drop = self.samples.len() - Self::MAX_SAMPLES;
            self.samples.drain(0..drop);
        }
    }

    fn last_price(&self) -> Option<f64> {
        self.samples.last().map(|(_, p)| *p)
    }

    fn realized_vol_bps(&self, window_ms: u64) -> Option<f64> {
        let now = self.observed_at_ms;
        let cutoff = now.saturating_sub(window_ms);
        let prices: Vec<f64> = self
            .samples
            .iter()
            .filter(|(t, _)| *t >= cutoff)
            .map(|(_, p)| *p)
            .collect();
        if prices.len() < 2 {
            return None;
        }
        let mut returns = Vec::with_capacity(prices.len() - 1);
        for w in prices.windows(2) {
            let prev = w[0];
            let cur = w[1];
            if prev > 0.0 && cur > 0.0 {
                returns.push((cur / prev).ln());
            }
        }
        if returns.is_empty() {
            return None;
        }
        let mean = returns.iter().copied().sum::<f64>() / returns.len() as f64;
        let var = returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / returns.len() as f64;
        Some(var.sqrt() * 10_000.0)
    }

    fn return_bps(&self, window_ms: u64) -> Option<f64> {
        let now = self.observed_at_ms;
        let cutoff = now.saturating_sub(window_ms);
        let last = self.samples.last()?.1;
        let earliest = self
            .samples
            .iter()
            .find(|(t, _)| *t >= cutoff)
            .map(|(_, p)| *p)?;
        if earliest <= 0.0 {
            return None;
        }
        Some(((last / earliest).ln()) * 10_000.0)
    }

    fn trade_count(&self, window_ms: u64) -> u64 {
        let now = self.observed_at_ms;
        let cutoff = now.saturating_sub(window_ms);
        self.samples.iter().filter(|(t, _)| *t >= cutoff).count() as u64
    }

    fn snapshot(&self) -> BtcRegimeSnapshot {
        BtcRegimeSnapshot {
            last_price: self.last_price(),
            realized_vol_5m_bps: self.realized_vol_bps(Self::WINDOW_MS_5M),
            realized_vol_15m_bps: self.realized_vol_bps(Self::WINDOW_MS_15M),
            trade_count_5m: self.trade_count(Self::WINDOW_MS_5M),
            trade_count_15m: self.trade_count(Self::WINDOW_MS_15M),
            return_30s_bps: self.return_bps(Self::WINDOW_MS_30S),
            return_60s_bps: self.return_bps(Self::WINDOW_MS_60S),
            return_120s_bps: self.return_bps(Self::WINDOW_MS_120S),
            return_180s_bps: self.return_bps(Self::WINDOW_MS_180S),
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
    starting_cash_usd: f64,
}

impl InventoryState {
    fn new(starting_cash_usd: f64) -> Self {
        Self {
            yes_qty: 0.0,
            yes_avg_cost: 0.0,
            no_qty: 0.0,
            no_avg_cost: 0.0,
            free_cash_usd: starting_cash_usd,
            starting_cash_usd,
        }
    }

    fn snapshot(&self) -> PairedInventorySnapshot {
        PairedInventorySnapshot {
            yes_qty: self.yes_qty,
            no_qty: self.no_qty,
            yes_avg_cost: self.yes_avg_cost,
            no_avg_cost: self.no_avg_cost,
            free_cash_usd: self.free_cash_usd,
            equity_usd: self.starting_cash_usd
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
    /// Whether risk-engine evaluation runs in-band on every intent. The
    /// live trader does cancel/replace through a venue adapter; the replay
    /// adapter does not yet model that, so leaving risk evaluation on by
    /// default would over-trip `TooManyOpenOrders*` caps. Scenarios that
    /// explicitly want to validate the risk strand turn this on.
    risk_evaluation_enabled: bool,
    /// Track open intents so we can map `SimulatedFill.client_order_id` back
    /// to the originating `OrderIntent`'s leg, side, and market.
    open_intents: BTreeMap<String, IntentRecord>,
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
    leg: Leg,
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
            risk_evaluation_enabled: false,
            open_intents: BTreeMap::new(),
            coid_by_strategy_slot: BTreeMap::new(),
            starting_cash_usd: DEFAULT_STARTING_CASH_USD,
            sequence: 0,
        }
    }

    /// Enable in-band `RiskEngine` evaluation of every intent. Disabled
    /// by default; see `risk_evaluation_enabled` field docs for why.
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
        let pair_cost = PairCostTracker::from_inventory(&inventory);
        let btc_regime = self.btc_regime.snapshot();
        let fair_value = self.fair_value_for(market, &btc_regime, now_ms);
        Some(StrategyInput {
            market: market.clone(),
            snapshot,
            inventory,
            pair_cost,
            fair_value,
            btc_regime,
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
        self.open_intents.insert(
            coid.clone(),
            IntentRecord {
                market_id: market.market_id.clone(),
                instrument_id: intent.instrument_id.clone(),
                side: intent.side,
                limit_price: intent.limit_price,
                quantity: intent.quantity,
                leg,
            },
        );
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
            // Live trader's `OrderIntent` does not yet expose a typed
            // aggressive/post-only flag, so the adapter defaults to the
            // resting maker semantics. Hedge-rescue (Close) intents are
            // FAK-style aggressive lifts; mark them as taker so the fill
            // simulator crosses the book immediately. This preserves the
            // "rescue completes the pair" invariant exercised by scenario
            // fixture #4.
            aggressive: matches!(intent.kind, IntentKind::Close),
            post_only: false,
        })
    }

    fn handle_decision(
        &mut self,
        decision: StrategyDecision,
        market: &BinaryOutcomeMarket,
        now_ms: u64,
        out: &mut ReplayDecision,
    ) {
        match decision {
            StrategyDecision::QuoteSet { intents, .. }
            | StrategyDecision::CapitalRecycle { intents, .. }
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
            StrategyDecision::Merge { .. }
            | StrategyDecision::Suppress { .. }
            | StrategyDecision::Noop { .. } => {}
        }
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
                out.cancels.push(prior.clone());
                self.open_intents.remove(&prior);
            }
            if let Some(sim_intent) = self.convert_intent(intent, market, now_ms) {
                self.coid_by_strategy_slot
                    .insert(strategy_slot, sim_intent.client_order_id.clone());
                out.submits.push(sim_intent);
            }
            return;
        }
        let runtime_inv = self
            .runtime_inventories
            .entry(market.market_id.clone())
            .or_insert_with(|| RuntimeInventoryState::new(self.starting_cash_usd));
        // Replay open-order counters mirror submit/fill events. Without a
        // cancel path the counter would grow unbounded; the adapter does
        // not emit cancels today (the live trader manages cancel/replace
        // through the venue, not exposed here), so we synthesise a count
        // from the live `open_intents` map instead. This is the same set
        // the simulator considers "resting" and matches the live engine's
        // book-of-record.
        let live_total = self.open_intents.len();
        let live_for_market = self
            .open_intents
            .values()
            .filter(|r| r.market_id == market.market_id)
            .count();
        let open_buy_notional_total_usd = self
            .open_intents
            .values()
            .filter(|record| matches!(record.side, TradeSide::Buy))
            .map(|record| record.limit_price * record.quantity)
            .sum();
        let open_signed_notional_for_market_usd = self
            .open_intents
            .values()
            .filter(|record| record.market_id == market.market_id)
            .map(|record| record.limit_price * record.quantity * record.side.sign())
            .sum();
        let open_position_qty_for_instrument = self
            .open_intents
            .values()
            .filter(|record| record.instrument_id == intent.instrument_id)
            .map(|record| record.quantity * record.side.sign())
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
        // runtime would cancel the prior order before placing the new one.
        // Emit an explicit cancel so the simulator drops the prior resting
        // order and our open_intents map stays in sync.
        if let Some(prior) = self.coid_by_strategy_slot.get(&strategy_slot).cloned() {
            out.cancels.push(prior.clone());
            self.open_intents.remove(&prior);
        }
        if let Some(sim_intent) = self.convert_intent(intent, market, now_ms) {
            self.coid_by_strategy_slot
                .insert(strategy_slot, sim_intent.client_order_id.clone());
            self.open_orders_total = self.open_orders_total.saturating_add(1);
            *self
                .open_orders_per_market
                .entry(market.market_id.clone())
                .or_insert(0) += 1;
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
        let inventory = self
            .inventories
            .entry(record.market_id.clone())
            .or_insert_with(|| InventoryState::new(self.starting_cash_usd));
        inventory.apply_fill(record.leg, record.side, fill.price, fill.size);

        // Decrement open-orders counters; the order has filled. Remove
        // from the open-intent map so subsequent risk evaluations see the
        // correct live count.
        self.open_orders_total = self.open_orders_total.saturating_sub(1);
        if let Some(c) = self.open_orders_per_market.get_mut(&record.market_id) {
            *c = c.saturating_sub(1);
        }
        self.open_intents.remove(&fill.client_order_id);

        // Build StrategyFillInput and notify the strategy.
        let Some(market) = self.markets.get(&record.market_id).cloned() else {
            return ReplayDecision::default();
        };
        let now_ms = fill.fill_ms;
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
        // Mirror the fill onto the runtime inventory used by the risk
        // engine so subsequent intents see post-fill exposure.
        if let Some(runtime_inv) = self.runtime_inventories.get_mut(&record.market_id) {
            let _ = runtime_inv.apply_fill(&fill_report);
        }
        let fill_input = StrategyFillInput {
            market: market.clone(),
            snapshot: input.snapshot,
            fair_value: input.fair_value,
            fill: fill_report,
        };
        let mut decision = ReplayDecision::default();
        let decisions = self.registry.on_fill(market.market_id.as_str(), fill_input);
        for d in decisions {
            self.handle_decision(d, &market, now_ms, &mut decision);
        }
        decision
    }
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
    use crate::replay::runner::{RunnerConfig, WindowStatus, run_window};
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
        };
        let summary = run_window(&mut adapter, &events, &cfg);
        assert_eq!(summary.status, WindowStatus::Ok);
        assert_eq!(summary.fills.len(), 0);
    }
}
