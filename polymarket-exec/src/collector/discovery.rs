//! Active-market discovery via the Polymarket Data API.
//!
//! Periodically polls the data API, extracts the active 5-minute BTC
//! windows, and broadcasts the asset-id universe to whoever subscribes
//! via the `tokio::sync::watch` channel. Also emits a `market_meta`
//! `Event` whenever a market is seen for the first time or its strike
//! changes.

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::sync::{mpsc, watch};
use tokio::time::{interval, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use super::schema::{Event, EventType, Source};

/// Default poll cadence for the data-API discovery loop.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq)]
pub struct MarketSnapshot {
    pub slug: String,
    pub market_type: String,
    pub asset_ids: Vec<String>,
    pub strike: Option<f64>,
    pub end_time_ms: Option<i64>,
}

/// Owned config for the discovery loop.
pub struct DiscoveryConfig {
    pub data_api_url: String,
    pub poll_interval: Duration,
    pub http_client: reqwest::Client,
}

impl DiscoveryConfig {
    pub fn new(data_api_url: String) -> Self {
        Self {
            data_api_url,
            poll_interval: DEFAULT_POLL_INTERVAL,
            http_client: reqwest::Client::new(),
        }
    }
}

/// Run the discovery loop until `shutdown` is signalled.
pub async fn run(
    config: DiscoveryConfig,
    assets_tx: watch::Sender<Vec<String>>,
    events_tx: mpsc::UnboundedSender<Event>,
    shutdown: CancellationToken,
) {
    let mut ticker = interval(config.poll_interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut state: HashMap<String, MarketSnapshot> = HashMap::new();

    while !shutdown.is_cancelled() {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = ticker.tick() => {
                match poll_once(&config).await {
                    Ok(snapshots) => apply(&snapshots, &mut state, &assets_tx, &events_tx),
                    Err(err) => warn!(error = %err, "discovery poll failed"),
                }
            }
        }
    }
}

async fn poll_once(config: &DiscoveryConfig) -> Result<Vec<MarketSnapshot>> {
    let resp = config
        .http_client
        .get(&config.data_api_url)
        .send()
        .await
        .context("data API request failed")?;
    let body: Value = resp.json().await.context("data API decode failed")?;
    Ok(parse_markets(&body))
}

/// Public for testability: pull `MarketSnapshot`s out of the data-API
/// JSON shape. Tolerates either a top-level array or an object with a
/// `markets` array.
pub fn parse_markets(body: &Value) -> Vec<MarketSnapshot> {
    let markets = body
        .as_array()
        .cloned()
        .or_else(|| body.get("markets").and_then(Value::as_array).cloned())
        .unwrap_or_default();
    markets
        .into_iter()
        .filter_map(|m| {
            let slug = m.get("slug").and_then(Value::as_str)?.to_string();
            let market_type = m
                .get("market_type")
                .and_then(Value::as_str)
                .unwrap_or("btc_5m")
                .to_string();
            let asset_ids = m
                .get("asset_ids")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if asset_ids.is_empty() {
                return None;
            }
            let strike = m.get("strike").and_then(Value::as_f64);
            let end_time_ms = m
                .get("end_time_ms")
                .and_then(Value::as_i64)
                .or_else(|| m.get("endTime").and_then(Value::as_i64));
            Some(MarketSnapshot {
                slug,
                market_type,
                asset_ids,
                strike,
                end_time_ms,
            })
        })
        .collect()
}

fn apply(
    snapshots: &[MarketSnapshot],
    state: &mut HashMap<String, MarketSnapshot>,
    assets_tx: &watch::Sender<Vec<String>>,
    events_tx: &mpsc::UnboundedSender<Event>,
) {
    let mut all_assets: Vec<String> = Vec::new();
    for snap in snapshots {
        all_assets.extend(snap.asset_ids.iter().cloned());
        let changed = match state.get(&snap.slug) {
            None => true,
            Some(prev) => prev != snap,
        };
        if changed {
            let event = build_market_meta(snap);
            if let Err(err) = events_tx.send(event) {
                debug!(error = %err, "market_meta channel closed");
            }
            state.insert(snap.slug.clone(), snap.clone());
        }
    }
    all_assets.sort();
    all_assets.dedup();
    let _ = assets_tx.send(all_assets);
}

fn build_market_meta(snap: &MarketSnapshot) -> Event {
    let now_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);
    Event {
        v: Event::SCHEMA_VERSION,
        ts_ns: now_ns,
        received_ns: now_ns,
        event_type: EventType::MarketMeta,
        market_type: snap.market_type.clone(),
        market_slug: Some(snap.slug.clone()),
        asset_id: snap.asset_ids.first().cloned(),
        side: None,
        price: None,
        size: None,
        sequence: None,
        source: Source::PolymarketDataApi,
        raw: serde_json::json!({
            "slug": snap.slug,
            "market_type": snap.market_type,
            "asset_ids": snap.asset_ids,
            "strike": snap.strike,
            "end_time_ms": snap.end_time_ms,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn parse_markets_extracts_active_5m_windows() {
        let body = json!({
            "markets": [
                {
                    "slug": "btc-up-or-down-1",
                    "market_type": "btc_5m",
                    "asset_ids": ["0xup", "0xdown"],
                    "strike": 63420.0,
                    "end_time_ms": 1_714_579_200_000i64
                },
                {
                    "slug": "btc-no-assets",
                    "market_type": "btc_5m",
                    "asset_ids": []
                }
            ]
        });
        let parsed = parse_markets(&body);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].slug, "btc-up-or-down-1");
        assert_eq!(parsed[0].asset_ids, vec!["0xup", "0xdown"]);
        assert_eq!(parsed[0].strike, Some(63420.0));
        assert_eq!(parsed[0].end_time_ms, Some(1_714_579_200_000));
    }

    #[test]
    fn parse_markets_accepts_top_level_array() {
        let body = json!([
            {
                "slug": "s",
                "market_type": "btc_5m",
                "asset_ids": ["a"]
            }
        ]);
        assert_eq!(parse_markets(&body).len(), 1);
    }

    #[test]
    fn apply_emits_meta_on_first_sight_then_silent_on_unchanged() {
        let snapshots = vec![MarketSnapshot {
            slug: "s".into(),
            market_type: "btc_5m".into(),
            asset_ids: vec!["a".into()],
            strike: Some(1.0),
            end_time_ms: Some(0),
        }];
        let mut state = HashMap::new();
        let (assets_tx, mut assets_rx) = watch::channel(Vec::<String>::new());
        let (events_tx, mut events_rx) = mpsc::unbounded_channel();

        apply(&snapshots, &mut state, &assets_tx, &events_tx);
        let first = events_rx.try_recv().expect("first meta");
        assert_eq!(first.event_type, EventType::MarketMeta);
        assert_eq!(assets_rx.borrow_and_update().clone(), vec!["a".to_string()]);

        apply(&snapshots, &mut state, &assets_tx, &events_tx);
        assert!(events_rx.try_recv().is_err());
    }

    #[test]
    fn apply_re_emits_when_strike_changes() {
        let mut state = HashMap::new();
        let (assets_tx, _assets_rx) = watch::channel(Vec::<String>::new());
        let (events_tx, mut events_rx) = mpsc::unbounded_channel();

        apply(
            &[MarketSnapshot {
                slug: "s".into(),
                market_type: "btc_5m".into(),
                asset_ids: vec!["a".into()],
                strike: Some(1.0),
                end_time_ms: Some(0),
            }],
            &mut state,
            &assets_tx,
            &events_tx,
        );
        events_rx.try_recv().expect("first emit");

        apply(
            &[MarketSnapshot {
                slug: "s".into(),
                market_type: "btc_5m".into(),
                asset_ids: vec!["a".into()],
                strike: Some(2.0),
                end_time_ms: Some(0),
            }],
            &mut state,
            &assets_tx,
            &events_tx,
        );
        let second = events_rx.try_recv().expect("strike change emit");
        assert_eq!(second.raw["strike"], json!(2.0));
    }

    #[tokio::test]
    async fn poll_once_decodes_wiremock_response() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "markets": [
                    {"slug": "s", "market_type": "btc_5m", "asset_ids": ["a"]}
                ]
            })))
            .mount(&server)
            .await;

        let cfg = DiscoveryConfig::new(format!("{}/markets", server.uri()));
        let parsed = poll_once(&cfg).await.expect("ok");
        let _ = Arc::new(parsed);
    }
}
