//! Perp trade websocket client feeding BTC price-level blend + basis momentum.

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::{interval, sleep, MissedTickBehavior};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::metrics::AppMetrics;

/// Two-rail liveness model for the perp websocket.
///
/// Mirrors the spot client: the venue can keep the transport alive (control
/// frames, pongs) while silently dropping the aggTrade subscription. We need
/// both:
///
/// 1. **Connection rail**: any inbound frame proves the socket is alive.
///    Resets on text/binary/ping/pong/close. Failure → bail and reconnect.
/// 2. **Data rail**: only PARSED aggTrade messages reset this. Failure
///    means the BTC perp tape isn't flowing — the price-level blend and
///    basis momentum are going stale even if the connection appears
///    healthy. Failure → bail and reconnect.
const DEFAULT_CONN_STALE_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_DATA_STALE_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_BINANCE_REST_BOOTSTRAP_URL: &str = "https://fapi.binance.com/fapi/v1/aggTrades";
const BINANCE_REST_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5);
const MAX_DIRECT_FEED_CLOCK_SKEW_MS: u64 = 10_000;

#[derive(Debug, Clone)]
pub struct PerpTradeEvent {
    pub symbol: String,
    pub price: f64,
    pub quantity: f64,
    pub observed_at_ms: u64,
}

#[derive(Clone)]
pub struct PerpWsClient {
    url: String,
    binance_rest_bootstrap_url: Option<String>,
    symbol: String,
    ping_interval: Duration,
    conn_stale_timeout: Duration,
    data_stale_timeout: Duration,
    metrics: Arc<AppMetrics>,
    last_binance_rest_trade_id: Arc<AtomicU64>,
    event_tx: Option<mpsc::UnboundedSender<PerpTradeEvent>>,
}

impl PerpWsClient {
    pub fn new(
        url: String,
        symbol: String,
        ping_interval: Duration,
        metrics: Arc<AppMetrics>,
        event_tx: Option<mpsc::UnboundedSender<PerpTradeEvent>>,
    ) -> Self {
        Self::with_timeouts(
            url,
            symbol,
            ping_interval,
            DEFAULT_CONN_STALE_TIMEOUT,
            DEFAULT_DATA_STALE_TIMEOUT,
            Some(DEFAULT_BINANCE_REST_BOOTSTRAP_URL.to_string()),
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
        metrics: Arc<AppMetrics>,
        event_tx: Option<mpsc::UnboundedSender<PerpTradeEvent>>,
    ) -> Self {
        Self {
            url,
            binance_rest_bootstrap_url,
            symbol: symbol.to_ascii_uppercase(),
            ping_interval,
            conn_stale_timeout,
            data_stale_timeout,
            metrics,
            last_binance_rest_trade_id: Arc::new(AtomicU64::new(0)),
            event_tx,
        }
    }

    pub async fn run(self, shutdown: CancellationToken) {
        let mut backoff = Duration::from_secs(1);
        while !shutdown.is_cancelled() {
            let _ = self.bootstrap_binance_rest(1_000).await;
            match self.run_once(shutdown.clone()).await {
                Ok(()) => break,
                Err(error) if shutdown.is_cancelled() => {
                    debug!(error = ?error, "perp websocket shutdown");
                    break;
                }
                Err(error) => {
                    warn!(
                        error = ?error,
                        backoff_ms = backoff.as_millis(),
                        "perp websocket loop failed; reconnecting"
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
                        let ingest_now_ms = now_unix_ms();
                        let last_seen_id = self.last_binance_rest_trade_id.load(Ordering::Relaxed);
                        let (events, max_forwarded_id) = normalize_binance_rest_events(
                            &rows,
                            &self.symbol,
                            ingest_now_ms,
                            last_seen_id,
                        );
                        let mut forwarded = 0usize;
                        for event in events {
                            self.forward_event(event, true);
                            forwarded += 1;
                        }
                        if max_forwarded_id > last_seen_id {
                            self.last_binance_rest_trade_id
                                .store(max_forwarded_id, Ordering::Relaxed);
                        }
                        if forwarded > 0 {
                            info!(
                                symbol = %self.symbol,
                                forwarded,
                                "bootstrapped BTC perp samples from Binance REST"
                            );
                        }
                        forwarded
                    }
                    Ok(_) => {
                        warn!("Binance perp REST bootstrap returned non-array payload");
                        0
                    }
                    Err(error) => {
                        warn!(error = ?error, "failed to decode Binance perp REST bootstrap");
                        0
                    }
                },
                Err(error) => {
                    warn!(error = ?error, "Binance perp REST bootstrap returned error status");
                    0
                }
            },
            Err(error) => {
                warn!(error = ?error, "Binance perp REST bootstrap request failed");
                0
            }
        }
    }

    async fn run_once(&self, shutdown: CancellationToken) -> Result<()> {
        let (stream, _) = connect_async(self.url.as_str())
            .await
            .with_context(|| format!("failed to connect perp websocket {}", self.url))?;
        info!(symbol = %self.symbol, "perp websocket connected");

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
                            "perp websocket connection stale: no frames for {}s (timeout={}s); forcing reconnect",
                            conn_elapsed.as_secs(),
                            self.conn_stale_timeout.as_secs()
                        );
                    }
                    // Data rail check: subscription dropped silently
                    // (transport alive but aggTrade stream stopped).
                    let data_elapsed = last_aggtrade_at.elapsed();
                    if data_elapsed > self.data_stale_timeout {
                        anyhow::bail!(
                            "perp websocket data stale: no aggTrade for {}s (timeout={}s); forcing reconnect",
                            data_elapsed.as_secs(),
                            self.data_stale_timeout.as_secs()
                        );
                    }
                    write
                        .send(Message::Ping(Vec::new().into()))
                        .await
                        .context("failed to send perp websocket ping")?;
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
                            anyhow::bail!("perp websocket closed by remote");
                        }
                        Some(Ok(Message::Frame(_))) => {}
                        Some(Err(error)) => return Err(error).context("perp websocket frame error"),
                        None => anyhow::bail!("perp websocket stream ended"),
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
            serde_json::from_str(text).context("failed to decode perp websocket payload")?;
        let payload = perp_trade_payload(&payload);
        let event_type = payload
            .get("e")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        if event_type != "aggtrade" {
            return Ok(false);
        }

        if let Some(event) = parse_binance_ws_trade(payload, &self.symbol) {
            self.forward_event(event, false);
            return Ok(true);
        }
        Ok(false)
    }

    fn forward_event(&self, mut event: PerpTradeEvent, is_bootstrap: bool) {
        let ingest_now_ms = now_unix_ms();
        if !is_bootstrap
            && event.observed_at_ms.abs_diff(ingest_now_ms) > MAX_DIRECT_FEED_CLOCK_SKEW_MS
        {
            event.observed_at_ms = ingest_now_ms;
        }
        self.metrics
            .observe_spot_trade("binance_perp_ws", event.observed_at_ms);
        if let Some(tx) = &self.event_tx {
            let _ = tx.send(event);
        }
    }
}

fn perp_trade_payload(payload: &Value) -> &Value {
    payload
        .get("data")
        .filter(|value| value.is_object())
        .unwrap_or(payload)
}

fn parse_binance_ws_trade(payload: &Value, expected_symbol: &str) -> Option<PerpTradeEvent> {
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
    Some(PerpTradeEvent {
        symbol: if symbol.is_empty() {
            expected_symbol.to_string()
        } else {
            symbol
        },
        price,
        quantity,
        observed_at_ms,
    })
}

fn parse_binance_rest_trade(payload: &Value, symbol: &str) -> Option<(PerpTradeEvent, Option<u64>)> {
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
    let source_trade_id = payload.get("a").and_then(value_as_u64_opt);
    Some((
        PerpTradeEvent {
            symbol: symbol.to_string(),
            price,
            quantity,
            observed_at_ms,
        },
        source_trade_id,
    ))
}

fn normalize_binance_rest_events(
    rows: &[Value],
    symbol: &str,
    ingest_now_ms: u64,
    last_seen_id: u64,
) -> (Vec<PerpTradeEvent>, u64) {
    let mut events = rows
        .iter()
        .filter_map(|row| parse_binance_rest_trade(row, symbol))
        .filter(|(_, trade_id)| trade_id.map_or(true, |id| id > last_seen_id))
        .collect::<Vec<_>>();
    let max_event_ms = events
        .iter()
        .map(|(event, _)| event.observed_at_ms)
        .max()
        .unwrap_or(ingest_now_ms);
    let max_forwarded_id = events
        .iter()
        .filter_map(|(_, trade_id)| *trade_id)
        .max()
        .unwrap_or(last_seen_id)
        .max(last_seen_id);
    events.sort_by_key(|(event, _)| event.observed_at_ms);
    let events = events
        .into_iter()
        .map(|(mut event, _)| {
            let event_lag_ms = max_event_ms.saturating_sub(event.observed_at_ms);
            event.observed_at_ms = ingest_now_ms.saturating_sub(event_lag_ms);
            event
        })
        .collect();
    (events, max_forwarded_id)
}

fn value_as_f64_opt(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(raw) => raw.parse().ok(),
        _ => None,
    }
}

fn value_as_u64_opt(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number.as_u64(),
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

    fn client(tx: mpsc::UnboundedSender<PerpTradeEvent>) -> PerpWsClient {
        PerpWsClient::new(
            "wss://example.invalid/ws/btcusdt@aggTrade".to_string(),
            "BTCUSDT".to_string(),
            Duration::from_secs(10),
            Arc::new(AppMetrics::new().unwrap()),
            Some(tx),
        )
    }

    #[tokio::test]
    async fn raw_aggtrade_payload_forwards_perp_trade() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let client = client(tx);
        let payload: Value = serde_json::from_str(
            r#"{"e":"aggTrade","s":"BTCUSDT","a":"12345","p":"63420.50","q":"0.012","T":1714431008123}"#,
        )
        .unwrap();
        assert_eq!(
            parse_binance_ws_trade(&payload, "BTCUSDT")
                .unwrap()
                .observed_at_ms,
            1714431008123
        );
        let started_ms = now_unix_ms();

        let parsed = client
            .handle_binance_text(
                r#"{"e":"aggTrade","s":"BTCUSDT","a":"12345","p":"63420.50","q":"0.012","T":1714431008123}"#,
            )
            .await
            .unwrap();

        assert!(parsed);
        let event = rx.recv().await.unwrap();
        assert_eq!(event.symbol, "BTCUSDT");
        assert_eq!(event.price, 63420.50);
        assert_eq!(event.quantity, 0.012);
        assert!(event.observed_at_ms >= started_ms);
        assert!(event.observed_at_ms <= now_unix_ms());
    }

    #[tokio::test]
    async fn combined_stream_aggtrade_payload_forwards_perp_trade() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let client = client(tx);
        let started_ms = now_unix_ms();

        let parsed = client
            .handle_binance_text(
                r#"{"stream":"btcusdt@aggTrade","data":{"e":"aggTrade","s":"BTCUSDT","a":12346,"p":"63421.25","q":"0.020","T":1714431009000}}"#,
            )
            .await
            .unwrap();

        assert!(parsed);
        let event = rx.recv().await.unwrap();
        assert_eq!(event.symbol, "BTCUSDT");
        assert_eq!(event.price, 63421.25);
        assert_eq!(event.quantity, 0.020);
        assert!(event.observed_at_ms >= started_ms);
        assert!(event.observed_at_ms <= now_unix_ms());
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

    #[test]
    fn binance_rest_events_are_deduped_and_shifted_to_ingest_timeline() {
        let rows = vec![
            serde_json::json!({"a": "10", "p": "63400.00", "q": "0.010", "T": 1_714_431_000_000u64}),
            serde_json::json!({"a": "11", "p": "63401.00", "q": "0.020", "T": 1_714_431_001_000u64}),
            serde_json::json!({"a": "12", "p": "63402.00", "q": "0.030", "T": 1_714_431_003_000u64}),
        ];

        let (events, max_id) =
            normalize_binance_rest_events(&rows, "BTCUSDT", 2_000_000_000_000, 10);

        assert_eq!(max_id, 12);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].observed_at_ms, 1_999_999_998_000);
        assert_eq!(events[1].observed_at_ms, 2_000_000_000_000);
    }
}
