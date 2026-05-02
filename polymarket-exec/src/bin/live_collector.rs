//! Live collector binary.
//!
//! Reuses `polymarket-exec`'s production wire-clients (market_ws, user_ws,
//! spot_ws) via the additive `with_raw_tap` extension, normalises each
//! captured frame into the canonical v=1 `Event` schema, and streams batches
//! to AWS Kinesis Firehose. Firehose handles JSON-to-Parquet conversion using
//! the Glue table schema provisioned by `infra/terraform/collector/`.
//!
//! Env vars (all `PM_COLLECTOR_*` prefixed; names match `task.tf` exactly):
//!   PM_COLLECTOR_FIREHOSE_STREAM         required, AWS Kinesis Firehose stream name
//!   PM_COLLECTOR_REGION                  required, e.g. us-east-1
//!   PM_COLLECTOR_DATA_API_URL            default https://data-api.polymarket.com
//!   PM_COLLECTOR_MARKET_WS_URL           default wss://ws-subscriptions-clob.polymarket.com/ws/market
//!   PM_COLLECTOR_BINANCE_WS_URL          default wss://stream.binance.com:9443/ws/btcusdt@aggTrade
//!   PM_COLLECTOR_COINBASE_WS_URL         default wss://advanced-trade-ws.coinbase.com
//!   PM_COLLECTOR_USER_WS_URL             default wss://ws-subscriptions-clob.polymarket.com/ws/user
//!   PM_USER_WS_DISABLED                  "1" to skip user_ws (default when no creds)
//!   PM_USER_WS_AUTH_API_KEY              required if user_ws enabled
//!   PM_USER_WS_AUTH_SECRET               required if user_ws enabled
//!   PM_USER_WS_AUTH_PASSPHRASE           required if user_ws enabled
//!   PM_COLLECTOR_HEALTH_PORT             default 8080
//!   PM_COLLECTOR_DISCOVERY_INTERVAL_SECS default 30
//!   PM_COLLECTOR_FLUSH_INTERVAL_SECS     default 5
//!
//! Exit codes: 0 normal shutdown, 1 unrecoverable error.

use std::env;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use aws_sdk_firehose::Client as FirehoseClient;
use serde_json::Value;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::{mpsc, watch};
use tokio::time::interval;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use polymarket_exec::book::BookStore;
use polymarket_exec::collector::discovery::{self, DiscoveryConfig};
use polymarket_exec::collector::firehose_sink::{AwsFirehose, FirehoseSink};
use polymarket_exec::collector::gap_detector::GapDetector;
use polymarket_exec::collector::health::{self, HealthState};
use polymarket_exec::collector::schema::{Event, EventType, Source};
use polymarket_exec::config::UserWsAuth;
use polymarket_exec::metrics::AppMetrics;
use polymarket_exec::wire::market_ws::MarketWsClient;
use polymarket_exec::wire::raw_frame::RawFrame;
use polymarket_exec::wire::spot_ws::SpotWsClient;
use polymarket_exec::wire::user_ws::UserWsClient;

const DEFAULT_DATA_API_URL: &str = "https://data-api.polymarket.com";
const DEFAULT_MARKET_WS_URL: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/market";
const DEFAULT_BINANCE_WS_URL: &str = "wss://stream.binance.com:9443/ws/btcusdt@aggTrade";
const DEFAULT_COINBASE_WS_URL: &str = "wss://advanced-trade-ws.coinbase.com";
const DEFAULT_USER_WS_URL: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/user";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    if let Err(error) = run().await {
        error!(error = ?error, "live_collector exited with error");
        std::process::exit(1);
    }
    Ok(())
}

async fn run() -> Result<()> {
    let cfg = Config::from_env()?;
    info!(
        firehose_stream = %cfg.firehose_stream,
        region = %cfg.region,
        "live_collector starting"
    );

    let aws_cfg = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new(cfg.region.clone()))
        .load()
        .await;
    let firehose = FirehoseClient::new(&aws_cfg);
    let sink = Arc::new(FirehoseSink::new(Arc::new(AwsFirehose::new(
        firehose,
        cfg.firehose_stream.clone(),
    ))));

    let metrics = Arc::new(AppMetrics::new().context("init AppMetrics")?);
    let books = Arc::new(BookStore::new(&[]));

    let (raw_tx, raw_rx) = mpsc::unbounded_channel::<RawFrame>();
    let (events_tx, events_rx) = mpsc::unbounded_channel::<Event>();
    let (assets_tx, assets_rx) = watch::channel::<Vec<String>>(Vec::new());

    let shutdown = CancellationToken::new();

    let market_client = MarketWsClient::new(
        cfg.market_ws_url.clone(),
        Vec::new(),
        Duration::from_secs(15),
        Arc::clone(&books),
        Arc::clone(&metrics),
    )
    .with_asset_updates(assets_rx)
    .with_raw_tap(raw_tx.clone());

    let spot_client = SpotWsClient::new(
        cfg.binance_ws_url.clone(),
        "btcusdt".into(),
        Duration::from_secs(15),
        Arc::clone(&metrics),
        None,
    )
    .with_raw_tap(raw_tx.clone());

    let user_client = if cfg.user_ws_enabled {
        let auth = cfg
            .user_ws_auth
            .clone()
            .context("user_ws enabled but credentials missing")?;
        Some(
            UserWsClient::new(
                cfg.user_ws_url.clone(),
                auth,
                Vec::new(),
                Duration::from_secs(30),
                Arc::clone(&metrics),
                None,
            )
            .with_raw_tap(raw_tx.clone()),
        )
    } else {
        None
    };

    let mut handles = Vec::new();

    {
        let st = shutdown.clone();
        handles.push(tokio::spawn(async move {
            market_client.run(st).await;
        }));
    }
    {
        let st = shutdown.clone();
        handles.push(tokio::spawn(async move {
            spot_client.run(st).await;
        }));
    }
    if let Some(client) = user_client {
        let st = shutdown.clone();
        handles.push(tokio::spawn(async move {
            client.run(st).await;
        }));
    }

    {
        let st = shutdown.clone();
        let dcfg = DiscoveryConfig::new(cfg.data_api_url.clone());
        let assets_tx = assets_tx.clone();
        let events_tx = events_tx.clone();
        handles.push(tokio::spawn(async move {
            discovery::run(dcfg, assets_tx, events_tx, st).await;
        }));
    }

    handles.push({
        let sink = Arc::clone(&sink);
        let st = shutdown.clone();
        tokio::spawn(converter_loop(raw_rx, events_rx, sink, st))
    });

    handles.push({
        let sink = Arc::clone(&sink);
        let st = shutdown.clone();
        let interval_secs = cfg.flush_interval_secs;
        tokio::spawn(flusher_loop(sink, st, interval_secs))
    });

    handles.push({
        let sink = Arc::clone(&sink);
        let st = shutdown.clone();
        let port = cfg.health_port;
        tokio::spawn(async move {
            let addr = SocketAddr::from(([0, 0, 0, 0], port));
            let state = HealthState { sink };
            if let Err(error) = health::serve(addr, state, st).await {
                warn!(error = ?error, "health server exited");
            }
        })
    });

    let mut sigterm = signal(SignalKind::terminate()).context("install SIGTERM handler")?;
    let mut sigint = signal(SignalKind::interrupt()).context("install SIGINT handler")?;
    tokio::select! {
        _ = sigterm.recv() => info!("received SIGTERM, shutting down"),
        _ = sigint.recv() => info!("received SIGINT, shutting down"),
    }
    shutdown.cancel();

    let grace = tokio::time::timeout(Duration::from_secs(30), async {
        sink.flush_now().await;
        for handle in handles {
            let _ = handle.await;
        }
    });
    if grace.await.is_err() {
        warn!("shutdown grace period exceeded");
    }

    info!("live_collector stopped cleanly");
    Ok(())
}

async fn converter_loop(
    mut raw_rx: mpsc::UnboundedReceiver<RawFrame>,
    mut events_rx: mpsc::UnboundedReceiver<Event>,
    sink: Arc<FirehoseSink>,
    shutdown: CancellationToken,
) {
    let mut gap = GapDetector::new();
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            Some(frame) = raw_rx.recv() => {
                if let Some(event) = raw_frame_to_event(frame) {
                    if let Some(synthetic) = gap.observe(&event) {
                        let _ = sink.enqueue(&synthetic).await;
                    }
                    let _ = sink.enqueue(&event).await;
                }
            }
            Some(event) = events_rx.recv() => {
                let _ = sink.enqueue(&event).await;
            }
            else => break,
        }
    }
}

async fn flusher_loop(sink: Arc<FirehoseSink>, shutdown: CancellationToken, interval_secs: u64) {
    let mut tick = interval(Duration::from_secs(interval_secs.max(1)));
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = tick.tick() => {
                if sink.should_periodic_flush().await {
                    sink.flush_now().await;
                }
            }
        }
    }
    sink.flush_now().await;
}

fn raw_frame_to_event(frame: RawFrame) -> Option<Event> {
    let RawFrame {
        source,
        asset_id,
        observed_at_ns,
        payload,
    } = frame;

    match source {
        "polymarket_market_ws" => Some(market_event(payload, asset_id, observed_at_ns)),
        "polymarket_user_ws" => Some(user_event(payload, asset_id, observed_at_ns)),
        "binance_aggtrade" => Some(btc_tick("binance_aggtrade", payload, observed_at_ns)),
        "coinbase_match" => Some(btc_tick("coinbase_match", payload, observed_at_ns)),
        _ => None,
    }
}

fn market_event(payload: Value, asset_id: Option<String>, observed_at_ns: i64) -> Event {
    let event_type_str = payload
        .get("event_type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let (event_type, side, price, size) = match event_type_str.as_str() {
        "book" => (EventType::BookSnapshot, None, None, None),
        "price_change" => {
            let side = payload
                .get("side")
                .and_then(Value::as_str)
                .map(str::to_ascii_uppercase);
            let price = payload
                .get("price")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let size = payload
                .get("size")
                .and_then(Value::as_str)
                .map(str::to_owned);
            (EventType::BookDelta, side, price, size)
        }
        "last_trade_price" | "trade" => {
            let side = payload
                .get("side")
                .and_then(Value::as_str)
                .map(str::to_ascii_uppercase);
            let price = payload
                .get("price")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let size = payload
                .get("size")
                .and_then(Value::as_str)
                .map(str::to_owned);
            (EventType::Trade, side, price, size)
        }
        _ => (EventType::BookDelta, None, None, None),
    };
    Event {
        v: 1,
        ts_ns: payload
            .get("timestamp")
            .and_then(Value::as_i64)
            .map(|ms| ms.saturating_mul(1_000_000))
            .unwrap_or(observed_at_ns),
        received_ns: observed_at_ns,
        event_type,
        market_type: market_type_for_asset(&payload),
        market_slug: payload
            .get("market")
            .and_then(Value::as_str)
            .map(str::to_owned),
        asset_id,
        side,
        price,
        size,
        sequence: payload.get("hash").and_then(Value::as_i64),
        source: Source::PolymarketMarketWs,
        raw: payload,
    }
}

fn user_event(payload: Value, asset_id: Option<String>, observed_at_ns: i64) -> Event {
    let event_type_str = payload
        .get("event_type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let event_type = if event_type_str == "trade" {
        EventType::UserFill
    } else {
        EventType::UserOrder
    };
    Event {
        v: 1,
        ts_ns: payload
            .get("timestamp")
            .and_then(Value::as_i64)
            .map(|ms| ms.saturating_mul(1_000_000))
            .unwrap_or(observed_at_ns),
        received_ns: observed_at_ns,
        event_type,
        market_type: market_type_for_asset(&payload),
        market_slug: payload
            .get("market")
            .and_then(Value::as_str)
            .map(str::to_owned),
        asset_id,
        side: payload
            .get("side")
            .and_then(Value::as_str)
            .map(str::to_ascii_uppercase),
        price: payload
            .get("price")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| {
                payload
                    .get("price")
                    .and_then(Value::as_f64)
                    .map(|p| p.to_string())
            }),
        size: payload
            .get("size")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| {
                payload
                    .get("size")
                    .and_then(Value::as_f64)
                    .map(|s| s.to_string())
            }),
        sequence: None,
        source: Source::PolymarketUserWs,
        raw: payload,
    }
}

fn btc_tick(source_label: &str, payload: Value, observed_at_ns: i64) -> Event {
    let source = match source_label {
        "binance_aggtrade" => Source::BinanceAggtrade,
        "coinbase_match" => Source::CoinbaseMatch,
        _ => Source::Collector,
    };
    let price = payload
        .get("p")
        .or_else(|| payload.get("price"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let size = payload
        .get("q")
        .or_else(|| payload.get("size"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let ts_ns = payload
        .get("T")
        .and_then(Value::as_i64)
        .map(|ms| ms.saturating_mul(1_000_000))
        .unwrap_or(observed_at_ns);
    Event {
        v: 1,
        ts_ns,
        received_ns: observed_at_ns,
        event_type: EventType::BtcTick,
        market_type: "_global".into(),
        market_slug: None,
        asset_id: None,
        side: None,
        price,
        size,
        sequence: payload.get("a").and_then(Value::as_i64),
        source,
        raw: payload,
    }
}

fn market_type_for_asset(payload: &Value) -> String {
    payload
        .get("market_type")
        .and_then(Value::as_str)
        .unwrap_or("btc_5m")
        .to_owned()
}

#[derive(Clone)]
struct Config {
    firehose_stream: String,
    region: String,
    data_api_url: String,
    market_ws_url: String,
    binance_ws_url: String,
    #[allow(dead_code)]
    coinbase_ws_url: String,
    user_ws_url: String,
    user_ws_enabled: bool,
    user_ws_auth: Option<UserWsAuth>,
    health_port: u16,
    flush_interval_secs: u64,
    #[allow(dead_code)]
    discovery_interval_secs: u64,
}

impl Config {
    fn from_env() -> Result<Self> {
        let firehose_stream = env::var("PM_COLLECTOR_FIREHOSE_STREAM")
            .context("PM_COLLECTOR_FIREHOSE_STREAM not set")?;
        let region = env::var("PM_COLLECTOR_REGION").context("PM_COLLECTOR_REGION not set")?;
        let user_ws_disabled = env::var("PM_USER_WS_DISABLED")
            .map(|v| v == "1")
            .unwrap_or(true);
        let user_ws_enabled = !user_ws_disabled;
        let user_ws_auth = if user_ws_enabled {
            Some(UserWsAuth {
                api_key: env::var("PM_USER_WS_AUTH_API_KEY")
                    .context("PM_USER_WS_AUTH_API_KEY required when user_ws enabled")?,
                api_secret: env::var("PM_USER_WS_AUTH_SECRET")
                    .context("PM_USER_WS_AUTH_SECRET required when user_ws enabled")?,
                api_passphrase: env::var("PM_USER_WS_AUTH_PASSPHRASE")
                    .context("PM_USER_WS_AUTH_PASSPHRASE required when user_ws enabled")?,
                private_key: env::var("PM_USER_WS_AUTH_PRIVATE_KEY").ok(),
                signature_type: env::var("PM_USER_WS_AUTH_SIGNATURE_TYPE").ok(),
                funder_address: env::var("PM_USER_WS_AUTH_FUNDER_ADDRESS").ok(),
            })
        } else {
            None
        };
        Ok(Self {
            firehose_stream,
            region,
            data_api_url: env::var("PM_COLLECTOR_DATA_API_URL")
                .unwrap_or_else(|_| DEFAULT_DATA_API_URL.into()),
            market_ws_url: env::var("PM_COLLECTOR_MARKET_WS_URL")
                .unwrap_or_else(|_| DEFAULT_MARKET_WS_URL.into()),
            binance_ws_url: env::var("PM_COLLECTOR_BINANCE_WS_URL")
                .unwrap_or_else(|_| DEFAULT_BINANCE_WS_URL.into()),
            coinbase_ws_url: env::var("PM_COLLECTOR_COINBASE_WS_URL")
                .unwrap_or_else(|_| DEFAULT_COINBASE_WS_URL.into()),
            user_ws_url: env::var("PM_COLLECTOR_USER_WS_URL")
                .unwrap_or_else(|_| DEFAULT_USER_WS_URL.into()),
            user_ws_enabled,
            user_ws_auth,
            health_port: env::var("PM_COLLECTOR_HEALTH_PORT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(8080),
            flush_interval_secs: env::var("PM_COLLECTOR_FLUSH_INTERVAL_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(5),
            discovery_interval_secs: env::var("PM_COLLECTOR_DISCOVERY_INTERVAL_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(30),
        })
    }
}
