pub mod parser;

use std::collections::HashMap;
use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;

use crate::config::parser::{
    env_or, load_user_auth, parse_asset_market_map, parse_bool, parse_duration_ms, parse_f64,
    parse_log_format, parse_path_optional, parse_socket_addr, parse_usize, split_csv_optional,
    split_csv_required,
};
use crate::risk::RiskLimits;

#[derive(Debug, Clone, Copy)]
pub enum LogFormat {
    Pretty,
    Json,
}

#[derive(Debug, Clone)]
pub struct UserWsAuth {
    pub api_key: String,
    pub api_secret: String,
    pub api_passphrase: String,
}

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub service_name: String,
    pub strategy_name: String,
    pub paper_mode: bool,
    pub log_level: String,
    pub log_format: LogFormat,
    pub metrics_bind: SocketAddr,
    pub market_ws_url: String,
    pub user_ws_url: String,
    pub market_assets: Vec<String>,
    pub user_markets: Vec<String>,
    pub runtime_loop_interval: Duration,
    pub summary_log_interval: Duration,
    pub book_stale_after: Duration,
    pub ping_interval: Duration,
    pub market_context_path: Option<PathBuf>,
    pub journal_path: Option<PathBuf>,
    pub starting_cash_usd: f64,
    pub event_log_capacity: usize,
    pub market_id_by_asset: HashMap<String, String>,
    pub risk_limits: RiskLimits,
    pub user_auth: Option<UserWsAuth>,
    pub dashboard_whale_events_path: Option<std::path::PathBuf>,
    pub dashboard_refresh_ms: u64,
    pub dashboard_event_limit: usize,
}

impl AppConfig {
    pub fn from_env() -> Result<Self> {
        // TODO(2026-04-23): centralize strategy config into one versioned block so
        // multi-strategy paper runs can be selected/enforced per run without env drift.
        let _ = dotenvy::dotenv();

        let service_name = env_or("WHALE_PAIR_EXEC_SERVICE_NAME", "whale-pair-exec");
        let strategy_name = env_or("WHALE_PAIR_STRATEGY", "unlawful_shear");
        let paper_mode = parse_bool("WHALE_PAIR_PAPER_MODE", true)?;
        let log_level = env_or("RUST_LOG", "info");
        let log_format = parse_log_format(&env_or("WHALE_PAIR_EXEC_LOG_FORMAT", "pretty"))?;
        let metrics_bind = parse_socket_addr("WHALE_PAIR_EXEC_METRICS_BIND", "0.0.0.0:9108")?;
        let market_ws_url = env_or(
            "POLYMARKET_MARKET_WS_URL",
            "wss://ws-subscriptions-clob.polymarket.com/ws/market",
        );
        let user_ws_url = env_or(
            "POLYMARKET_USER_WS_URL",
            "wss://ws-subscriptions-clob.polymarket.com/ws/user",
        );
        let market_assets = split_csv_required("WHALE_PAIR_ASSET_IDS")?;
        let user_markets = split_csv_optional("WHALE_PAIR_USER_MARKETS");
        let runtime_loop_interval = parse_duration_ms(
            "WHALE_PAIR_EXEC_LOOP_INTERVAL_MS",
            1_000,
        )?;
        let summary_log_interval = parse_duration_ms(
            "WHALE_PAIR_EXEC_SUMMARY_INTERVAL_MS",
            10_000,
        )?;
        let book_stale_after = parse_duration_ms(
            "WHALE_PAIR_EXEC_BOOK_STALE_MS",
            2_000,
        )?;
        let ping_interval = parse_duration_ms("WHALE_PAIR_EXEC_PING_INTERVAL_MS", 10_000)?;
        let market_context_path = parse_path_optional("WHALE_PAIR_EXEC_MARKET_CONTEXT_PATH");
        let journal_path = parse_path_optional("WHALE_PAIR_EXEC_JOURNAL_PATH");
        let starting_cash_usd =
            parse_f64("WHALE_PAIR_EXEC_STARTING_CASH_USD", 0.0)?;
        let event_log_capacity =
            parse_usize("WHALE_PAIR_EXEC_EVENT_LOG_CAPACITY", 4_096)?;
        let market_id_by_asset = parse_asset_market_map(
            &env::var("WHALE_PAIR_INSTRUMENT_MARKETS").unwrap_or_default(),
        )?;
        let risk_limits = RiskLimits {
            max_order_notional_usd: parse_f64(
                "WHALE_PAIR_EXEC_MAX_ORDER_NOTIONAL_USD",
                250.0,
            )?,
            max_gross_notional_usd: parse_f64(
                "WHALE_PAIR_EXEC_MAX_GROSS_NOTIONAL_USD",
                1_000.0,
            )?,
            max_net_notional_per_market_usd: parse_f64(
                "WHALE_PAIR_EXEC_MAX_NET_NOTIONAL_PER_MARKET_USD",
                500.0,
            )?,
            max_position_quantity_per_instrument: parse_f64(
                "WHALE_PAIR_EXEC_MAX_POSITION_QTY_PER_INSTRUMENT",
                10_000.0,
            )?,
            min_free_cash_usd: parse_f64(
                "WHALE_PAIR_EXEC_MIN_FREE_CASH_USD",
                0.0,
            )?,
            max_open_orders_total: parse_usize(
                "WHALE_PAIR_EXEC_MAX_OPEN_ORDERS_TOTAL",
                32,
            )?,
            max_open_orders_per_market: parse_usize(
                "WHALE_PAIR_EXEC_MAX_OPEN_ORDERS_PER_MARKET",
                8,
            )?,
        };
        let user_auth = load_user_auth();
        let dashboard_whale_events_path =
            parse_path_optional("WHALE_PAIR_DASHBOARD_WHALE_EVENTS_PATH");
        let dashboard_refresh_ms =
            parse_duration_ms("WHALE_PAIR_DASHBOARD_REFRESH_MS", 2_000)?.as_millis() as u64;
        let dashboard_event_limit =
            parse_usize("WHALE_PAIR_DASHBOARD_EVENT_LIMIT", 200)?;

        Ok(Self {
            service_name,
            strategy_name,
            paper_mode,
            log_level,
            log_format,
            metrics_bind,
            market_ws_url,
            user_ws_url,
            market_assets,
            user_markets,
            runtime_loop_interval,
            summary_log_interval,
            book_stale_after,
            ping_interval,
            market_context_path,
            journal_path,
            starting_cash_usd,
            event_log_capacity,
            market_id_by_asset,
            risk_limits,
            user_auth,
            dashboard_whale_events_path,
            dashboard_refresh_ms,
            dashboard_event_limit,
        })
    }

    pub fn market_id_for_asset(&self, asset_id: &str) -> String {
        self.market_id_by_asset
            .get(asset_id)
            .cloned()
            .unwrap_or_else(|| asset_id.to_string())
    }
}
