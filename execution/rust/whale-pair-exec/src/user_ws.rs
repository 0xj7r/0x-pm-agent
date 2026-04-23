use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::{interval, sleep, MissedTickBehavior};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::config::UserWsAuth;
use crate::types::CloseMethod;
use crate::metrics::{AppMetrics, StreamKind};

#[derive(Debug, Clone)]
pub enum UserOrderEvent {
    OrderOpened {
        client_order_id: String,
        observed_at_ms: u64,
    },
    OrderRejected {
        client_order_id: Option<String>,
        reason: Option<String>,
        observed_at_ms: u64,
    },
    OrderCancelled {
        client_order_id: Option<String>,
        reason: Option<String>,
        observed_at_ms: u64,
    },
    OrderFilled {
        order_id: Option<String>,
        client_order_id: Option<String>,
        close_method: Option<CloseMethod>,
        market_id: Option<String>,
        asset_id: Option<String>,
        side: String,
        price: f64,
        quantity: f64,
        liquidity: Option<String>,
        observed_at_ms: u64,
    },
    OrderMerged {
        order_id: Option<String>,
        client_order_id: Option<String>,
        market_id: Option<String>,
        asset_id: Option<String>,
        price: f64,
        quantity: f64,
        observed_at_ms: u64,
    },
    OrderRedeemed {
        order_id: Option<String>,
        client_order_id: Option<String>,
        market_id: Option<String>,
        asset_id: Option<String>,
        price: f64,
        quantity: f64,
        observed_at_ms: u64,
    },
}

pub struct UserWsClient {
    url: String,
    auth: UserWsAuth,
    markets: Vec<String>,
    ping_interval: Duration,
    metrics: Arc<AppMetrics>,
    event_tx: Option<mpsc::UnboundedSender<UserOrderEvent>>,
}

impl UserWsClient {
    pub fn new(
        url: String,
        auth: UserWsAuth,
        markets: Vec<String>,
        ping_interval: Duration,
        metrics: Arc<AppMetrics>,
        event_tx: Option<mpsc::UnboundedSender<UserOrderEvent>>,
    ) -> Self {
        Self {
            url,
            auth,
            markets,
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
                    debug!(error = ?error, "user websocket shutdown");
                    break;
                }
                Err(error) => {
                    self.metrics.set_stream_connected(StreamKind::User, false);
                    self.metrics.inc_reconnect(StreamKind::User);
                    warn!(
                        error = ?error,
                        backoff_ms = backoff.as_millis(),
                        "user websocket loop failed; reconnecting"
                    );
                    sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                }
            }
        }
        self.metrics.set_stream_connected(StreamKind::User, false);
    }

    async fn run_once(&self, shutdown: CancellationToken) -> Result<()> {
        let (stream, _) = connect_async(self.url.as_str())
            .await
            .with_context(|| format!("failed to connect user websocket {}", self.url))?;
        info!(market_count = self.markets.len(), "user websocket connected");
        self.metrics.set_stream_connected(StreamKind::User, true);

        let (mut write, mut read) = stream.split();
        let subscribe = build_subscribe_payload(&self.auth, &self.markets);
        write
            .send(Message::Text(subscribe.to_string().into()))
            .await
            .context("failed to subscribe user websocket")?;

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
                        .context("failed to send user websocket ping")?;
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
                            anyhow::bail!("user websocket closed by remote");
                        }
                        Some(Ok(Message::Frame(_))) => {}
                        Some(Err(error)) => return Err(error).context("user websocket frame error"),
                        None => anyhow::bail!("user websocket stream ended"),
                    }
                }
            }
        }
    }

    async fn handle_text(&self, text: &str) -> Result<()> {
        if text == "PONG" {
            return Ok(());
        }

        let payload: Value = serde_json::from_str(text).context("failed to decode user websocket payload")?;
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
            .unwrap_or("unknown");
        let status = event.get("status").and_then(Value::as_str).unwrap_or("unknown");
        self.metrics.observe_user_message(event_type, status);

        if let Some(tx) = &self.event_tx {
            if let Some(event) = classify_user_event(&event) {
                let _ = tx.send(event);
            }
        }

        debug!(
            event_type,
            status,
            order_id = event.get("id").and_then(|value| value.as_str()),
            taker_order_id = event
                .get("taker_order_id")
                .and_then(|value| value.as_str()),
            market = event.get("market").and_then(|value| value.as_str()),
            asset_id = event.get("asset_id").and_then(|value| value.as_str()),
            side = event.get("side").and_then(|value| value.as_str()),
            price = event.get("price").and_then(value_as_f64_opt),
            size = event
                .get("size")
                .or_else(|| event.get("matched_amount"))
                .and_then(value_as_f64_opt),
            "user websocket event"
        );
        Ok(())
    }
}

fn classify_user_event(event: &Value) -> Option<UserOrderEvent> {
    let observed_at_ms = event
        .get("timestamp")
        .and_then(value_as_u64_opt)
        .or_else(|| event.get("ts").and_then(value_as_u64_opt))
        .unwrap_or_else(now_millis_fallback);

    let event_type = event
        .get("event_type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let status = event
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();

    let market_id = event
        .get("market")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let asset_id = event
        .get("asset_id")
        .and_then(Value::as_str)
        .or_else(|| event.get("assetId").and_then(Value::as_str))
        .map(ToOwned::to_owned);

    let order_id = event.get("id").and_then(Value::as_str).map(ToOwned::to_owned);
    let client_order_id = event
        .get("client_order_id")
        .and_then(Value::as_str)
        .or_else(|| event.get("taker_order_id").and_then(Value::as_str))
        .map(ToOwned::to_owned);

    let side = event
        .get("side")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();

    let price = event
        .get("price")
        .and_then(value_as_f64_opt)
        .or_else(|| event.get("match_price").and_then(value_as_f64_opt))
        .unwrap_or(0.0);
    let qty = event
        .get("size")
        .or_else(|| event.get("matched_amount"))
        .and_then(value_as_f64_opt)
        .unwrap_or(0.0);
    let liquidity = event
        .get("liquidity")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| event.get("maker_or_taker").and_then(Value::as_str).map(ToOwned::to_owned));

    let reason = event
        .get("reason")
        .and_then(Value::as_str)
        .or_else(|| event.get("cancel_reason").and_then(Value::as_str))
        .map(ToOwned::to_owned);

    let close_method = parse_str(
        event,
        &["close_method", "activity", "method", "tx_type", "event", "type"],
    )
    .map(CloseMethod::from_raw)
    .filter(|method| *method != CloseMethod::Unknown);

    let status_low = status.to_ascii_lowercase();
    let event_type_low = event_type.to_ascii_lowercase();
    let is_merge_signal = status_low.contains("merged")
        || event_type_low.contains("merge")
        || matches!(close_method, Some(CloseMethod::Merge | CloseMethod::Settle | CloseMethod::Settlement));
    let is_redeem_signal = status_low.contains("redeemed")
        || event_type_low.contains("redeem")
        || matches!(close_method, Some(CloseMethod::Redeem));
    let is_fill_signal = event_type_low == "fill"
        || event_type_low == "trade"
        || event_type_low == "trades"
        || status_low.contains("filled")
        || (status_low.contains("match") && qty > 0.0)
        || matches!(status_low.as_str(), "closed" | "done")
        || (matches!(event_type_low.as_str(), "order" | "order_update" | "orderbook")
            && qty > 0.0
            && status_low.contains("fill"));

    if is_merge_signal
    {
        if qty > 0.0 {
            return Some(UserOrderEvent::OrderMerged {
                order_id,
                client_order_id,
                market_id,
                asset_id,
                price,
                quantity: qty,
                observed_at_ms,
            });
        }
    }

    if is_redeem_signal
    {
        if qty > 0.0 {
            return Some(UserOrderEvent::OrderRedeemed {
                order_id,
                client_order_id,
                market_id,
                asset_id,
                price,
                quantity: qty,
                observed_at_ms,
            });
        }
    }

    if is_fill_signal
    {
        if qty > 0.0 {
            return Some(UserOrderEvent::OrderFilled {
                order_id,
                client_order_id,
                close_method,
                market_id,
                asset_id,
                side,
                price,
                quantity: qty,
                liquidity,
                observed_at_ms,
            });
        }
    }

    if status_low.contains("rejected") {
        return Some(UserOrderEvent::OrderRejected {
            client_order_id,
            reason,
            observed_at_ms,
        });
    }

    if status_low.contains("cancel") || event_type_low == "cancel" {
        return Some(UserOrderEvent::OrderCancelled {
            client_order_id,
            reason,
            observed_at_ms,
        });
    }

    if status_low == "open"
        || status_low == "opened"
        || status_low == "active"
        || (event_type_low == "order" && status_low.contains("open"))
    {
        if let Some(client_order_id) = client_order_id {
            return Some(UserOrderEvent::OrderOpened {
                client_order_id,
                observed_at_ms,
            });
        }
    }

    if matches!(event_type_low.as_str(), "order" | "orderbook" | "order_update")
        && qty > 0.0
        && !client_order_id.is_none()
        && (status_low.contains("closed") || status_low.contains("done") || status_low.contains("fill"))
    {
        return Some(UserOrderEvent::OrderFilled {
            order_id,
            client_order_id,
            market_id,
            asset_id,
            side,
            price,
            quantity: qty,
            liquidity,
            close_method: None,
            observed_at_ms,
        });
    }

    None
}

fn parse_str<'a>(event: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| event.get(key))
        .and_then(Value::as_str)
}

fn build_subscribe_payload(auth: &UserWsAuth, markets: &[String]) -> Value {
    if markets.is_empty() {
        json!({
            "auth": {
                "apiKey": auth.api_key,
                "secret": auth.api_secret,
                "passphrase": auth.api_passphrase,
            },
            "type": "user",
        })
    } else {
        json!({
            "auth": {
                "apiKey": auth.api_key,
                "secret": auth.api_secret,
                "passphrase": auth.api_passphrase,
            },
            "type": "user",
            "markets": markets,
        })
    }
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

fn now_millis_fallback() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
