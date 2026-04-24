//! Spot trade websocket client feeding BTC regime telemetry into the runtime.

use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::{interval, sleep, MissedTickBehavior};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::metrics::AppMetrics;

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
        Self {
            url,
            symbol: symbol.to_ascii_uppercase(),
            ping_interval,
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

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    let _ = write.send(Message::Close(None)).await;
                    return Ok(());
                }
                _ = pings.tick() => {
                    write
                        .send(Message::Ping(Vec::new().into()))
                        .await
                        .context("failed to send spot websocket ping")?;
                }
                frame = read.next() => {
                    match frame {
                        Some(Ok(Message::Text(text))) => self.handle_text(&text).await?,
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

    async fn handle_text(&self, text: &str) -> Result<()> {
        let payload: Value =
            serde_json::from_str(text).context("failed to decode spot websocket payload")?;
        let event_type = payload
            .get("e")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        if event_type != "aggtrade" {
            return Ok(());
        }

        let symbol = payload
            .get("s")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_uppercase();
        if !symbol.is_empty() && symbol != self.symbol {
            return Ok(());
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
            return Ok(());
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
        Ok(())
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
