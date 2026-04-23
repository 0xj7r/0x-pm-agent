use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::time::{interval, sleep, MissedTickBehavior};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::book::{BookStore, Level};
use crate::metrics::{AppMetrics, StreamKind};

pub struct MarketWsClient {
    url: String,
    assets: Vec<String>,
    ping_interval: Duration,
    books: Arc<BookStore>,
    metrics: Arc<AppMetrics>,
}

impl MarketWsClient {
    pub fn new(
        url: String,
        assets: Vec<String>,
        ping_interval: Duration,
        books: Arc<BookStore>,
        metrics: Arc<AppMetrics>,
    ) -> Self {
        Self {
            url,
            assets,
            ping_interval,
            books,
            metrics,
        }
    }

    pub async fn run(self, shutdown: CancellationToken) {
        let mut backoff = Duration::from_secs(1);
        while !shutdown.is_cancelled() {
            match self.run_once(shutdown.clone()).await {
                Ok(()) => break,
                Err(error) if shutdown.is_cancelled() => {
                    debug!(error = ?error, "market websocket shutdown");
                    break;
                }
                Err(error) => {
                    self.metrics.set_stream_connected(StreamKind::Market, false);
                    self.metrics.inc_reconnect(StreamKind::Market);
                    warn!(
                        error = ?error,
                        backoff_ms = backoff.as_millis(),
                        "market websocket loop failed; reconnecting"
                    );
                    sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                }
            }
        }
        self.metrics.set_stream_connected(StreamKind::Market, false);
    }

    async fn run_once(&self, shutdown: CancellationToken) -> Result<()> {
        let (stream, _) = connect_async(self.url.as_str())
            .await
            .with_context(|| format!("failed to connect market websocket {}", self.url))?;
        info!(asset_count = self.assets.len(), "market websocket connected");
        self.metrics.set_stream_connected(StreamKind::Market, true);

        let (mut write, mut read) = stream.split();
        let subscribe = json!({
            "assets_ids": self.assets,
            "type": "market",
            "initial_dump": true,
            "level": 2,
            "custom_feature_enabled": true,
        });
        write
            .send(Message::Text(subscribe.to_string().into()))
            .await
            .context("failed to subscribe market websocket")?;

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
                        .send(Message::Text("PING".to_string().into()))
                        .await
                        .context("failed to send market websocket ping")?;
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
                            anyhow::bail!("market websocket closed by remote");
                        }
                        Some(Ok(Message::Frame(_))) => {}
                        Some(Err(error)) => return Err(error).context("market websocket frame error"),
                        None => anyhow::bail!("market websocket stream ended"),
                    }
                }
            }
        }
    }

    async fn handle_text(&self, text: &str) -> Result<()> {
        if text == "PONG" {
            return Ok(());
        }

        let payload: Value = serde_json::from_str(text).context("failed to decode market websocket payload")?;
        match payload {
            Value::Array(items) => {
                for item in items {
                    self.handle_event(item).await?;
                }
            }
            Value::Object(_) => self.handle_event(payload).await?,
            _ => {}
        }
        Ok(())
    }

    async fn handle_event(&self, event: Value) -> Result<()> {
        let event_type = event
            .get("event_type")
            .and_then(Value::as_str)
            .unwrap_or_else(|| if event.get("bids").is_some() || event.get("asks").is_some() { "book" } else { "unknown" });

        match event_type {
            "book" => {
                let asset_id = value_as_str(&event, "asset_id");
                if asset_id.is_empty() {
                    return Ok(());
                }
                let bids = parse_levels(event.get("bids"));
                let asks = parse_levels(event.get("asks"));
                let state = self.books.apply_snapshot(&asset_id, &bids, &asks).await;
                self.metrics.observe_market_message("book");
                debug!(
                    asset_id = %asset_id,
                    best_bid = state.best_bid,
                    best_ask = state.best_ask,
                    spread = state.spread,
                    "market book update"
                );
            }
            "price_change" => {
                self.metrics.observe_market_message("price_change");
                let changes = event
                    .get("price_changes")
                    .or_else(|| event.get("pc"))
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                for change in changes {
                    let asset_id = change
                        .get("asset_id")
                        .or_else(|| change.get("a"))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if asset_id.is_empty() {
                        continue;
                    }
                    let best_bid = change
                        .get("best_bid")
                        .or_else(|| change.get("bb"))
                        .and_then(value_as_f64_opt);
                    let best_ask = change
                        .get("best_ask")
                        .or_else(|| change.get("ba"))
                        .and_then(value_as_f64_opt);
                    let observed_at_ms = change
                        .get("timestamp")
                        .or_else(|| change.get("t"))
                        .or_else(|| change.get("ts"))
                        .and_then(Value::as_u64)
                        .unwrap_or_else(now_unix_ms);
                    self.books
                        .apply_best_bid_ask(asset_id, best_bid, best_ask)
                        .await;
                    self.books.record_trade_event(asset_id, observed_at_ms).await;
                }
            }
            "best_bid_ask" => {
                let asset_id = value_as_str(&event, "asset_id");
                if asset_id.is_empty() {
                    return Ok(());
                }
                let best_bid = event.get("best_bid").and_then(value_as_f64_opt);
                let best_ask = event.get("best_ask").and_then(value_as_f64_opt);
                self.books
                    .apply_best_bid_ask(&asset_id, best_bid, best_ask)
                    .await;
                self.metrics.observe_market_message("best_bid_ask");
            }
            "last_trade_price" => {
                let asset_id = value_as_str(&event, "asset_id");
                if asset_id.is_empty() {
                    return Ok(());
                }
                if let Some(price) = event.get("price").and_then(value_as_f64_opt) {
                    self.books.apply_last_trade(&asset_id, price).await;
                }
                self.metrics.observe_market_message("last_trade_price");
            }
            other => {
                self.metrics.observe_market_message(other);
                debug!(event_type = other, payload = %event, "unhandled market websocket event");
            }
        }

        Ok(())
    }
}

fn parse_levels(value: Option<&Value>) -> Vec<Level> {
    value
        .and_then(Value::as_array)
        .map(|levels| {
            levels
                .iter()
                .filter_map(|level| {
                    let price = level.get("price").and_then(value_as_f64_opt)?;
                    let size = level
                        .get("size")
                        .or_else(|| level.get("s"))
                        .and_then(value_as_f64_opt)
                        .unwrap_or_default();
                    Some(Level { price, size })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn value_as_str(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
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
