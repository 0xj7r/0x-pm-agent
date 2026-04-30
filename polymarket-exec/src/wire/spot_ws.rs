//! Spot trade websocket client feeding BTC regime telemetry into the runtime.

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::DateTime;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::{interval, sleep, MissedTickBehavior};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::metrics::AppMetrics;

/// Two-rail liveness model for the spot websocket.
///
/// 2026-04-29 production incident: Binance spot WS silently stalled at
/// 22:55 UTC, no messages for 2+ hours, no error events. Vol calculation's
/// 5-min sliding window drained, regime classifier returned None,
/// paired-MM ran without regime gate protection during a directional run.
/// Material PnL bleed.
///
/// Single-rail "any frame received" liveness is INSUFFICIENT because the
/// venue can keep the transport alive (control frames, pongs) while
/// silently dropping the aggTrade subscription. We need both:
///
/// 1. **Connection rail**: any inbound frame proves the socket is alive.
///    Resets on text/binary/ping/pong/close. Failure → bail and reconnect.
/// 2. **Data rail**: only PARSED aggTrade messages reset this. Failure
///    means the BTC trade tape isn't flowing — regime telemetry is
///    going stale even if the connection appears healthy. Failure → bail
///    and reconnect (more aggressive than just marking unhealthy because
///    the strategy's regime gate is the load-bearing protection).
///
/// Defaults are env-configurable via `WHALE_PAIR_EXEC_SPOT_WS_*_TIMEOUT_MS`.
const DEFAULT_CONN_STALE_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_DATA_STALE_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_BINANCE_REST_BOOTSTRAP_URL: &str = "https://api.binance.com/api/v3/aggTrades";
const DEFAULT_COINBASE_WS_URL: &str = "wss://advanced-trade-ws.coinbase.com";
const BINANCE_REST_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub enum SpotFeedSource {
    BinanceRestBootstrap,
    BinanceWs,
    CoinbaseWs,
}

impl SpotFeedSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::BinanceRestBootstrap => "binance_rest_bootstrap",
            Self::BinanceWs => "binance_ws",
            Self::CoinbaseWs => "coinbase_ws",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SpotTradeEvent {
    pub symbol: String,
    pub source: SpotFeedSource,
    pub price: f64,
    pub quantity: f64,
    pub observed_at_ms: u64,
}

#[derive(Clone)]
pub struct SpotWsClient {
    url: String,
    coinbase_ws_url: Option<String>,
    binance_rest_bootstrap_url: Option<String>,
    symbol: String,
    coinbase_product_id: String,
    ping_interval: Duration,
    conn_stale_timeout: Duration,
    data_stale_timeout: Duration,
    metrics: Arc<AppMetrics>,
    last_binance_trade_ingest_ms: Arc<AtomicU64>,
    event_tx: Option<mpsc::UnboundedSender<SpotTradeEvent>>,
}

impl SpotWsClient {
    pub fn new(
        url: String,
        symbol: String,
        ping_interval: Duration,
        metrics: Arc<AppMetrics>,
        event_tx: Option<mpsc::UnboundedSender<SpotTradeEvent>>,
    ) -> Self {
        Self::with_timeouts(
            url,
            symbol,
            ping_interval,
            DEFAULT_CONN_STALE_TIMEOUT,
            DEFAULT_DATA_STALE_TIMEOUT,
            Some(DEFAULT_BINANCE_REST_BOOTSTRAP_URL.to_string()),
            Some(DEFAULT_COINBASE_WS_URL.to_string()),
            metrics,
            event_tx,
        )
    }

    pub fn with_timeouts(
        url: String,
        symbol: String,
        ping_interval: Duration,
        conn_stale_timeout: Duration,
        data_stale_timeout: Duration,
        binance_rest_bootstrap_url: Option<String>,
        coinbase_ws_url: Option<String>,
        metrics: Arc<AppMetrics>,
        event_tx: Option<mpsc::UnboundedSender<SpotTradeEvent>>,
    ) -> Self {
        Self {
            url,
            coinbase_ws_url,
            binance_rest_bootstrap_url,
            symbol: symbol.to_ascii_uppercase(),
            coinbase_product_id: coinbase_product_for_symbol(&symbol),
            ping_interval,
            conn_stale_timeout,
            data_stale_timeout,
            metrics,
            last_binance_trade_ingest_ms: Arc::new(AtomicU64::new(0)),
            event_tx,
        }
    }

    pub async fn run(self, shutdown: CancellationToken) {
        let coinbase_handle = self.coinbase_ws_url.as_ref().map(|_| {
            let client = self.clone();
            let child_shutdown = shutdown.child_token();
            tokio::spawn(async move {
                client.run_coinbase(child_shutdown).await;
            })
        });
        self.run_binance(shutdown.clone()).await;
        if let Some(handle) = coinbase_handle {
            let _ = handle.await;
        }
    }

    async fn run_binance(&self, shutdown: CancellationToken) {
        let mut backoff = Duration::from_secs(1);
        while !shutdown.is_cancelled() {
            let _ = self.bootstrap_binance_rest(1_000).await;
            match self.run_once(shutdown.clone()).await {
                Ok(()) => break,
                Err(error) if shutdown.is_cancelled() => {
                    debug!(error = ?error, "spot websocket shutdown");
                    break;
                }
                Err(error) => {
                    warn!(
                        error = ?error,
                        backoff_ms = backoff.as_millis(),
                        "spot websocket loop failed; reconnecting"
                    );
                    sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                }
            }
        }
    }

    async fn run_coinbase(&self, shutdown: CancellationToken) {
        let Some(url) = self.coinbase_ws_url.as_deref() else {
            return;
        };
        let mut backoff = Duration::from_secs(1);
        while !shutdown.is_cancelled() {
            match self.run_coinbase_once(url, shutdown.clone()).await {
                Ok(()) => break,
                Err(error) if shutdown.is_cancelled() => {
                    debug!(error = ?error, "coinbase spot websocket shutdown");
                    break;
                }
                Err(error) => {
                    warn!(
                        error = ?error,
                        backoff_ms = backoff.as_millis(),
                        "coinbase spot websocket loop failed; reconnecting"
                    );
                    sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                }
            }
        }
    }

    async fn bootstrap_binance_rest(&self, limit: usize) -> usize {
        let Some(url) = self.binance_rest_bootstrap_url.as_deref() else {
            return 0;
        };
        let limit_string = limit.to_string();
        let request = reqwest::Client::new().get(url).query(&[
            ("symbol", self.symbol.as_str()),
            ("limit", limit_string.as_str()),
        ]);
        match request.send().await {
            Ok(response) => match response.error_for_status() {
                Ok(response) => match response.json::<Value>().await {
                    Ok(Value::Array(rows)) => {
                        let mut forwarded = 0usize;
                        for row in rows {
                            if let Some(event) = parse_binance_rest_trade(
                                &row,
                                &self.symbol,
                                SpotFeedSource::BinanceRestBootstrap,
                            ) {
                                self.forward_event(event);
                                forwarded += 1;
                            }
                        }
                        if forwarded > 0 {
                            info!(
                                symbol = %self.symbol,
                                forwarded,
                                "bootstrapped BTC regime samples from Binance REST"
                            );
                        }
                        forwarded
                    }
                    Ok(_) => {
                        warn!("Binance REST bootstrap returned non-array payload");
                        0
                    }
                    Err(error) => {
                        warn!(error = ?error, "failed to decode Binance REST bootstrap");
                        0
                    }
                },
                Err(error) => {
                    warn!(error = ?error, "Binance REST bootstrap returned error status");
                    0
                }
            },
            Err(error) => {
                warn!(error = ?error, "Binance REST bootstrap request failed");
                0
            }
        }
    }

    async fn run_once(&self, shutdown: CancellationToken) -> Result<()> {
        let (stream, _) = connect_async(self.url.as_str())
            .await
            .with_context(|| format!("failed to connect spot websocket {}", self.url))?;
        info!(symbol = %self.symbol, "spot websocket connected");

        let (mut write, mut read) = stream.split();
        let mut pings = interval(self.ping_interval);
        pings.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut rest_keepalive = interval(BINANCE_REST_KEEPALIVE_INTERVAL);
        rest_keepalive.set_missed_tick_behavior(MissedTickBehavior::Delay);

        // Two-rail liveness: connection rail tracks any frame, data rail
        // tracks only parsed aggTrade messages. Both must stay fresh.
        let now = Instant::now();
        let mut last_frame_at = now;
        let mut last_aggtrade_at = now;

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    let _ = write.send(Message::Close(None)).await;
                    return Ok(());
                }
                _ = pings.tick() => {
                    // Connection rail check: silent transport stall.
                    let conn_elapsed = last_frame_at.elapsed();
                    if conn_elapsed > self.conn_stale_timeout {
                        anyhow::bail!(
                            "spot websocket connection stale: no frames for {}s (timeout={}s); forcing reconnect",
                            conn_elapsed.as_secs(),
                            self.conn_stale_timeout.as_secs()
                        );
                    }
                    // Data rail check: subscription dropped silently
                    // (transport alive but aggTrade stream stopped).
                    // This is the failure mode that produced the 2026-04-29
                    // 2-hour regime-blind incident — control frames kept
                    // arriving while the actual BTC tape went silent.
                    let data_elapsed = last_aggtrade_at.elapsed();
                    if data_elapsed > self.data_stale_timeout {
                        anyhow::bail!(
                            "spot websocket data stale: no aggTrade for {}s (timeout={}s); forcing reconnect",
                            data_elapsed.as_secs(),
                            self.data_stale_timeout.as_secs()
                        );
                    }
                    write
                        .send(Message::Ping(Vec::new().into()))
                        .await
                        .context("failed to send spot websocket ping")?;
                }
                _ = rest_keepalive.tick(), if self.binance_rest_bootstrap_url.is_some() => {
                    // Keep signal windows warm even if WS becomes quiet.
                    // Lightweight poll: small most-recent aggTrades slice.
                    let forwarded = self.bootstrap_binance_rest(100).await;
                    if forwarded > 0 {
                        last_aggtrade_at = Instant::now();
                    }
                }
                frame = read.next() => {
                    // Connection rail: any frame proves transport is alive.
                    last_frame_at = Instant::now();
                    match frame {
                        Some(Ok(Message::Text(text))) => {
                            // Data rail is updated INSIDE handle_text only
                            // when an aggTrade was successfully parsed.
                            if self.handle_binance_text(&text).await? {
                                last_aggtrade_at = Instant::now();
                            }
                        }
                        Some(Ok(Message::Binary(_))) => {}
                        Some(Ok(Message::Ping(payload))) => {
                            write.send(Message::Pong(payload)).await.ok();
                        }
                        Some(Ok(Message::Pong(_))) => {}
                        Some(Ok(Message::Close(_))) => {
                            anyhow::bail!("spot websocket closed by remote");
                        }
                        Some(Ok(Message::Frame(_))) => {}
                        Some(Err(error)) => return Err(error).context("spot websocket frame error"),
                        None => anyhow::bail!("spot websocket stream ended"),
                    }
                }
            }
        }
    }

    async fn run_coinbase_once(&self, url: &str, shutdown: CancellationToken) -> Result<()> {
        let (stream, _) = connect_async(url)
            .await
            .with_context(|| format!("failed to connect coinbase spot websocket {url}"))?;
        info!(product_id = %self.coinbase_product_id, "coinbase spot websocket connected");

        let (mut write, mut read) = stream.split();
        let subscribe = json!({
            "type": "subscribe",
            "product_ids": [self.coinbase_product_id],
            "channel": "market_trades",
        });
        write
            .send(Message::Text(subscribe.to_string().into()))
            .await
            .context("failed to subscribe coinbase market trades")?;
        // Coinbase Advanced Trade docs: channels can close after 60-90s
        // without heartbeats. Keep fallback stream live by subscribing.
        let heartbeat_subscribe = json!({
            "type": "subscribe",
            "product_ids": [self.coinbase_product_id],
            "channel": "heartbeats",
        });
        write
            .send(Message::Text(heartbeat_subscribe.to_string().into()))
            .await
            .context("failed to subscribe coinbase heartbeats")?;

        let mut pings = interval(self.ping_interval);
        pings.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let now = Instant::now();
        let mut last_frame_at = now;
        let mut last_trade_at = now;

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    let _ = write.send(Message::Close(None)).await;
                    return Ok(());
                }
                _ = pings.tick() => {
                    let conn_elapsed = last_frame_at.elapsed();
                    if conn_elapsed > self.conn_stale_timeout {
                        anyhow::bail!(
                            "coinbase spot websocket connection stale: no frames for {}s (timeout={}s); forcing reconnect",
                            conn_elapsed.as_secs(),
                            self.conn_stale_timeout.as_secs()
                        );
                    }
                    let data_elapsed = last_trade_at.elapsed();
                    if data_elapsed > self.data_stale_timeout {
                        anyhow::bail!(
                            "coinbase spot websocket data stale: no market_trades for {}s (timeout={}s); forcing reconnect",
                            data_elapsed.as_secs(),
                            self.data_stale_timeout.as_secs()
                        );
                    }
                    write
                        .send(Message::Ping(Vec::new().into()))
                        .await
                        .context("failed to send coinbase websocket ping")?;
                }
                frame = read.next() => {
                    last_frame_at = Instant::now();
                    match frame {
                        Some(Ok(Message::Text(text))) => {
                            if self.handle_coinbase_text(&text).await? {
                                last_trade_at = Instant::now();
                            }
                        }
                        Some(Ok(Message::Binary(_))) => {}
                        Some(Ok(Message::Ping(payload))) => {
                            write.send(Message::Pong(payload)).await.ok();
                        }
                        Some(Ok(Message::Pong(_))) => {}
                        Some(Ok(Message::Close(_))) => {
                            anyhow::bail!("coinbase spot websocket closed by remote");
                        }
                        Some(Ok(Message::Frame(_))) => {}
                        Some(Err(error)) => return Err(error).context("coinbase spot websocket frame error"),
                        None => anyhow::bail!("coinbase spot websocket stream ended"),
                    }
                }
            }
        }
    }

    /// Returns `true` when a valid aggTrade was parsed and forwarded
    /// downstream — used by the run loop's data-rail liveness tracker.
    /// Returns `false` for non-aggTrade messages, wrong-symbol messages,
    /// or aggTrades with invalid price/quantity. The connection rail
    /// counts ANY received frame; the data rail counts ONLY this true
    /// return.
    async fn handle_binance_text(&self, text: &str) -> Result<bool> {
        let payload: Value =
            serde_json::from_str(text).context("failed to decode spot websocket payload")?;
        let payload = spot_trade_payload(&payload);
        let event_type = payload
            .get("e")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        if event_type != "aggtrade" {
            return Ok(false);
        }

        if let Some(event) = parse_binance_ws_trade(payload, &self.symbol) {
            self.forward_event(event);
            return Ok(true);
        }
        Ok(false)
    }

    async fn handle_coinbase_text(&self, text: &str) -> Result<bool> {
        let payload: Value =
            serde_json::from_str(text).context("failed to decode coinbase websocket payload")?;
        let mut parsed_any = false;
        let Some(events) = payload.get("events").and_then(Value::as_array) else {
            return Ok(false);
        };
        for event in events {
            let Some(trades) = event.get("trades").and_then(Value::as_array) else {
                continue;
            };
            for trade in trades {
                if let Some(parsed) = parse_coinbase_trade(trade, &self.coinbase_product_id) {
                    self.forward_event(parsed);
                    parsed_any = true;
                }
            }
        }
        Ok(parsed_any)
    }

    fn forward_event(&self, event: SpotTradeEvent) {
        let ingest_now_ms = now_unix_ms();
        if matches!(&event.source, SpotFeedSource::CoinbaseWs) {
            let last_binance = self.last_binance_trade_ingest_ms.load(Ordering::Relaxed);
            if last_binance > 0
                && ingest_now_ms.saturating_sub(last_binance)
                    <= self.data_stale_timeout.as_millis() as u64
            {
                return;
            }
        } else {
            self.last_binance_trade_ingest_ms
                .store(ingest_now_ms, Ordering::Relaxed);
        }
        self.metrics
            .observe_spot_trade(event.source.as_str(), event.observed_at_ms);
        if let Some(tx) = &self.event_tx {
            let _ = tx.send(event);
        }
    }
}

fn spot_trade_payload(payload: &Value) -> &Value {
    payload
        .get("data")
        .filter(|value| value.is_object())
        .unwrap_or(payload)
}

fn parse_binance_ws_trade(payload: &Value, expected_symbol: &str) -> Option<SpotTradeEvent> {
    let symbol = payload
        .get("s")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_uppercase();
    if !symbol.is_empty() && symbol != expected_symbol {
        return None;
    }
    let price = payload
        .get("p")
        .and_then(value_as_f64_opt)
        .filter(|value| value.is_finite() && *value > 0.0)?;
    let quantity = payload
        .get("q")
        .and_then(value_as_f64_opt)
        .filter(|value| value.is_finite() && *value > 0.0)?;
    let observed_at_ms = payload
        .get("T")
        .and_then(Value::as_u64)
        .or_else(|| payload.get("E").and_then(Value::as_u64))
        .unwrap_or_else(now_unix_ms);
    Some(SpotTradeEvent {
        symbol: if symbol.is_empty() {
            expected_symbol.to_string()
        } else {
            symbol
        },
        source: SpotFeedSource::BinanceWs,
        price,
        quantity,
        observed_at_ms,
    })
}

fn parse_binance_rest_trade(
    payload: &Value,
    symbol: &str,
    source: SpotFeedSource,
) -> Option<SpotTradeEvent> {
    let price = payload
        .get("p")
        .and_then(value_as_f64_opt)
        .filter(|value| value.is_finite() && *value > 0.0)?;
    let quantity = payload
        .get("q")
        .and_then(value_as_f64_opt)
        .filter(|value| value.is_finite() && *value > 0.0)?;
    let observed_at_ms = payload
        .get("T")
        .and_then(Value::as_u64)
        .unwrap_or_else(now_unix_ms);
    Some(SpotTradeEvent {
        symbol: symbol.to_string(),
        source,
        price,
        quantity,
        observed_at_ms,
    })
}

fn parse_coinbase_trade(payload: &Value, product_id: &str) -> Option<SpotTradeEvent> {
    let event_product = payload.get("product_id").and_then(Value::as_str)?;
    if event_product != product_id {
        return None;
    }
    let price = payload
        .get("price")
        .and_then(value_as_f64_opt)
        .filter(|value| value.is_finite() && *value > 0.0)?;
    let quantity = payload
        .get("size")
        .and_then(value_as_f64_opt)
        .filter(|value| value.is_finite() && *value > 0.0)?;
    let observed_at_ms = payload
        .get("time")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339_ms)
        .unwrap_or_else(now_unix_ms);
    Some(SpotTradeEvent {
        symbol: "BTCUSDT".to_string(),
        source: SpotFeedSource::CoinbaseWs,
        price,
        quantity,
        observed_at_ms,
    })
}

fn coinbase_product_for_symbol(symbol: &str) -> String {
    match symbol.to_ascii_uppercase().as_str() {
        "BTCUSDT" | "BTCUSD" => "BTC-USD".to_string(),
        other => other.to_string(),
    }
}

fn parse_rfc3339_ms(raw: &str) -> Option<u64> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .and_then(|dt| u64::try_from(dt.timestamp_millis()).ok())
}

fn value_as_f64_opt(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(raw) => raw.parse().ok(),
        _ => None,
    }
}

fn now_unix_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn client(tx: mpsc::UnboundedSender<SpotTradeEvent>) -> SpotWsClient {
        SpotWsClient::new(
            "wss://example.invalid/ws/btcusdt@aggTrade".to_string(),
            "BTCUSDT".to_string(),
            Duration::from_secs(10),
            Arc::new(AppMetrics::new().unwrap()),
            Some(tx),
        )
    }

    #[tokio::test]
    async fn raw_aggtrade_payload_forwards_spot_trade() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let client = client(tx);

        let parsed = client
            .handle_binance_text(
                r#"{"e":"aggTrade","s":"BTCUSDT","p":"63420.50","q":"0.012","T":1714431008123}"#,
            )
            .await
            .unwrap();

        assert!(parsed);
        let event = rx.recv().await.unwrap();
        assert_eq!(event.symbol, "BTCUSDT");
        assert_eq!(event.source.as_str(), "binance_ws");
        assert_eq!(event.price, 63420.50);
        assert_eq!(event.quantity, 0.012);
        assert_eq!(event.observed_at_ms, 1714431008123);
    }

    #[tokio::test]
    async fn combined_stream_aggtrade_payload_forwards_spot_trade() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let client = client(tx);

        let parsed = client
            .handle_binance_text(
                r#"{"stream":"btcusdt@aggTrade","data":{"e":"aggTrade","s":"BTCUSDT","p":"63421.25","q":"0.020","T":1714431009000}}"#,
            )
            .await
            .unwrap();

        assert!(parsed);
        let event = rx.recv().await.unwrap();
        assert_eq!(event.symbol, "BTCUSDT");
        assert_eq!(event.source.as_str(), "binance_ws");
        assert_eq!(event.price, 63421.25);
        assert_eq!(event.quantity, 0.020);
        assert_eq!(event.observed_at_ms, 1714431009000);
    }

    #[tokio::test]
    async fn non_aggtrade_payload_does_not_refresh_data_rail() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let client = client(tx);

        let parsed = client
            .handle_binance_text(r#"{"result":null,"id":1}"#)
            .await
            .unwrap();

        assert!(!parsed);
        assert!(rx.try_recv().is_err());
    }
}
