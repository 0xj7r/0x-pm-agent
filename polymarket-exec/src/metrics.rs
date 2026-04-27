//! Prometheus metrics registry and update helpers for runtime, books, and execution.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use prometheus::{
    Encoder, Gauge, GaugeVec, Histogram, HistogramOpts, IntCounter, IntCounterVec, IntGauge, Opts,
    Registry, TextEncoder,
};

use crate::book::BookState;
use crate::types::{FillLiquidity, FillReport};

#[derive(Debug, Clone, Copy)]
pub enum StreamKind {
    Market,
    User,
}

impl StreamKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Market => "market",
            Self::User => "user",
        }
    }
}

#[derive(Debug)]
pub struct AppMetrics {
    registry: Registry,
    market_ws_connected: IntGauge,
    user_ws_connected: IntGauge,
    execution_adapter_connected: IntGauge,
    market_messages_total: IntCounterVec,
    user_messages_total: IntCounterVec,
    reconnects_total: IntCounterVec,
    runtime_loop_seconds: Histogram,
    book_age_ms: GaugeVec,
    book_best_bid: GaugeVec,
    book_best_ask: GaugeVec,
    book_spread: GaugeVec,
    stale_books_total: IntCounterVec,
    book_stale_events_total: IntCounter,
    market_last_message_age_ms: Gauge,
    user_last_message_age_ms: Gauge,
    last_reconcile_age_ms: Gauge,
    quote_ladder_count: IntGauge,
    quote_max_per_side_usd: Gauge,
    quote_min_edge_bps: Gauge,
    quote_skew_cap_bps: Gauge,
    quote_edge_bps: Gauge,
    quote_age_ms: Gauge,
    quote_refresh_interval_ms: Gauge,
    fill_total: IntCounter,
    fill_maker_total: IntCounter,
    fill_taker_total: IntCounter,
    fill_maker_share: Gauge,
    fill_notional_usd_total: Gauge,
    pair_completed_qty_total: Gauge,
    stranded_yes_qty: Gauge,
    stranded_no_qty: Gauge,
    merge_candidate_qty: Gauge,
    merge_latency_ms: Gauge,
    reconcile_failures_total: IntCounter,
    runtime_riskoff_transitions_total: IntCounter,
    uncertain_submit_total: IntCounter,
    venue_cash_usd: Gauge,
    venue_position_count: IntGauge,
    inventory_skew_usd: Gauge,
    realized_pnl_usd: Gauge,
    unrealized_pnl_usd: Gauge,
    fees_usd_total: Gauge,
    rebates_usd_total: Gauge,
    /// Count of our currently-open orders that the venue says are
    /// scoring for maker rewards. Set by periodic /order-scoring sweep.
    /// Diagnostic only — strategy doesn't gate on this yet.
    orders_scoring_total: IntGauge,
    /// Count of our currently-open orders that the venue says are NOT
    /// scoring (outside spread, too small, too fresh, etc.). High value
    /// here means we're posting but capturing zero of the rebate edge.
    orders_non_scoring_total: IntGauge,
    net_edge_usd_total: Gauge,
    strategy_events_total: IntCounterVec,
    strategy_intents_total: IntCounterVec,
    market_last_message_unix_ms: AtomicU64,
    user_last_message_unix_ms: AtomicU64,
    last_reconcile_unix_ms: AtomicU64,
}

#[derive(Debug, Clone, Default)]
pub struct ControlPlaneMetricsSnapshot {
    pub market_ws_connected: bool,
    pub user_ws_connected: bool,
    pub execution_adapter_connected: bool,
    pub market_last_message_age_ms: f64,
    pub user_last_message_age_ms: f64,
    pub last_reconcile_age_ms: f64,
    pub quote_ladder_count: usize,
    pub quote_max_per_side_usd: f64,
    pub quote_min_edge_bps: f64,
    pub quote_skew_cap_bps: f64,
    pub quote_edge_bps: f64,
    pub quote_age_ms: f64,
    pub quote_refresh_interval_ms: f64,
    pub fill_total: u64,
    pub fill_maker_total: u64,
    pub fill_taker_total: u64,
    pub fill_maker_share: f64,
    pub fill_notional_usd_total: f64,
    pub pair_completed_qty_total: f64,
    pub stranded_yes_qty: f64,
    pub stranded_no_qty: f64,
    pub merge_candidate_qty: f64,
    pub merge_latency_ms: f64,
    pub book_stale_events_total: u64,
    pub reconcile_failures_total: u64,
    pub runtime_riskoff_transitions_total: u64,
    pub uncertain_submit_total: u64,
    pub venue_cash_usd: f64,
    pub venue_position_count: usize,
    pub inventory_skew_usd: f64,
    pub realized_pnl_usd: f64,
    pub unrealized_pnl_usd: f64,
    pub fees_usd_total: f64,
    pub rebates_usd_total: f64,
    pub orders_scoring_total: i64,
    pub orders_non_scoring_total: i64,
    pub net_edge_usd_total: f64,
}

impl AppMetrics {
    pub fn new() -> Result<Self> {
        let registry = Registry::new_custom(Some("polymarket_exec".to_string()), None)
            .context("failed to construct prometheus registry")?;

        let market_ws_connected = IntGauge::with_opts(Opts::new(
            "market_ws_connected",
            "Market websocket connected",
        ))?;
        let user_ws_connected =
            IntGauge::with_opts(Opts::new("user_ws_connected", "User websocket connected"))?;
        let execution_adapter_connected = IntGauge::with_opts(Opts::new(
            "execution_adapter_connected",
            "Execution adapter connected",
        ))?;
        let market_messages_total = IntCounterVec::new(
            Opts::new(
                "market_ws_messages_total",
                "Market websocket messages by event type",
            ),
            &["event_type"],
        )?;
        let user_messages_total = IntCounterVec::new(
            Opts::new(
                "user_ws_messages_total",
                "User websocket messages by event type and status",
            ),
            &["event_type", "status"],
        )?;
        let reconnects_total = IntCounterVec::new(
            Opts::new("ws_reconnects_total", "Websocket reconnect attempts"),
            &["stream"],
        )?;
        let runtime_loop_seconds = Histogram::with_opts(HistogramOpts::new(
            "runtime_loop_seconds",
            "Runtime loop duration in seconds",
        ))?;
        let book_age_ms = GaugeVec::new(
            Opts::new("book_age_ms", "Age of the most recent top-of-book update"),
            &["asset_id"],
        )?;
        let book_best_bid = GaugeVec::new(
            Opts::new("book_best_bid", "Best bid by asset"),
            &["asset_id"],
        )?;
        let book_best_ask = GaugeVec::new(
            Opts::new("book_best_ask", "Best ask by asset"),
            &["asset_id"],
        )?;
        let book_spread = GaugeVec::new(
            Opts::new("book_spread", "Top-of-book spread by asset"),
            &["asset_id"],
        )?;
        let stale_books_total = IntCounterVec::new(
            Opts::new("book_stale_total", "Count of stale book observations"),
            &["asset_id"],
        )?;
        let book_stale_events_total = IntCounter::new(
            "book_stale_events_total",
            "Count of stale book observations across all assets",
        )?;
        let market_last_message_age_ms = Gauge::with_opts(Opts::new(
            "market_ws_last_message_age_ms",
            "Age of the last market websocket message",
        ))?;
        let user_last_message_age_ms = Gauge::with_opts(Opts::new(
            "user_ws_last_message_age_ms",
            "Age of the last user websocket message",
        ))?;
        let last_reconcile_age_ms = Gauge::with_opts(Opts::new(
            "last_reconcile_age_ms",
            "Age of the last reconcile tick",
        ))?;
        let quote_ladder_count = IntGauge::with_opts(Opts::new(
            "quote_ladder_count",
            "Current number of active quote ladder levels",
        ))?;
        let quote_max_per_side_usd = Gauge::with_opts(Opts::new(
            "quote_max_per_side_usd",
            "Configured max quote notional per side",
        ))?;
        let quote_min_edge_bps = Gauge::with_opts(Opts::new(
            "quote_min_edge_bps",
            "Configured minimum edge threshold in basis points",
        ))?;
        let quote_skew_cap_bps = Gauge::with_opts(Opts::new(
            "quote_skew_cap_bps",
            "Configured skew cap in basis points",
        ))?;
        let quote_edge_bps = Gauge::with_opts(Opts::new(
            "quote_edge_bps",
            "Observed quote edge in basis points",
        ))?;
        let quote_age_ms = Gauge::with_opts(Opts::new(
            "quote_age_ms",
            "Age of the oldest active quote in milliseconds",
        ))?;
        let quote_refresh_interval_ms = Gauge::with_opts(Opts::new(
            "quote_refresh_interval_ms",
            "Configured quote refresh interval in milliseconds",
        ))?;
        let fill_total = IntCounter::new("fill_total", "Total fills observed")?;
        let fill_maker_total = IntCounter::new("fill_maker_total", "Maker fills observed")?;
        let fill_taker_total = IntCounter::new("fill_taker_total", "Taker fills observed")?;
        let fill_maker_share = Gauge::with_opts(Opts::new(
            "fill_maker_share",
            "Share of maker fills observed",
        ))?;
        let fill_notional_usd_total = Gauge::with_opts(Opts::new(
            "fill_notional_usd_total",
            "Cumulative notional on fills in USD",
        ))?;
        let pair_completed_qty_total = Gauge::with_opts(Opts::new(
            "pair_completed_qty_total",
            "Cumulative pair-completion quantity",
        ))?;
        let stranded_yes_qty = Gauge::with_opts(Opts::new(
            "stranded_yes_qty",
            "Current stranded yes-side quantity",
        ))?;
        let stranded_no_qty = Gauge::with_opts(Opts::new(
            "stranded_no_qty",
            "Current stranded no-side quantity",
        ))?;
        let merge_candidate_qty = Gauge::with_opts(Opts::new(
            "merge_candidate_qty",
            "Current merge candidate quantity",
        ))?;
        let merge_latency_ms = Gauge::with_opts(Opts::new(
            "merge_latency_ms",
            "Observed merge latency in milliseconds",
        ))?;
        let reconcile_failures_total =
            IntCounter::new("reconcile_failures_total", "Count of reconcile failures")?;
        let runtime_riskoff_transitions_total = IntCounter::new(
            "runtime_riskoff_transitions_total",
            "Count of runtime risk-off transitions",
        )?;
        let uncertain_submit_total = IntCounter::new(
            "uncertain_submit_total",
            "Count of uncertain submit outcomes",
        )?;
        let venue_cash_usd = Gauge::with_opts(Opts::new(
            "venue_cash_usd",
            "Latest synced venue cash balance in USD",
        ))?;
        let venue_position_count = IntGauge::with_opts(Opts::new(
            "venue_position_count",
            "Latest synced venue position count",
        ))?;
        let inventory_skew_usd = Gauge::with_opts(Opts::new(
            "inventory_skew_usd",
            "Current inventory skew in USD",
        ))?;
        let realized_pnl_usd =
            Gauge::with_opts(Opts::new("realized_pnl_usd", "Realized PnL in USD"))?;
        let unrealized_pnl_usd =
            Gauge::with_opts(Opts::new("unrealized_pnl_usd", "Unrealized PnL in USD"))?;
        let fees_usd_total = Gauge::with_opts(Opts::new("fees_usd_total", "Fees in USD"))?;
        let rebates_usd_total = Gauge::with_opts(Opts::new("rebates_usd_total", "Rebates in USD"))?;
        let orders_scoring_total = IntGauge::with_opts(Opts::new(
            "orders_scoring_total",
            "Open orders currently scoring for maker rewards",
        ))?;
        let orders_non_scoring_total = IntGauge::with_opts(Opts::new(
            "orders_non_scoring_total",
            "Open orders NOT scoring for maker rewards (out of spread, too small, too fresh)",
        ))?;
        let net_edge_usd_total =
            Gauge::with_opts(Opts::new("net_edge_usd_total", "Net edge in USD"))?;
        let strategy_events_total = IntCounterVec::new(
            Opts::new(
                "strategy_events_total",
                "Classified strategy/risk events for live attribution",
            ),
            &["event"],
        )?;
        let strategy_intents_total = IntCounterVec::new(
            Opts::new(
                "strategy_intents_total",
                "Classified strategy intents emitted before execution filtering",
            ),
            &["intent"],
        )?;

        registry.register(Box::new(market_ws_connected.clone()))?;
        registry.register(Box::new(user_ws_connected.clone()))?;
        registry.register(Box::new(execution_adapter_connected.clone()))?;
        registry.register(Box::new(market_messages_total.clone()))?;
        registry.register(Box::new(user_messages_total.clone()))?;
        registry.register(Box::new(reconnects_total.clone()))?;
        registry.register(Box::new(runtime_loop_seconds.clone()))?;
        registry.register(Box::new(book_age_ms.clone()))?;
        registry.register(Box::new(book_best_bid.clone()))?;
        registry.register(Box::new(book_best_ask.clone()))?;
        registry.register(Box::new(book_spread.clone()))?;
        registry.register(Box::new(stale_books_total.clone()))?;
        registry.register(Box::new(book_stale_events_total.clone()))?;
        registry.register(Box::new(market_last_message_age_ms.clone()))?;
        registry.register(Box::new(user_last_message_age_ms.clone()))?;
        registry.register(Box::new(last_reconcile_age_ms.clone()))?;
        registry.register(Box::new(quote_ladder_count.clone()))?;
        registry.register(Box::new(quote_max_per_side_usd.clone()))?;
        registry.register(Box::new(quote_min_edge_bps.clone()))?;
        registry.register(Box::new(quote_skew_cap_bps.clone()))?;
        registry.register(Box::new(quote_edge_bps.clone()))?;
        registry.register(Box::new(quote_age_ms.clone()))?;
        registry.register(Box::new(quote_refresh_interval_ms.clone()))?;
        registry.register(Box::new(fill_total.clone()))?;
        registry.register(Box::new(fill_maker_total.clone()))?;
        registry.register(Box::new(fill_taker_total.clone()))?;
        registry.register(Box::new(fill_maker_share.clone()))?;
        registry.register(Box::new(fill_notional_usd_total.clone()))?;
        registry.register(Box::new(pair_completed_qty_total.clone()))?;
        registry.register(Box::new(stranded_yes_qty.clone()))?;
        registry.register(Box::new(stranded_no_qty.clone()))?;
        registry.register(Box::new(merge_candidate_qty.clone()))?;
        registry.register(Box::new(merge_latency_ms.clone()))?;
        registry.register(Box::new(reconcile_failures_total.clone()))?;
        registry.register(Box::new(runtime_riskoff_transitions_total.clone()))?;
        registry.register(Box::new(uncertain_submit_total.clone()))?;
        registry.register(Box::new(venue_cash_usd.clone()))?;
        registry.register(Box::new(venue_position_count.clone()))?;
        registry.register(Box::new(inventory_skew_usd.clone()))?;
        registry.register(Box::new(realized_pnl_usd.clone()))?;
        registry.register(Box::new(unrealized_pnl_usd.clone()))?;
        registry.register(Box::new(fees_usd_total.clone()))?;
        registry.register(Box::new(rebates_usd_total.clone()))?;
        registry.register(Box::new(orders_scoring_total.clone()))?;
        registry.register(Box::new(orders_non_scoring_total.clone()))?;
        registry.register(Box::new(net_edge_usd_total.clone()))?;
        registry.register(Box::new(strategy_events_total.clone()))?;
        registry.register(Box::new(strategy_intents_total.clone()))?;

        Ok(Self {
            registry,
            market_ws_connected,
            user_ws_connected,
            execution_adapter_connected,
            market_messages_total,
            user_messages_total,
            reconnects_total,
            runtime_loop_seconds,
            book_age_ms,
            book_best_bid,
            book_best_ask,
            book_spread,
            stale_books_total,
            book_stale_events_total,
            market_last_message_age_ms,
            user_last_message_age_ms,
            last_reconcile_age_ms,
            quote_ladder_count,
            quote_max_per_side_usd,
            quote_min_edge_bps,
            quote_skew_cap_bps,
            quote_edge_bps,
            quote_age_ms,
            quote_refresh_interval_ms,
            fill_total,
            fill_maker_total,
            fill_taker_total,
            fill_maker_share,
            fill_notional_usd_total,
            pair_completed_qty_total,
            stranded_yes_qty,
            stranded_no_qty,
            merge_candidate_qty,
            merge_latency_ms,
            reconcile_failures_total,
            runtime_riskoff_transitions_total,
            uncertain_submit_total,
            venue_cash_usd,
            venue_position_count,
            inventory_skew_usd,
            realized_pnl_usd,
            unrealized_pnl_usd,
            fees_usd_total,
            rebates_usd_total,
            orders_scoring_total,
            orders_non_scoring_total,
            net_edge_usd_total,
            strategy_events_total,
            strategy_intents_total,
            market_last_message_unix_ms: AtomicU64::new(0),
            user_last_message_unix_ms: AtomicU64::new(0),
            last_reconcile_unix_ms: AtomicU64::new(0),
        })
    }

    pub fn runtime_loop_timer(&self) -> prometheus::HistogramTimer {
        self.runtime_loop_seconds.start_timer()
    }

    pub fn set_stream_connected(&self, stream: StreamKind, connected: bool) {
        let value = if connected { 1 } else { 0 };
        match stream {
            StreamKind::Market => self.market_ws_connected.set(value),
            StreamKind::User => self.user_ws_connected.set(value),
        }
    }

    pub fn inc_reconnect(&self, stream: StreamKind) {
        self.reconnects_total
            .with_label_values(&[stream.as_str()])
            .inc();
    }

    pub fn observe_market_message(&self, event_type: &str) {
        self.market_messages_total
            .with_label_values(&[event_type])
            .inc();
        self.touch_stream(StreamKind::Market);
    }

    pub fn observe_user_message(&self, event_type: &str, status: &str) {
        self.user_messages_total
            .with_label_values(&[event_type, status])
            .inc();
        self.touch_stream(StreamKind::User);
    }

    pub fn observe_book(&self, book: &BookState, stale_after: Duration) {
        let asset = book.asset_id.as_str();
        self.book_best_bid
            .with_label_values(&[asset])
            .set(book.best_bid);
        self.book_best_ask
            .with_label_values(&[asset])
            .set(book.best_ask);
        self.book_spread
            .with_label_values(&[asset])
            .set(book.spread);
        let age_ms = book.age_ms().unwrap_or_default() as f64;
        self.book_age_ms.with_label_values(&[asset]).set(age_ms);
        if book.is_stale(stale_after) {
            self.stale_books_total.with_label_values(&[asset]).inc();
            self.book_stale_events_total.inc();
        }
    }

    pub fn observe_missing_book(&self, asset_id: &str) {
        self.book_age_ms.with_label_values(&[asset_id]).set(-1.0);
        self.book_best_bid.with_label_values(&[asset_id]).set(0.0);
        self.book_best_ask.with_label_values(&[asset_id]).set(0.0);
        self.book_spread.with_label_values(&[asset_id]).set(0.0);
        self.stale_books_total.with_label_values(&[asset_id]).inc();
        self.book_stale_events_total.inc();
    }

    pub fn refresh_stream_ages(&self) {
        self.market_last_message_age_ms.set(age_ms(
            self.market_last_message_unix_ms.load(Ordering::Relaxed),
        ));
        self.user_last_message_age_ms.set(age_ms(
            self.user_last_message_unix_ms.load(Ordering::Relaxed),
        ));
        self.last_reconcile_age_ms
            .set(age_ms(self.last_reconcile_unix_ms.load(Ordering::Relaxed)));
    }

    fn touch_stream(&self, stream: StreamKind) {
        let now_ms = now_unix_ms();
        match stream {
            StreamKind::Market => self
                .market_last_message_unix_ms
                .store(now_ms, Ordering::Relaxed),
            StreamKind::User => self
                .user_last_message_unix_ms
                .store(now_ms, Ordering::Relaxed),
        }
    }

    pub fn set_execution_adapter_connected(&self, connected: bool) {
        self.execution_adapter_connected
            .set(if connected { 1 } else { 0 });
    }

    pub fn touch_reconcile(&self) {
        self.last_reconcile_unix_ms
            .store(now_unix_ms(), Ordering::Relaxed);
    }

    pub fn observe_uncertain_submit(&self) {
        self.uncertain_submit_total.inc();
    }

    pub fn observe_reconcile_failure(&self) {
        self.reconcile_failures_total.inc();
    }

    pub fn observe_riskoff_transition(&self) {
        self.runtime_riskoff_transitions_total.inc();
    }

    pub fn observe_strategy_event(&self, event: &str) {
        self.strategy_events_total.with_label_values(&[event]).inc();
    }

    pub fn observe_strategy_intent(&self, intent: &str) {
        self.strategy_intents_total
            .with_label_values(&[intent])
            .inc();
    }

    pub fn record_fill(&self, fill: &FillReport, merge_latency_ms: Option<u64>) {
        self.fill_total.inc();
        match fill.liquidity {
            FillLiquidity::Maker => self.fill_maker_total.inc(),
            FillLiquidity::Taker => self.fill_taker_total.inc(),
            FillLiquidity::Unknown => {}
        }

        let current_total = self.fill_total.get() as f64;
        if current_total > 0.0 {
            self.fill_maker_share
                .set(self.fill_maker_total.get() as f64 / current_total);
        }

        self.fill_notional_usd_total
            .set(self.fill_notional_usd_total.get() + fill.notional_usd().max(0.0));
        if fill.fee_usd >= 0.0 {
            self.fees_usd_total
                .set(self.fees_usd_total.get() + fill.fee_usd);
        } else {
            self.rebates_usd_total
                .set(self.rebates_usd_total.get() + fill.fee_usd.abs());
        }
        if let Some(latency_ms) = merge_latency_ms {
            self.pair_completed_qty_total
                .set(self.pair_completed_qty_total.get() + fill.quantity.max(0.0));
            self.merge_latency_ms.set(latency_ms as f64);
        }
    }

    pub fn set_quote_metrics(
        &self,
        ladder_count: usize,
        max_per_side_usd: f64,
        min_edge_bps: f64,
        skew_cap_bps: f64,
        refresh_interval_ms: u64,
        edge_bps: f64,
        age_ms: f64,
    ) {
        self.quote_ladder_count.set(ladder_count as i64);
        self.quote_max_per_side_usd.set(max_per_side_usd);
        self.quote_min_edge_bps.set(min_edge_bps);
        self.quote_skew_cap_bps.set(skew_cap_bps);
        self.quote_refresh_interval_ms
            .set(refresh_interval_ms as f64);
        self.quote_edge_bps.set(edge_bps);
        self.quote_age_ms.set(age_ms);
    }

    pub fn set_pair_metrics(
        &self,
        stranded_yes_qty: f64,
        stranded_no_qty: f64,
        merge_candidate_qty: f64,
    ) {
        self.stranded_yes_qty.set(stranded_yes_qty);
        self.stranded_no_qty.set(stranded_no_qty);
        self.merge_candidate_qty.set(merge_candidate_qty);
    }

    pub fn set_risk_metrics(&self, inventory_skew_usd: f64) {
        self.inventory_skew_usd.set(inventory_skew_usd);
    }

    pub fn set_venue_balance_metrics(&self, cash_usd: f64, position_count: usize) {
        self.venue_cash_usd.set(cash_usd);
        self.venue_position_count.set(position_count as i64);
    }

    pub fn set_economics_metrics(
        &self,
        realized_pnl_usd: f64,
        unrealized_pnl_usd: f64,
        net_edge_usd_total: f64,
    ) {
        self.realized_pnl_usd.set(realized_pnl_usd);
        self.unrealized_pnl_usd.set(unrealized_pnl_usd);
        self.net_edge_usd_total.set(net_edge_usd_total);
    }

    pub fn set_health_metrics(
        &self,
        last_reconcile_age_ms: f64,
        execution_adapter_connected: bool,
    ) {
        self.last_reconcile_age_ms.set(last_reconcile_age_ms);
        self.execution_adapter_connected
            .set(if execution_adapter_connected { 1 } else { 0 });
    }

    pub fn snapshot(&self) -> ControlPlaneMetricsSnapshot {
        ControlPlaneMetricsSnapshot {
            market_ws_connected: self.market_ws_connected.get() != 0,
            user_ws_connected: self.user_ws_connected.get() != 0,
            execution_adapter_connected: self.execution_adapter_connected.get() != 0,
            market_last_message_age_ms: self.market_last_message_age_ms.get(),
            user_last_message_age_ms: self.user_last_message_age_ms.get(),
            last_reconcile_age_ms: self.last_reconcile_age_ms.get(),
            quote_ladder_count: self.quote_ladder_count.get().max(0) as usize,
            quote_max_per_side_usd: self.quote_max_per_side_usd.get(),
            quote_min_edge_bps: self.quote_min_edge_bps.get(),
            quote_skew_cap_bps: self.quote_skew_cap_bps.get(),
            quote_edge_bps: self.quote_edge_bps.get(),
            quote_age_ms: self.quote_age_ms.get(),
            quote_refresh_interval_ms: self.quote_refresh_interval_ms.get(),
            fill_total: self.fill_total.get(),
            fill_maker_total: self.fill_maker_total.get(),
            fill_taker_total: self.fill_taker_total.get(),
            fill_maker_share: self.fill_maker_share.get(),
            fill_notional_usd_total: self.fill_notional_usd_total.get(),
            pair_completed_qty_total: self.pair_completed_qty_total.get(),
            stranded_yes_qty: self.stranded_yes_qty.get(),
            stranded_no_qty: self.stranded_no_qty.get(),
            merge_candidate_qty: self.merge_candidate_qty.get(),
            merge_latency_ms: self.merge_latency_ms.get(),
            book_stale_events_total: self.book_stale_events_total.get(),
            reconcile_failures_total: self.reconcile_failures_total.get(),
            runtime_riskoff_transitions_total: self.runtime_riskoff_transitions_total.get(),
            uncertain_submit_total: self.uncertain_submit_total.get(),
            venue_cash_usd: self.venue_cash_usd.get(),
            venue_position_count: self.venue_position_count.get().max(0) as usize,
            inventory_skew_usd: self.inventory_skew_usd.get(),
            realized_pnl_usd: self.realized_pnl_usd.get(),
            unrealized_pnl_usd: self.unrealized_pnl_usd.get(),
            fees_usd_total: self.fees_usd_total.get(),
            rebates_usd_total: self.rebates_usd_total.get(),
            orders_scoring_total: self.orders_scoring_total.get(),
            orders_non_scoring_total: self.orders_non_scoring_total.get(),
            net_edge_usd_total: self.net_edge_usd_total.get(),
        }
    }

    /// Update the rebate-eligibility gauges from a /order-scoring or
    /// /orders-scoring sweep. Pass the count of currently-open orders
    /// the venue says ARE scoring vs AREN'T.
    pub fn record_order_scoring_counts(&self, scoring: usize, non_scoring: usize) {
        self.orders_scoring_total.set(scoring as i64);
        self.orders_non_scoring_total.set(non_scoring as i64);
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let metric_families = self.registry.gather();
        let encoder = TextEncoder::new();
        let mut buffer = Vec::new();
        encoder
            .encode(&metric_families, &mut buffer)
            .context("failed to encode prometheus metrics")?;
        Ok(buffer)
    }
}

fn age_ms(last_message_unix_ms: u64) -> f64 {
    if last_message_unix_ms == 0 {
        return -1.0;
    }
    let now_ms = now_unix_ms();
    now_ms.saturating_sub(last_message_unix_ms) as f64
}

fn now_unix_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
