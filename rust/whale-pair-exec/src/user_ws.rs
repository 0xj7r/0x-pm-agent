use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::time::{interval, sleep, MissedTickBehavior};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::config::UserWsAuth;
use crate::metrics::{AppMetrics, StreamKind};

pub struct UserWsClient {
    url: String,
    auth: UserWsAuth,
    markets: Vec<String>,
    ping_interval: Duration,
    metrics: Arc<AppMetrics>,
}

impl UserWsClient {
    pub fn new(
        url: String,
        auth: UserWsAuth,
        markets: Vec<String>,
        ping_interval: Duration,
        metrics: Arc<AppMetrics>,
    ) -> Self {
        Self {
            url,
            auth,
            markets,
            ping_interval,
            metrics,
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

        debug!(
            event_type,
            status,
            order_id = event.get("id").and_then(Value::as_str),
            taker_order_id = event.get("taker_order_id").and_then(Value::as_str),
            market = event.get("market").and_then(Value::as_str),
            asset_id = event.get("asset_id").and_then(Value::as_str),
            side = event.get("side").and_then(Value::as_str),
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
