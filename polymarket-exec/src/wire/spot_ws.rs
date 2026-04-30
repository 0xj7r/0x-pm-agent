//! Spot trade websocket client feeding BTC regime telemetry into the runtime.

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

#[derive(Debug, Clone)]
pub struct SpotTradeEvent {
    pub symbol: String,
    pub price: f64,
    pub quantity: f64,
    pub observed_at_ms: u64,
}

pub struct SpotWsClient {
    url: String,
    symbol: String,
    ping_interval: Duration,
    conn_stale_timeout: Duration,
    data_stale_timeout: Duration,
    metrics: std::sync::Arc<AppMetrics>,
    event_tx: Option<mpsc::UnboundedSender<SpotTradeEvent>>,
}

impl SpotWsClient {
    pub fn new(
        url: String,
        symbol: String,
        ping_interval: Duration,
        metrics: std::sync::Arc<AppMetrics>,
        event_tx: Option<mpsc::UnboundedSender<SpotTradeEvent>>,
    ) -> Self {
        Self::with_timeouts(
            url,
            symbol,
            ping_interval,
            DEFAULT_CONN_STALE_TIMEOUT,
            DEFAULT_DATA_STALE_TIMEOUT,
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
        metrics: std::sync::Arc<AppMetrics>,
        event_tx: Option<mpsc::UnboundedSender<SpotTradeEvent>>,
    ) -> Self {
        Self {
            url,
            symbol: symbol.to_ascii_uppercase(),
            ping_interval,
            conn_stale_timeout,
            data_stale_timeout,
            metrics,
            event_tx,
        }
    }

    pub async fn run(self, shutdown: CancellationToken) {
        let mut backoff = Duration::from_secs(1);
        while !shutdown.is_cancelled() {
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

    async fn run_once(&self, shutdown: CancellationToken) -> Result<()> {
        let (stream, _) = connect_async(self.url.as_str())
            .await
            .with_context(|| format!("failed to connect spot websocket {}", self.url))?;
        info!(symbol = %self.symbol, "spot websocket connected");

        let (mut write, mut read) = stream.split();
        let mut pings = interval(self.ping_interval);
        pings.set_missed_tick_behavior(MissedTickBehavior::Delay);

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
                frame = read.next() => {
                    // Connection rail: any frame proves transport is alive.
                    last_frame_at = Instant::now();
                    match frame {
                        Some(Ok(Message::Text(text))) => {
                            // Data rail is updated INSIDE handle_text only
                            // when an aggTrade was successfully parsed.
                            if self.handle_text(&text).await? {
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

    /// Returns `true` when a valid aggTrade was parsed and forwarded
    /// downstream — used by the run loop's data-rail liveness tracker.
    /// Returns `false` for non-aggTrade messages, wrong-symbol messages,
    /// or aggTrades with invalid price/quantity. The connection rail
    /// counts ANY received frame; the data rail counts ONLY this true
    /// return.
    async fn handle_text(&self, text: &str) -> Result<bool> {
        let payload: Value =
            serde_json::from_str(text).context("failed to decode spot websocket payload")?;
        let event_type = payload
            .get("e")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        if event_type != "aggtrade" {
            return Ok(false);
        }

        let symbol = payload
            .get("s")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_uppercase();
        if !symbol.is_empty() && symbol != self.symbol {
            return Ok(false);
        }

        let price = payload
            .get("p")
            .and_then(value_as_f64_opt)
            .filter(|value| value.is_finite() && *value > 0.0);
        let quantity = payload
            .get("q")
            .and_then(value_as_f64_opt)
            .filter(|value| value.is_finite() && *value > 0.0);
        let observed_at_ms = payload
            .get("T")
            .and_then(Value::as_u64)
            .or_else(|| payload.get("E").and_then(Value::as_u64))
            .unwrap_or_else(now_unix_ms);

        let (Some(price), Some(quantity)) = (price, quantity) else {
            return Ok(false);
        };

        self.metrics.observe_market_message("spot_trade");
        if let Some(tx) = &self.event_tx {
            let _ = tx.send(SpotTradeEvent {
                symbol: if symbol.is_empty() {
                    self.symbol.clone()
                } else {
                    symbol
                },
                price,
                quantity,
                observed_at_ms,
            });
        }
        Ok(true)
    }
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
