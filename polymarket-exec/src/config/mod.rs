//! Runtime configuration model and environment-driven assembly for the executor.

pub mod parser;

use std::collections::HashMap;
use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::config::parser::{
    env_or, load_user_auth, parse_asset_market_map, parse_bool, parse_duration_ms, parse_f64,
    parse_log_format, parse_path_optional, parse_socket_addr, parse_usize, split_csv_optional,
    split_csv_required,
};
use crate::risk::RiskLimits;
use crate::strategy::StrategyProfile;

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
    pub private_key: Option<String>,
    pub signature_type: Option<String>,
    pub funder_address: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub service_name: String,
    pub strategy_name: String,
    pub paper_mode: bool,
    pub log_level: String,
    pub log_format: LogFormat,
    pub metrics_bind: SocketAddr,
    pub clob_api_url: String,
    pub data_api_url: String,
    pub relayer_url: String,
    pub relayer_api_key: Option<String>,
    pub relayer_api_key_address: Option<String>,
    pub ctf_contract_address: String,
    pub collateral_token_address: String,
    pub collateral_decimals: u8,
    pub proxy_wallet_address: Option<String>,
    pub market_ws_url: String,
    pub user_ws_url: String,
    pub spot_ws_url: String,
    pub spot_symbol: String,
    pub market_assets: Vec<String>,
    pub user_markets: Vec<String>,
    pub runtime_loop_interval: Duration,
    pub summary_log_interval: Duration,
    pub order_reconcile_interval: Duration,
    pub order_reconcile_stale_window: Duration,
    pub runtime_checkpoint_interval: Duration,
    pub book_stale_after: Duration,
    pub order_store_path: Option<PathBuf>,
    pub runtime_run_id: Option<String>,
    pub ping_interval: Duration,
    pub market_context_path: Option<PathBuf>,
    pub journal_path: Option<PathBuf>,
    pub journal_rotate_bytes: Option<u64>,
    pub starting_cash_usd: f64,
    pub event_log_capacity: usize,
    pub market_id_by_asset: HashMap<String, String>,
    pub risk_limits: RiskLimits,
    pub strategy_profile_path: Option<PathBuf>,
    pub strategy_profile: Option<StrategyProfile>,
    pub user_auth: Option<UserWsAuth>,
    pub dashboard_whale_events_path: Option<std::path::PathBuf>,
    pub dashboard_refresh_ms: u64,
    pub dashboard_event_limit: usize,
    pub audit_path: Option<PathBuf>,
    pub clob_version: String,
    pub clob_v2_builder_code: String,
    pub clob_v2_metadata: String,
    pub clob_v2_neg_risk: bool,
    pub live_post_only: bool,
    pub live_order_ttl: Duration,
    pub live_order_max_age: Duration,
    pub live_reconcile_missing_grace: Duration,
    pub quote_min_order_age: Duration,
    pub live_max_submit_errors: usize,
    pub live_max_cancel_errors: usize,
    pub live_kill_on_reconcile_mismatch: bool,
    pub live_kill_switch_path: Option<PathBuf>,
    pub paper_min_fill_notional_usd: f64,
    pub paper_max_fills_per_order: usize,
    pub paper_min_fill_interval: Duration,
    /// Optional UTC ms timestamp at which the paper market resolves. When
    /// `paper_mode` is true and the runtime clock crosses this value, the
    /// paper environment forces settlement: cancels open orders, applies
    /// merge for paired inventory, and applies redeem at
    /// `paper_market_resolution_price` for stranded inventory.
    pub paper_market_close_at_ms: Option<u64>,
    /// Optional resolution price in [0.0, 1.0] used when settling stranded
    /// inventory at `paper_market_close_at_ms`. 0.0 = "no" wins, 1.0 =
    /// "yes" wins, 0.5 = unknown / split. Required only if there is
    /// stranded (non-paired) inventory at close.
    pub paper_market_resolution_price: Option<f64>,
}

impl AppConfig {
    pub fn from_env() -> Result<Self> {
        let _ = dotenvy::dotenv();

        let service_name = env_or("WHALE_PAIR_EXEC_SERVICE_NAME", "polymarket-exec");
        let strategy_name = env_or("WHALE_PAIR_STRATEGY", "unlawful_shear");
        let strategy_profile_path = parse_path_optional("WHALE_PAIR_STRATEGY_PROFILE_PATH");
        let strategy_profile = strategy_profile_path
            .as_deref()
            .map(StrategyProfile::load)
            .transpose()?;
        let paper_mode = parse_bool("WHALE_PAIR_PAPER_MODE", true)?;
        let log_level = env_or("RUST_LOG", "info");
        let log_format = parse_log_format(&env_or("WHALE_PAIR_EXEC_LOG_FORMAT", "pretty"))?;
        let metrics_bind = parse_socket_addr("WHALE_PAIR_EXEC_METRICS_BIND", "0.0.0.0:9108")?;
        let clob_api_url = env_or("POLYMARKET_CLOB_API_URL", "https://clob.polymarket.com");
        let data_api_url = env_or("POLYMARKET_DATA_API_URL", "https://data-api.polymarket.com");
        let relayer_url = env_or(
            "POLYMARKET_RELAYER_URL",
            crate::wire::relayer::DEFAULT_RELAYER_URL,
        );
        let relayer_api_key = env::var("RELAYER_API_KEY")
            .or_else(|_| env::var("POLYMARKET_RELAYER_API_KEY"))
            .ok()
            .filter(|value| !value.trim().is_empty());
        let relayer_api_key_address = env::var("RELAYER_API_KEY_ADDRESS")
            .or_else(|_| env::var("POLYMARKET_RELAYER_API_KEY_ADDRESS"))
            .ok()
            .filter(|value| !value.trim().is_empty());
        let ctf_contract_address = env_or(
            "POLYMARKET_CTF_CONTRACT_ADDRESS",
            crate::wire::relayer::DEFAULT_CTF_ADDRESS,
        );
        let collateral_token_address = env_or(
            "POLYMARKET_COLLATERAL_TOKEN_ADDRESS",
            crate::wire::relayer::DEFAULT_USDCE_ADDRESS,
        );
        let collateral_decimals = parse_usize("POLYMARKET_COLLATERAL_DECIMALS", 6)? as u8;
        let proxy_wallet_address = env::var("POLYMARKET_PROXY_WALLET_ADDRESS")
            .ok()
            .filter(|value| !value.trim().is_empty());
        // Polymarket CLOB V2 cutover: 2026-04-28. Default to V2; operators can
        // override with POLYMARKET_CLOB_VERSION=v1 if they need legacy behavior
        // for a specific reason (e.g. comparing pre-cutover behavior). The V2
        // signing path is in wire/clob_v2.rs and verified at startup by
        // log_live_venue_config.
        let clob_version = env_or("POLYMARKET_CLOB_VERSION", "v2");
        let clob_v2_builder_code = env_or(
            "POLYMARKET_CLOB_V2_BUILDER_CODE",
            "0x0000000000000000000000000000000000000000000000000000000000000000",
        );
        let clob_v2_metadata = env_or(
            "POLYMARKET_CLOB_V2_METADATA",
            "0x0000000000000000000000000000000000000000000000000000000000000000",
        );
        let clob_v2_neg_risk = parse_bool("POLYMARKET_CLOB_V2_NEG_RISK", false)?;
        let market_ws_url = env_or(
            "POLYMARKET_MARKET_WS_URL",
            "wss://ws-subscriptions-clob.polymarket.com/ws/market",
        );
        let user_ws_url = env_or(
            "POLYMARKET_USER_WS_URL",
            "wss://ws-subscriptions-clob.polymarket.com/ws/user",
        );
        let spot_ws_url = env_or(
            "WHALE_PAIR_EXEC_SPOT_WS_URL",
            "wss://stream.binance.com:9443/ws/btcusdt@aggTrade",
        );
        let spot_symbol = env_or("WHALE_PAIR_EXEC_SPOT_SYMBOL", "BTCUSDT");
        let market_assets = split_csv_required("WHALE_PAIR_ASSET_IDS")?;
        let user_markets = split_csv_optional("WHALE_PAIR_USER_MARKETS");
        let runtime_loop_interval = parse_duration_ms("WHALE_PAIR_EXEC_LOOP_INTERVAL_MS", 1_000)?;
        let summary_log_interval =
            parse_duration_ms("WHALE_PAIR_EXEC_SUMMARY_INTERVAL_MS", 10_000)?;
        let order_reconcile_interval =
            parse_duration_ms("WHALE_PAIR_ORDER_RECONCILE_INTERVAL_MS", 15_000)?;
        let order_reconcile_stale_window =
            parse_duration_ms("WHALE_PAIR_ORDER_RECONCILE_STALE_MS", 30_000)?;
        let runtime_checkpoint_interval =
            parse_duration_ms("WHALE_PAIR_RUNTIME_CHECKPOINT_INTERVAL_MS", 30_000)?;
        let order_store_path = parse_path_optional("WHALE_PAIR_ORDER_STORE_PATH");
        let runtime_run_id = env::var("WHALE_PAIR_RUNTIME_RUN_ID")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let book_stale_after = parse_duration_ms_or_profile(
            "WHALE_PAIR_EXEC_BOOK_STALE_MS",
            strategy_profile
                .as_ref()
                .and_then(|profile| profile.risk.book_stale_ms),
            2_000,
        )?;
        let ping_interval = parse_duration_ms("WHALE_PAIR_EXEC_PING_INTERVAL_MS", 10_000)?;
        let market_context_path = parse_path_optional("WHALE_PAIR_EXEC_MARKET_CONTEXT_PATH");
        let journal_path = parse_path_optional("WHALE_PAIR_EXEC_JOURNAL_PATH");
        let journal_rotate_bytes = env::var("WHALE_PAIR_EXEC_JOURNAL_ROTATE_BYTES")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(|value| {
                value
                    .parse::<u64>()
                    .with_context(|| {
                        format!(
                            "failed to parse WHALE_PAIR_EXEC_JOURNAL_ROTATE_BYTES as u64 from `{value}`"
                        )
                    })
            })
            .transpose()?;
        let starting_cash_usd = parse_f64("WHALE_PAIR_EXEC_STARTING_CASH_USD", 0.0)?;
        let event_log_capacity = parse_usize("WHALE_PAIR_EXEC_EVENT_LOG_CAPACITY", 4_096)?;
        let market_id_by_asset =
            parse_asset_market_map(&env::var("WHALE_PAIR_INSTRUMENT_MARKETS").unwrap_or_default())?;
        let profile_inventory = strategy_profile.as_ref().map(|profile| &profile.inventory);
        let risk_limits = RiskLimits {
            max_order_notional_usd: parse_f64_or_profile(
                "WHALE_PAIR_EXEC_MAX_ORDER_NOTIONAL_USD",
                profile_inventory.and_then(|profile| profile.max_order_notional_usd),
                250.0,
            )?,
            max_gross_notional_usd: parse_f64_or_profile(
                "WHALE_PAIR_EXEC_MAX_GROSS_NOTIONAL_USD",
                profile_inventory.and_then(|profile| profile.max_gross_notional_usd),
                1_000.0,
            )?,
            max_net_notional_per_market_usd: parse_f64_or_profile(
                "WHALE_PAIR_EXEC_MAX_NET_NOTIONAL_PER_MARKET_USD",
                profile_inventory.and_then(|profile| profile.max_net_notional_per_market_usd),
                500.0,
            )?,
            max_position_quantity_per_instrument: parse_f64_or_profile(
                "WHALE_PAIR_EXEC_MAX_POSITION_QTY_PER_INSTRUMENT",
                profile_inventory.and_then(|profile| profile.max_position_quantity_per_instrument),
                10_000.0,
            )?,
            min_free_cash_usd: parse_f64_or_profile(
                "WHALE_PAIR_EXEC_MIN_FREE_CASH_USD",
                profile_inventory.and_then(|profile| profile.min_free_cash_usd),
                0.0,
            )?,
            max_open_orders_total: parse_usize_or_profile(
                "WHALE_PAIR_EXEC_MAX_OPEN_ORDERS_TOTAL",
                profile_inventory.and_then(|profile| profile.max_open_orders_total),
                32,
            )?,
            max_open_orders_per_market: parse_usize_or_profile(
                "WHALE_PAIR_EXEC_MAX_OPEN_ORDERS_PER_MARKET",
                profile_inventory.and_then(|profile| profile.max_open_orders_per_market),
                8,
            )?,
        };
        let user_auth = load_user_auth();
        let dashboard_whale_events_path =
            parse_path_optional("WHALE_PAIR_DASHBOARD_WHALE_EVENTS_PATH");
        let dashboard_refresh_ms =
            parse_duration_ms("WHALE_PAIR_DASHBOARD_REFRESH_MS", 2_000)?.as_millis() as u64;
        let dashboard_event_limit = parse_usize("WHALE_PAIR_DASHBOARD_EVENT_LIMIT", 200)?;
        let audit_path = parse_path_optional("WHALE_PAIR_EXEC_AUDIT_PATH");
        let live_post_only = parse_bool("WHALE_PAIR_LIVE_POST_ONLY", true)?;
        let live_order_ttl = parse_duration_ms("WHALE_PAIR_LIVE_ORDER_TTL_MS", 20_000)?;
        let live_order_max_age = parse_duration_ms("WHALE_PAIR_LIVE_ORDER_MAX_AGE_MS", 25_000)?;
        let live_reconcile_missing_grace =
            parse_duration_ms("WHALE_PAIR_LIVE_RECONCILE_MISSING_GRACE_MS", 5_000)?;
        let quote_min_order_age_default_ms = if paper_mode { 750 } else { 5_000 };
        let quote_min_order_age = parse_duration_ms(
            "WHALE_PAIR_QUOTE_MIN_ORDER_AGE_MS",
            quote_min_order_age_default_ms,
        )?;
        let live_max_submit_errors = parse_usize("WHALE_PAIR_LIVE_MAX_SUBMIT_ERRORS", 1)?;
        let live_max_cancel_errors = parse_usize("WHALE_PAIR_LIVE_MAX_CANCEL_ERRORS", 1)?;
        let live_kill_on_reconcile_mismatch =
            parse_bool("WHALE_PAIR_LIVE_KILL_ON_RECONCILE_MISMATCH", true)?;
        let live_kill_switch_path = parse_path_optional("WHALE_PAIR_LIVE_KILL_SWITCH_PATH");
        let paper_min_fill_notional_usd =
            parse_f64("WHALE_PAIR_PAPER_MIN_FILL_NOTIONAL_USD", 0.05)?;
        let paper_max_fills_per_order = parse_usize("WHALE_PAIR_PAPER_MAX_FILLS_PER_ORDER", 3)?;
        let paper_min_fill_interval =
            parse_duration_ms("WHALE_PAIR_PAPER_MIN_FILL_INTERVAL_MS", 750)?;
        let paper_market_close_at_ms = std::env::var("WHALE_PAIR_PAPER_MARKET_CLOSE_AT_MS")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map(|v| {
                v.trim()
                    .parse::<u64>()
                    .map_err(|err| anyhow::anyhow!(
                        "invalid WHALE_PAIR_PAPER_MARKET_CLOSE_AT_MS: {err}"
                    ))
            })
            .transpose()?;
        let paper_market_resolution_price =
            std::env::var("WHALE_PAIR_PAPER_MARKET_RESOLUTION_PRICE")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .map(|v| {
                    v.trim()
                        .parse::<f64>()
                        .map_err(|err| anyhow::anyhow!(
                            "invalid WHALE_PAIR_PAPER_MARKET_RESOLUTION_PRICE: {err}"
                        ))
                        .and_then(|p| {
                            if (0.0..=1.0).contains(&p) {
                                Ok(p)
                            } else {
                                Err(anyhow::anyhow!(
                                    "WHALE_PAIR_PAPER_MARKET_RESOLUTION_PRICE must be in [0.0, 1.0], got {p}"
                                ))
                            }
                        })
                })
                .transpose()?;

        Ok(Self {
            service_name,
            strategy_name,
            paper_mode,
            log_level,
            log_format,
            metrics_bind,
            clob_api_url,
            data_api_url,
            relayer_url,
            relayer_api_key,
            relayer_api_key_address,
            ctf_contract_address,
            collateral_token_address,
            collateral_decimals,
            proxy_wallet_address,
            market_ws_url,
            user_ws_url,
            spot_ws_url,
            spot_symbol,
            market_assets,
            user_markets,
            runtime_loop_interval,
            summary_log_interval,
            order_reconcile_interval,
            order_reconcile_stale_window,
            runtime_checkpoint_interval,
            book_stale_after,
            order_store_path,
            runtime_run_id,
            ping_interval,
            market_context_path,
            journal_path,
            journal_rotate_bytes,
            starting_cash_usd,
            event_log_capacity,
            market_id_by_asset,
            risk_limits,
            strategy_profile_path,
            strategy_profile,
            user_auth,
            dashboard_whale_events_path,
            dashboard_refresh_ms,
            dashboard_event_limit,
            audit_path,
            clob_version,
            clob_v2_builder_code,
            clob_v2_metadata,
            clob_v2_neg_risk,
            live_post_only,
            live_order_ttl,
            live_order_max_age,
            live_reconcile_missing_grace,
            quote_min_order_age,
            live_max_submit_errors,
            live_max_cancel_errors,
            live_kill_on_reconcile_mismatch,
            live_kill_switch_path,
            paper_min_fill_notional_usd,
            paper_max_fills_per_order,
            paper_min_fill_interval,
            paper_market_close_at_ms,
            paper_market_resolution_price,
        })
    }

    pub fn market_id_for_asset(&self, asset_id: &str) -> String {
        self.market_id_by_asset
            .get(asset_id)
            .cloned()
            .unwrap_or_else(|| asset_id.to_string())
    }
}

fn parse_duration_ms_or_profile(key: &str, profile: Option<u64>, default: u64) -> Result<Duration> {
    if env::var_os(key).is_some() {
        parse_duration_ms(key, default)
    } else {
        Ok(Duration::from_millis(profile.unwrap_or(default)))
    }
}

fn parse_f64_or_profile(key: &str, profile: Option<f64>, default: f64) -> Result<f64> {
    if env::var_os(key).is_some() {
        parse_f64(key, default)
    } else {
        Ok(profile.unwrap_or(default))
    }
}

fn parse_usize_or_profile(key: &str, profile: Option<usize>, default: usize) -> Result<usize> {
    if env::var_os(key).is_some() {
        parse_usize(key, default)
    } else {
        Ok(profile.unwrap_or(default))
    }
}
