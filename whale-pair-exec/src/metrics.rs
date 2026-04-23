use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use prometheus::{
    Encoder, Gauge, GaugeVec, Histogram, HistogramOpts, IntCounterVec, IntGauge, Opts, Registry,
    TextEncoder,
};

use crate::book::BookState;

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
    market_messages_total: IntCounterVec,
    user_messages_total: IntCounterVec,
    reconnects_total: IntCounterVec,
    runtime_loop_seconds: Histogram,
    book_age_ms: GaugeVec,
    book_best_bid: GaugeVec,
    book_best_ask: GaugeVec,
    book_spread: GaugeVec,
    stale_books_total: IntCounterVec,
    market_last_message_age_ms: Gauge,
    user_last_message_age_ms: Gauge,
    market_last_message_unix_ms: AtomicU64,
    user_last_message_unix_ms: AtomicU64,
}

impl AppMetrics {
    pub fn new() -> Result<Self> {
        let registry = Registry::new_custom(Some("whale_pair_exec".to_string()), None)
            .context("failed to construct prometheus registry")?;

        let market_ws_connected =
            IntGauge::with_opts(Opts::new("market_ws_connected", "Market websocket connected"))?;
        let user_ws_connected =
            IntGauge::with_opts(Opts::new("user_ws_connected", "User websocket connected"))?;
        let market_messages_total = IntCounterVec::new(
            Opts::new("market_ws_messages_total", "Market websocket messages by event type"),
            &["event_type"],
        )?;
        let user_messages_total = IntCounterVec::new(
            Opts::new("user_ws_messages_total", "User websocket messages by event type and status"),
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
        let market_last_message_age_ms = Gauge::with_opts(Opts::new(
            "market_ws_last_message_age_ms",
            "Age of the last market websocket message",
        ))?;
        let user_last_message_age_ms = Gauge::with_opts(Opts::new(
            "user_ws_last_message_age_ms",
            "Age of the last user websocket message",
        ))?;

        registry.register(Box::new(market_ws_connected.clone()))?;
        registry.register(Box::new(user_ws_connected.clone()))?;
        registry.register(Box::new(market_messages_total.clone()))?;
        registry.register(Box::new(user_messages_total.clone()))?;
        registry.register(Box::new(reconnects_total.clone()))?;
        registry.register(Box::new(runtime_loop_seconds.clone()))?;
        registry.register(Box::new(book_age_ms.clone()))?;
        registry.register(Box::new(book_best_bid.clone()))?;
        registry.register(Box::new(book_best_ask.clone()))?;
        registry.register(Box::new(book_spread.clone()))?;
        registry.register(Box::new(stale_books_total.clone()))?;
        registry.register(Box::new(market_last_message_age_ms.clone()))?;
        registry.register(Box::new(user_last_message_age_ms.clone()))?;

        Ok(Self {
            registry,
            market_ws_connected,
            user_ws_connected,
            market_messages_total,
            user_messages_total,
            reconnects_total,
            runtime_loop_seconds,
            book_age_ms,
            book_best_bid,
            book_best_ask,
            book_spread,
            stale_books_total,
            market_last_message_age_ms,
            user_last_message_age_ms,
            market_last_message_unix_ms: AtomicU64::new(0),
            user_last_message_unix_ms: AtomicU64::new(0),
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
        }
    }

    pub fn observe_missing_book(&self, asset_id: &str) {
        self.book_age_ms.with_label_values(&[asset_id]).set(-1.0);
        self.book_best_bid.with_label_values(&[asset_id]).set(0.0);
        self.book_best_ask.with_label_values(&[asset_id]).set(0.0);
        self.book_spread.with_label_values(&[asset_id]).set(0.0);
        self.stale_books_total.with_label_values(&[asset_id]).inc();
    }

    pub fn refresh_stream_ages(&self) {
        self.market_last_message_age_ms
            .set(age_ms(self.market_last_message_unix_ms.load(Ordering::Relaxed)));
        self.user_last_message_age_ms
            .set(age_ms(self.user_last_message_unix_ms.load(Ordering::Relaxed)));
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
