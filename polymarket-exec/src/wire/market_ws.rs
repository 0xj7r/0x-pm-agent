//! Polymarket market websocket client that maintains live order-book state.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::watch;
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
    assets_rx: Option<watch::Receiver<Vec<String>>>,
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
            assets_rx: None,
        }
    }

    pub fn with_asset_updates(mut self, assets_rx: watch::Receiver<Vec<String>>) -> Self {
        self.assets_rx = Some(assets_rx);
        self
    }

    pub async fn run(self, shutdown: CancellationToken) {
        let mut backoff = Duration::from_secs(1);
        let mut assets = self.assets.clone();
        let mut assets_rx = self.assets_rx.clone();
        while !shutdown.is_cancelled() {
            if let Some(rx) = assets_rx.as_mut() {
                assets = rx.borrow_and_update().clone();
            }
            match self
                .run_once(shutdown.clone(), assets.clone(), assets_rx.clone())
                .await
            {
                Ok(()) => break,
                Err(error) if error.to_string().contains("asset subscription changed") => {
                    self.metrics.set_stream_connected(StreamKind::Market, false);
                    info!(
                        asset_count = assets.len(),
                        "market websocket asset universe changed; reconnecting"
                    );
                    backoff = Duration::from_secs(1);
                    continue;
                }
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

    async fn run_once(
        &self,
        shutdown: CancellationToken,
        assets: Vec<String>,
        mut assets_rx: Option<watch::Receiver<Vec<String>>>,
    ) -> Result<()> {
        let (stream, _) = connect_async(self.url.as_str())
            .await
            .with_context(|| format!("failed to connect market websocket {}", self.url))?;
        info!(asset_count = assets.len(), "market websocket connected");
        self.metrics.set_stream_connected(StreamKind::Market, true);

        let (mut write, mut read) = stream.split();
        let subscribe = json!({
            "assets_ids": assets,
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
                changed = async {
                    match assets_rx.as_mut() {
                        Some(rx) => rx.changed().await.map(|_| ()),
                        None => std::future::pending().await,
                    }
                } => {
                    changed.context("market websocket asset update channel closed")?;
                    anyhow::bail!("asset subscription changed");
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

        let payload: Value =
            serde_json::from_str(text).context("failed to decode market websocket payload")?;
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
            .unwrap_or_else(|| {
                if event.get("bids").is_some() || event.get("asks").is_some() {
                    "book"
                } else {
                    "unknown"
                }
            });

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
                    self.books
                        .record_trade_event(asset_id, observed_at_ms)
                        .await;
                    // Shadow-subsystem hook: in shadow_live mode, feed
                    // the depth-delta to TradeSynthesiser so size can be
                    // recovered when last_trade_price arrives. We treat
                    // touch-move as a depth signal at the new touch
                    // price; size is approximate but the synthesiser's
                    // 100ms correlation window matches that imprecision.
                    if let Some(handle) =
                        crate::runtime::shadow_subsystem::shadow()
                    {
                        if let Ok(mut shadow) = handle.lock() {
                            if let Some(bid) = best_bid {
                                shadow.on_book_depth_delta(asset_id, bid, 1.0, observed_at_ms);
                            }
                            if let Some(ask) = best_ask {
                                shadow.on_book_depth_delta(asset_id, ask, 1.0, observed_at_ms);
                            }
                        }
                    }
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
                let price_opt = event.get("price").and_then(value_as_f64_opt);
                let observed_at_ms = event
                    .get("timestamp")
                    .or_else(|| event.get("t"))
                    .or_else(|| event.get("ts"))
                    .and_then(Value::as_u64)
                    .unwrap_or_else(now_unix_ms);
                let mut shadow_inputs: Option<(f64, Option<f64>, Option<f64>)> = None;
                if let Some(price) = price_opt {
                    let book_state = self.books.apply_last_trade(&asset_id, price).await;
                    let best_bid = (book_state.best_bid > 0.0).then_some(book_state.best_bid);
                    let best_ask = (book_state.best_ask > 0.0).then_some(book_state.best_ask);
                    shadow_inputs = Some((price, best_bid, best_ask));
                }
                self.metrics.observe_market_message("last_trade_price");
                if let (Some((price, best_bid, best_ask)), Some(handle)) =
                    (shadow_inputs, crate::runtime::shadow_subsystem::shadow())
                {
                    if let Ok(mut shadow) = handle.lock() {
                        shadow.on_last_trade_price(
                            &asset_id,
                            price,
                            best_bid,
                            best_ask,
                            observed_at_ms,
                        );
                    }
                }
            }
            other => {
                self.metrics.observe_market_message(other);
                debug!(event_type = other, payload = %event, "unhandled market websocket event");
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_update_receiver_is_marked_seen_before_reconnect() {
        let (_tx, mut rx) = watch::channel(vec!["old".to_string()]);
        let _ = _tx.send(vec!["new".to_string()]);

        let assets = rx.borrow_and_update().clone();
        assert_eq!(assets, vec!["new".to_string()]);
        assert!(!rx.has_changed().expect("sender open"));
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
