//! Runtime configuration model and environment-driven assembly for the executor.

pub mod parser;

use std::collections::HashMap;
use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::config::parser::{
    env_or, env_value, load_user_auth, parse_asset_market_map, parse_bool, parse_duration_ms,
    parse_f64, parse_log_format, parse_path_optional, parse_socket_addr, parse_usize,
    split_csv_optional, split_csv_required,
};
use crate::risk::RiskLimits;
use crate::strategy::StrategyProfile;

const DEFAULT_BINANCE_REST_BOOTSTRAP_URL: &str = "https://api.binance.com/api/v3/aggTrades";
const DEFAULT_COINBASE_SPOT_WS_URL: &str = "wss://advanced-trade-ws.coinbase.com";

#[derive(Debug, Clone, Copy)]
pub enum LogFormat {
    Pretty,
    Json,
}

/// One slug-prefix / window pair that the discovery loop should sweep.
///
/// The legacy single-family discovery uses `market_discovery_slug_prefix` plus
/// `market_discovery_window`. Multi-market strategies may need to span
/// multiple families simultaneously (BTC 5m + ETH 5m + BTC 15m + ...), each
/// with its own slug timestamp window. When `market_discovery_families` is
/// non-empty the runner iterates these instead of the single-prefix path.
#[derive(Debug, Clone)]
pub struct MarketDiscoveryFamily {
    pub prefix: String,
    pub window: Duration,
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
    pub ctf_collateral_token_address: String,
    pub collateral_token_address: String,
    pub collateral_decimals: u8,
    pub proxy_wallet_address: Option<String>,
    pub polygon_rpc_url: Option<String>,
    pub market_ws_url: String,
    pub user_ws_url: String,
    pub spot_ws_url: String,
    pub spot_rest_bootstrap_url: Option<String>,
    pub coinbase_spot_ws_url: Option<String>,
    pub spot_symbol: String,
    pub market_assets: Vec<String>,
    pub user_markets: Vec<String>,
    pub market_discovery_enabled: bool,
    pub market_discovery_interval: Duration,
    pub market_discovery_window: Duration,
    pub market_discovery_include_prev: usize,
    pub market_discovery_include_next: usize,
    pub market_discovery_gamma_url: String,
    pub market_discovery_slug_prefix: String,
    pub market_discovery_families: Vec<MarketDiscoveryFamily>,
    pub runtime_loop_interval: Duration,
    pub summary_log_interval: Duration,
    pub order_reconcile_interval: Duration,
    pub order_reconcile_stale_window: Duration,
    pub runtime_checkpoint_interval: Duration,
    pub book_stale_after: Duration,
    pub order_store_path: Option<PathBuf>,
    pub runtime_run_id: Option<String>,
    pub ping_interval: Duration,
    /// Spot WS connection-rail staleness threshold. Any inbound frame
    /// (text/binary/ping/pong) resets the timer. Used to detect TCP-level
    /// silent stalls.
    pub spot_ws_conn_stale_timeout: Duration,
    /// Spot WS data-rail staleness threshold. Only successfully-parsed
    /// aggTrade messages reset this timer. Used to detect subscription
    /// drops where the venue keeps the connection alive but stops
    /// sending the actual BTC trade tape.
    pub spot_ws_data_stale_timeout: Duration,
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
    pub quote_churn_window: Duration,
    pub quote_hard_pull: Duration,
    pub quote_max_churn_per_window: usize,
    pub quote_max_submit_per_window: usize,
    pub quote_max_replace_per_window: usize,
    pub quote_max_cancel_per_window: usize,
    pub live_max_submit_errors: usize,
    pub live_max_cancel_errors: usize,
    pub live_kill_on_reconcile_mismatch: bool,
    pub live_kill_switch_path: Option<PathBuf>,
    /// Live EOA startup collateral repair. When enabled, the engine checks the
    /// signer's Polygon USDC.e balance before quoting and wraps any balance
    /// above `live_pusd_auto_wrap_min_usd` into pUSD via Polymarket's
    /// CollateralOnramp. This is intentionally opt-in because it sends
    /// on-chain transactions and consumes MATIC gas.
    pub live_pusd_auto_wrap: bool,
    pub live_pusd_auto_wrap_min_usd: f64,
    /// After how long of healthy operation post-restart should the runtime
    /// auto-recover from a persisted RiskOff state. Default 30s. Set to 0
    /// to disable (operator must clear runtime_state manually).
    ///
    /// This exists because `Persist risk-off runtime state` (commit 2982576)
    /// added durable persistence of RiskOff but no recovery path. A single
    /// transient reconcile mismatch traps the bot indefinitely until human
    /// intervention. The default 30s window gives the protective property
    /// (don't trade if conditions are still bad) without trapping the bot
    /// forever (transient triggers self-clear quickly).
    pub live_risk_off_auto_recover: Duration,
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
    /// Phase 2 paper env: minimum ms between submit ack and the first fill
    /// attempt. Forces the book to update at least once after the simulated
    /// round-trip before a fill can be considered. Default 150 ms.
    pub paper_submit_latency_ms: u64,
    /// Phase 2 paper env: assumed queue position as a fraction of top-of-book
    /// size. 0.75 means we assume we are 75% back in the queue (conservative).
    /// Default 0.75.
    pub paper_queue_depth_fraction: f64,
    /// Phase 2 paper env: probability of post-only rejection in paper mode
    /// when the order would cross the book. Default 0.85.
    pub paper_post_only_reject_probability: f64,
    /// Phase 2 paper env: window after a cancel request during which a late
    /// fill may still be applied. Default 500 ms.
    pub paper_cancel_race_window_ms: u64,
    /// Phase 3 paper env: optional path for the per-session JSON report
    /// card (`PaperReportSummary`). When set and `paper_mode` is true, the
    /// runtime instantiates a `PaperReportWriter`, accumulates fill / edge
    /// / reject metrics, and flushes on shutdown. None disables.
    pub paper_report_path: Option<PathBuf>,
    /// Phase 5 paper env: optional JSONL path for compact book-state
    /// snapshots. When set, every book update is appended; the resulting
    /// file is the input to `PM_BTC_5M_EXEC_MODE=replay`. Useful in any
    /// mode (paper, shadow_live, even live) for forensic post-hoc replay.
    pub book_snapshot_log_path: Option<PathBuf>,
    /// Optional JSONL path for shadow quote records. Each paper/shadow-live
    /// submit records the intended order plus top-N book depth so offline
    /// calibration can compare quote decisions against later book movement.
    pub shadow_quote_log_path: Option<PathBuf>,
    /// Phase 5 paper env: max depth levels per side captured in each book
    /// snapshot record. Bigger = bigger files; smaller = less faithful
    /// replay. Default 10.
    pub book_snapshot_max_levels: usize,
    /// Maker rebate coefficient. Applied as a negative fee on maker fills
    /// in paper mode: `fee_usd = -notional * coeff * p * (1-p)`. Default
    /// 0.0 (no rebate). Set to V2's actual maker-rebate value to model
    /// economics realistically.
    pub paper_maker_rebate_coeff: f64,
    /// Taker fee coefficient override for paper mode. When None, the
    /// strategy's `taker_fee_coeff()` is used (current behavior). When
    /// Some, overrides for paper-mode fills only — useful for A/B
    /// testing fee scenarios.
    pub paper_taker_fee_coeff_override: Option<f64>,
}

impl AppConfig {
    pub fn from_env() -> Result<Self> {
        if should_load_dotenv() {
            let _ = dotenvy::dotenv();
        }

        let service_name = env_value("PM_BTC_5M_EXEC_SERVICE_NAME")
            .unwrap_or_else(|| "polymarket-exec".to_string());
        let strategy_name = env_value("PM_BTC_5M_STRATEGY")
            .unwrap_or_else(|| "pair_cost_arb,paired_mm".to_string());
        let strategy_profile_paths = parse_strategy_profile_paths();
        let strategy_profile_path = strategy_profile_paths.first().cloned();
        let strategy_profile = if strategy_profile_paths.is_empty() {
            None
        } else if strategy_profile_paths.len() == 1 {
            Some(StrategyProfile::load(&strategy_profile_paths[0])?)
        } else {
            Some(StrategyProfile::load_merged(&strategy_profile_paths)?)
        };
        let paper_mode = parse_bool("PM_BTC_5M_PAPER_MODE", true)?;
        if !paper_mode
            && strategy_profile.is_none()
            && strategy_name
                .split([',', '+'])
                .map(str::trim)
                .any(|name| !name.is_empty() && name != "noop")
        {
            anyhow::bail!(
                "live strategy `{strategy_name}` requires PM_BTC_5M_STRATEGY_PROFILE_PATH or PM_BTC_5M_STRATEGY_PROFILE_PATHS"
            );
        }
        let log_level = env_or("RUST_LOG", "info");
        let log_format = parse_log_format(&env_or("PM_BTC_5M_EXEC_LOG_FORMAT", "pretty"))?;
        let metrics_bind = parse_socket_addr("PM_BTC_5M_EXEC_METRICS_BIND", "0.0.0.0:9108")?;
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
            crate::wire::relayer::DEFAULT_PUSD_ADDRESS,
        );
        let ctf_collateral_token_address = env_or(
            "POLYMARKET_CTF_COLLATERAL_TOKEN_ADDRESS",
            &collateral_token_address,
        );
        let collateral_decimals = parse_usize("POLYMARKET_COLLATERAL_DECIMALS", 6)? as u8;
        let proxy_wallet_address = env::var("POLYMARKET_PROXY_WALLET_ADDRESS")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let polygon_rpc_url = env::var("POLYGON_RPC_URL")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|value| !value.is_empty());
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
            "PM_BTC_5M_EXEC_SPOT_WS_URL",
            "wss://stream.binance.com:9443/ws/btcusdt@aggTrade",
        );
        let spot_rest_bootstrap_url = parse_optional_url_with_default(
            "PM_BTC_5M_EXEC_SPOT_REST_BOOTSTRAP_URL",
            DEFAULT_BINANCE_REST_BOOTSTRAP_URL,
        );
        let coinbase_spot_ws_url = parse_optional_url_with_default(
            "PM_BTC_5M_EXEC_COINBASE_SPOT_WS_URL",
            DEFAULT_COINBASE_SPOT_WS_URL,
        );
        let spot_symbol = env_or("PM_BTC_5M_EXEC_SPOT_SYMBOL", "BTCUSDT");
        let market_discovery_enabled = parse_bool("PM_BTC_5M_MARKET_DISCOVERY_ENABLED", false)?;
        let market_assets = if market_discovery_enabled {
            split_csv_optional("PM_BTC_5M_ASSET_IDS")
        } else {
            split_csv_required("PM_BTC_5M_ASSET_IDS")?
        };
        let user_markets = split_csv_optional("PM_BTC_5M_USER_MARKETS");
        let market_discovery_interval =
            parse_duration_ms("PM_BTC_5M_MARKET_DISCOVERY_INTERVAL_MS", 30_000)?;
        let market_discovery_window =
            parse_duration_ms("PM_BTC_5M_MARKET_DISCOVERY_WINDOW_MS", 5 * 60 * 1_000)?;
        let market_discovery_include_prev =
            parse_usize("PM_BTC_5M_MARKET_DISCOVERY_INCLUDE_PREV", 0)?;
        let market_discovery_include_next =
            parse_usize("PM_BTC_5M_MARKET_DISCOVERY_INCLUDE_NEXT", 0)?;
        let market_discovery_gamma_url = env_or(
            "PM_BTC_5M_MARKET_DISCOVERY_GAMMA_URL",
            "https://gamma-api.polymarket.com/markets",
        );
        let market_discovery_slug_prefix =
            env_or("PM_BTC_5M_MARKET_DISCOVERY_SLUG_PREFIX", "btc-updown-5m-");
        let market_discovery_families =
            parse_market_discovery_families("PM_BTC_5M_MARKET_DISCOVERY_FAMILIES")?;
        let runtime_loop_interval = parse_duration_ms("PM_BTC_5M_EXEC_LOOP_INTERVAL_MS", 1_000)?;
        let summary_log_interval = parse_duration_ms("PM_BTC_5M_EXEC_SUMMARY_INTERVAL_MS", 10_000)?;
        let order_reconcile_interval =
            parse_duration_ms("PM_BTC_5M_ORDER_RECONCILE_INTERVAL_MS", 15_000)?;
        let order_reconcile_stale_window =
            parse_duration_ms("PM_BTC_5M_ORDER_RECONCILE_STALE_MS", 30_000)?;
        let runtime_checkpoint_interval =
            parse_duration_ms("PM_BTC_5M_RUNTIME_CHECKPOINT_INTERVAL_MS", 30_000)?;
        let order_store_path = parse_path_optional("PM_BTC_5M_ORDER_STORE_PATH");
        let runtime_run_id =
            env_value("PM_BTC_5M_RUNTIME_RUN_ID").filter(|value| !value.trim().is_empty());
        let book_stale_after = parse_duration_ms_or_profile(
            "PM_BTC_5M_EXEC_BOOK_STALE_MS",
            strategy_profile
                .as_ref()
                .and_then(|profile| profile.risk.book_stale_ms),
            2_000,
        )?;
        let ping_interval = parse_duration_ms("PM_BTC_5M_EXEC_PING_INTERVAL_MS", 10_000)?;
        let spot_ws_conn_stale_timeout =
            parse_duration_ms("PM_BTC_5M_EXEC_SPOT_WS_CONN_STALE_TIMEOUT_MS", 30_000)?;
        let spot_ws_data_stale_timeout =
            parse_duration_ms("PM_BTC_5M_EXEC_SPOT_WS_DATA_STALE_TIMEOUT_MS", 30_000)?;
        let market_context_path = parse_path_optional("PM_BTC_5M_EXEC_MARKET_CONTEXT_PATH");
        let journal_path = parse_path_optional("PM_BTC_5M_EXEC_JOURNAL_PATH");
        let journal_rotate_bytes = env_value("PM_BTC_5M_EXEC_JOURNAL_ROTATE_BYTES")
            .filter(|value| !value.trim().is_empty())
            .map(|value| {
                value.parse::<u64>().with_context(|| {
                    format!(
                        "failed to parse PM_BTC_5M_EXEC_JOURNAL_ROTATE_BYTES as u64 from `{value}`"
                    )
                })
            })
            .transpose()?;
        let starting_cash_usd = parse_f64("PM_BTC_5M_EXEC_STARTING_CASH_USD", 0.0)?;
        let event_log_capacity = parse_usize("PM_BTC_5M_EXEC_EVENT_LOG_CAPACITY", 4_096)?;
        let market_id_by_asset =
            parse_asset_market_map(&env_value("PM_BTC_5M_INSTRUMENT_MARKETS").unwrap_or_default())?;
        let profile_inventory = strategy_profile.as_ref().map(|profile| &profile.inventory);
        let risk_limits = RiskLimits {
            max_order_notional_usd: parse_f64_or_profile(
                "PM_BTC_5M_EXEC_MAX_ORDER_NOTIONAL_USD",
                profile_inventory.and_then(|profile| profile.max_order_notional_usd),
                250.0,
            )?,
            max_gross_notional_usd: parse_f64_or_profile(
                "PM_BTC_5M_EXEC_MAX_GROSS_NOTIONAL_USD",
                profile_inventory.and_then(|profile| profile.max_gross_notional_usd),
                1_000.0,
            )?,
            max_net_notional_per_market_usd: parse_f64_or_profile(
                "PM_BTC_5M_EXEC_MAX_NET_NOTIONAL_PER_MARKET_USD",
                profile_inventory.and_then(|profile| profile.max_net_notional_per_market_usd),
                500.0,
            )?,
            max_position_quantity_per_instrument: parse_f64_or_profile(
                "PM_BTC_5M_EXEC_MAX_POSITION_QTY_PER_INSTRUMENT",
                profile_inventory.and_then(|profile| profile.max_position_quantity_per_instrument),
                10_000.0,
            )?,
            min_free_cash_usd: parse_f64_or_profile(
                "PM_BTC_5M_EXEC_MIN_FREE_CASH_USD",
                profile_inventory.and_then(|profile| profile.min_free_cash_usd),
                0.0,
            )?,
            min_free_cash_bps: parse_f64_or_profile(
                "PM_BTC_5M_EXEC_MIN_FREE_CASH_BPS",
                profile_inventory.and_then(|profile| profile.min_free_cash_bps),
                0.0,
            )?,
            min_portfolio_equity_usd: parse_f64_or_profile(
                "PM_BTC_5M_EXEC_MIN_PORTFOLIO_EQUITY_USD",
                profile_inventory.and_then(|profile| profile.min_portfolio_equity_usd),
                0.0,
            )?,
            min_portfolio_equity_bps: parse_f64_or_profile(
                "PM_BTC_5M_EXEC_MIN_PORTFOLIO_EQUITY_BPS",
                profile_inventory.and_then(|profile| profile.min_portfolio_equity_bps),
                0.0,
            )?,
            max_session_loss_usd: parse_f64_or_profile(
                "PM_BTC_5M_EXEC_MAX_SESSION_LOSS_USD",
                profile_inventory.and_then(|profile| profile.max_session_loss_usd),
                0.0,
            )?,
            max_session_loss_bps: parse_f64_or_profile(
                "PM_BTC_5M_EXEC_MAX_SESSION_LOSS_BPS",
                profile_inventory.and_then(|profile| profile.max_session_loss_bps),
                0.0,
            )?,
            max_open_orders_total: parse_usize_or_profile(
                "PM_BTC_5M_EXEC_MAX_OPEN_ORDERS_TOTAL",
                profile_inventory.and_then(|profile| profile.max_open_orders_total),
                32,
            )?,
            max_open_orders_per_market: parse_usize_or_profile(
                "PM_BTC_5M_EXEC_MAX_OPEN_ORDERS_PER_MARKET",
                profile_inventory.and_then(|profile| profile.max_open_orders_per_market),
                8,
            )?,
        };
        let user_auth = load_user_auth();
        let dashboard_whale_events_path =
            parse_path_optional("PM_BTC_5M_DASHBOARD_WHALE_EVENTS_PATH");
        let dashboard_refresh_ms =
            parse_duration_ms("PM_BTC_5M_DASHBOARD_REFRESH_MS", 2_000)?.as_millis() as u64;
        let dashboard_event_limit = parse_usize("PM_BTC_5M_DASHBOARD_EVENT_LIMIT", 200)?;
        let audit_path = parse_path_optional("PM_BTC_5M_EXEC_AUDIT_PATH");
        let live_post_only = parse_bool("PM_BTC_5M_LIVE_POST_ONLY", true)?;
        let live_order_ttl = parse_duration_ms("PM_BTC_5M_LIVE_ORDER_TTL_MS", 20_000)?;
        let live_order_max_age = parse_duration_ms("PM_BTC_5M_LIVE_ORDER_MAX_AGE_MS", 25_000)?;
        let live_reconcile_missing_grace =
            parse_duration_ms("PM_BTC_5M_LIVE_RECONCILE_MISSING_GRACE_MS", 5_000)?;
        let quote_min_order_age_default_ms = if paper_mode { 750 } else { 5_000 };
        let configured_quote_min_order_age = parse_duration_ms(
            "PM_BTC_5M_QUOTE_MIN_ORDER_AGE_MS",
            quote_min_order_age_default_ms,
        )?;
        let quote_min_order_age =
            effective_quote_min_order_age(paper_mode, configured_quote_min_order_age);
        let quote_churn_window = parse_duration_ms("PM_BTC_5M_QUOTE_CHURN_WINDOW_MS", 20_000)?;
        let quote_hard_pull = parse_duration_ms("PM_BTC_5M_QUOTE_HARD_PULL_MS", 5_000)?;
        let quote_max_churn_per_window = parse_usize("PM_BTC_5M_QUOTE_MAX_CHURN_PER_WINDOW", 12)?;
        let quote_max_submit_per_window = parse_usize("PM_BTC_5M_QUOTE_MAX_SUBMIT_PER_WINDOW", 6)?;
        let quote_max_replace_per_window =
            parse_usize("PM_BTC_5M_QUOTE_MAX_REPLACE_PER_WINDOW", 4)?;
        let quote_max_cancel_per_window = parse_usize("PM_BTC_5M_QUOTE_MAX_CANCEL_PER_WINDOW", 12)?;
        let live_max_submit_errors = parse_usize("PM_BTC_5M_LIVE_MAX_SUBMIT_ERRORS", 1)?;
        let live_max_cancel_errors = parse_usize("PM_BTC_5M_LIVE_MAX_CANCEL_ERRORS", 1)?;
        let live_kill_on_reconcile_mismatch =
            parse_bool("PM_BTC_5M_LIVE_KILL_ON_RECONCILE_MISMATCH", true)?;
        let live_kill_switch_path = parse_path_optional("PM_BTC_5M_LIVE_KILL_SWITCH_PATH");
        let live_pusd_auto_wrap = parse_bool("PM_BTC_5M_LIVE_PUSD_AUTO_WRAP", false)?;
        let live_pusd_auto_wrap_min_usd = parse_f64("PM_BTC_5M_LIVE_PUSD_AUTO_WRAP_MIN_USD", 0.01)?;
        let live_risk_off_auto_recover =
            parse_duration_ms("PM_BTC_5M_LIVE_RISK_OFF_AUTO_RECOVER_MS", 30_000)?;
        let paper_min_fill_notional_usd = parse_f64("PM_BTC_5M_PAPER_MIN_FILL_NOTIONAL_USD", 0.05)?;
        let paper_max_fills_per_order = parse_usize("PM_BTC_5M_PAPER_MAX_FILLS_PER_ORDER", 3)?;
        let paper_min_fill_interval =
            parse_duration_ms("PM_BTC_5M_PAPER_MIN_FILL_INTERVAL_MS", 750)?;
        let paper_market_close_at_ms = env_value("PM_BTC_5M_PAPER_MARKET_CLOSE_AT_MS")
            .filter(|v| !v.trim().is_empty())
            .map(|v| {
                v.trim().parse::<u64>().map_err(|err| {
                    anyhow::anyhow!("invalid PM_BTC_5M_PAPER_MARKET_CLOSE_AT_MS: {err}")
                })
            })
            .transpose()?;
        let paper_market_resolution_price =
            env_value("PM_BTC_5M_PAPER_MARKET_RESOLUTION_PRICE")
                .filter(|v| !v.trim().is_empty())
                .map(|v| {
                    v.trim()
                        .parse::<f64>()
                        .map_err(|err| anyhow::anyhow!(
                            "invalid PM_BTC_5M_PAPER_MARKET_RESOLUTION_PRICE: {err}"
                        ))
                        .and_then(|p| {
                            if (0.0..=1.0).contains(&p) {
                                Ok(p)
                            } else {
                                Err(anyhow::anyhow!(
                                    "PM_BTC_5M_PAPER_MARKET_RESOLUTION_PRICE must be in [0.0, 1.0], got {p}"
                                ))
                            }
                        })
                })
                .transpose()?;
        let paper_submit_latency_ms =
            parse_duration_ms("PM_BTC_5M_PAPER_SUBMIT_LATENCY_MS", 150)?.as_millis() as u64;
        let paper_queue_depth_fraction = parse_f64("PM_BTC_5M_PAPER_QUEUE_DEPTH_FRACTION", 0.75)?;
        if !(0.0..=1.0).contains(&paper_queue_depth_fraction) {
            anyhow::bail!(
                "PM_BTC_5M_PAPER_QUEUE_DEPTH_FRACTION must be in [0.0, 1.0], got {paper_queue_depth_fraction}"
            );
        }
        let paper_post_only_reject_probability =
            parse_f64("PM_BTC_5M_PAPER_POST_ONLY_REJECT_PROBABILITY", 0.85)?;
        if !(0.0..=1.0).contains(&paper_post_only_reject_probability) {
            anyhow::bail!(
                "PM_BTC_5M_PAPER_POST_ONLY_REJECT_PROBABILITY must be in [0.0, 1.0], got {paper_post_only_reject_probability}"
            );
        }
        let paper_cancel_race_window_ms =
            parse_duration_ms("PM_BTC_5M_PAPER_CANCEL_RACE_WINDOW_MS", 500)?.as_millis() as u64;
        let paper_report_path = parse_path_optional("PM_BTC_5M_PAPER_REPORT_PATH");
        let book_snapshot_log_path = parse_path_optional("PM_BTC_5M_BOOK_SNAPSHOT_LOG_PATH");
        let shadow_quote_log_path = parse_path_optional("PM_BTC_5M_SHADOW_QUOTE_LOG_PATH");
        let book_snapshot_max_levels = parse_usize("PM_BTC_5M_BOOK_SNAPSHOT_MAX_LEVELS", 10)?;
        let paper_maker_rebate_coeff = parse_f64("PM_BTC_5M_PAPER_MAKER_REBATE_COEFF", 0.0)?;
        let paper_taker_fee_coeff_override = env_value("PM_BTC_5M_PAPER_TAKER_FEE_COEFF")
            .filter(|v| !v.trim().is_empty())
            .map(|v| v.trim().parse::<f64>())
            .transpose()
            .map_err(|err| anyhow::anyhow!("invalid PM_BTC_5M_PAPER_TAKER_FEE_COEFF: {err}"))?;

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
            ctf_collateral_token_address,
            collateral_token_address,
            collateral_decimals,
            proxy_wallet_address,
            polygon_rpc_url,
            market_ws_url,
            user_ws_url,
            spot_ws_url,
            spot_rest_bootstrap_url,
            coinbase_spot_ws_url,
            spot_symbol,
            market_assets,
            user_markets,
            market_discovery_enabled,
            market_discovery_interval,
            market_discovery_window,
            market_discovery_include_prev,
            market_discovery_include_next,
            market_discovery_gamma_url,
            market_discovery_slug_prefix,
            market_discovery_families,
            runtime_loop_interval,
            summary_log_interval,
            order_reconcile_interval,
            order_reconcile_stale_window,
            runtime_checkpoint_interval,
            book_stale_after,
            order_store_path,
            runtime_run_id,
            ping_interval,
            spot_ws_conn_stale_timeout,
            spot_ws_data_stale_timeout,
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
            quote_churn_window,
            quote_hard_pull,
            quote_max_churn_per_window,
            quote_max_submit_per_window,
            quote_max_replace_per_window,
            quote_max_cancel_per_window,
            live_max_submit_errors,
            live_max_cancel_errors,
            live_kill_on_reconcile_mismatch,
            live_kill_switch_path,
            live_pusd_auto_wrap,
            live_pusd_auto_wrap_min_usd,
            live_risk_off_auto_recover,
            paper_min_fill_notional_usd,
            paper_max_fills_per_order,
            paper_min_fill_interval,
            paper_market_close_at_ms,
            paper_market_resolution_price,
            paper_submit_latency_ms,
            paper_queue_depth_fraction,
            paper_post_only_reject_probability,
            paper_cancel_race_window_ms,
            paper_report_path,
            book_snapshot_log_path,
            shadow_quote_log_path,
            book_snapshot_max_levels,
            paper_maker_rebate_coeff,
            paper_taker_fee_coeff_override,
        })
    }

    pub fn market_id_for_asset(&self, asset_id: &str) -> String {
        self.market_id_by_asset
            .get(asset_id)
            .cloned()
            .unwrap_or_else(|| asset_id.to_string())
    }
}

fn should_load_dotenv() -> bool {
    match env_value("PM_BTC_5M_EXEC_LOAD_DOTENV") {
        Some(value) => !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        ),
        None => true,
    }
}

fn parse_optional_url_with_default(key: &str, default: &str) -> Option<String> {
    env_value(key)
        .map(|v| v.trim().to_string())
        .and_then(|value| {
            let lowered = value.to_ascii_lowercase();
            if value.is_empty() || matches!(lowered.as_str(), "0" | "false" | "off" | "none") {
                None
            } else {
                Some(value)
            }
        })
        .or_else(|| Some(default.to_string()))
}

fn parse_strategy_profile_paths() -> Vec<PathBuf> {
    env_value("PM_BTC_5M_STRATEGY_PROFILE_PATHS")
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|path| !path.is_empty())
                .map(PathBuf::from)
                .collect::<Vec<_>>()
        })
        .filter(|paths| !paths.is_empty())
        .or_else(|| parse_path_optional("PM_BTC_5M_STRATEGY_PROFILE_PATH").map(|path| vec![path]))
        .unwrap_or_default()
}

fn parse_duration_ms_or_profile(key: &str, profile: Option<u64>, default: u64) -> Result<Duration> {
    if env_value(key).is_some() {
        parse_duration_ms(key, default)
    } else {
        Ok(Duration::from_millis(profile.unwrap_or(default)))
    }
}

fn effective_quote_min_order_age(paper_mode: bool, configured: Duration) -> Duration {
    if paper_mode {
        configured
    } else {
        configured.max(Duration::from_millis(5_000))
    }
}

fn parse_f64_or_profile(key: &str, profile: Option<f64>, default: f64) -> Result<f64> {
    if env_value(key).is_some() {
        parse_f64(key, default)
    } else {
        Ok(profile.unwrap_or(default))
    }
}

fn parse_usize_or_profile(key: &str, profile: Option<usize>, default: usize) -> Result<usize> {
    if env_value(key).is_some() {
        parse_usize(key, default)
    } else {
        Ok(profile.unwrap_or(default))
    }
}

fn parse_market_discovery_families(key: &str) -> Result<Vec<MarketDiscoveryFamily>> {
    let Some(raw) = env_value(key) else {
        return Ok(Vec::new());
    };
    let mut families = Vec::new();
    for entry in raw.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let (prefix, window_raw) = entry
            .rsplit_once(':')
            .with_context(|| format!("{key} entry `{entry}` missing `:window_ms` suffix"))?;
        let prefix = prefix.trim();
        if prefix.is_empty() {
            anyhow::bail!("{key} entry `{entry}` has empty prefix");
        }
        let window_ms: u64 = window_raw.trim().parse().with_context(|| {
            format!("{key} entry `{entry}` has non-integer window_ms `{window_raw}`")
        })?;
        if window_ms == 0 {
            anyhow::bail!("{key} entry `{entry}` has zero window_ms");
        }
        families.push(MarketDiscoveryFamily {
            prefix: prefix.to_string(),
            window: Duration::from_millis(window_ms),
        });
    }
    Ok(families)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_csv_families_with_per_entry_window() {
        std::env::set_var(
            "TEST_FAMILIES_OK",
            "btc-updown-5m-:300000,eth-updown-5m-:300000,btc-updown-15m-:900000",
        );
        let families = parse_market_discovery_families("TEST_FAMILIES_OK").unwrap();
        std::env::remove_var("TEST_FAMILIES_OK");
        assert_eq!(families.len(), 3);
        assert_eq!(families[0].prefix, "btc-updown-5m-");
        assert_eq!(families[0].window, Duration::from_millis(300_000));
        assert_eq!(families[2].window, Duration::from_millis(900_000));
    }

    #[test]
    fn returns_empty_when_unset() {
        std::env::remove_var("TEST_FAMILIES_UNSET_X");
        let families = parse_market_discovery_families("TEST_FAMILIES_UNSET_X").unwrap();
        assert!(families.is_empty());
    }

    #[test]
    fn rejects_entry_without_window_separator() {
        std::env::set_var("TEST_FAMILIES_BAD", "btc-updown-5m-");
        let err = parse_market_discovery_families("TEST_FAMILIES_BAD").unwrap_err();
        std::env::remove_var("TEST_FAMILIES_BAD");
        assert!(err.to_string().contains("missing `:window_ms`"));
    }

    #[test]
    fn rejects_zero_window() {
        std::env::set_var("TEST_FAMILIES_ZERO", "btc-updown-5m-:0");
        let err = parse_market_discovery_families("TEST_FAMILIES_ZERO").unwrap_err();
        std::env::remove_var("TEST_FAMILIES_ZERO");
        assert!(err.to_string().contains("zero window_ms"));
    }

    #[test]
    fn dotenv_loading_can_be_disabled_for_service_envs() {
        std::env::set_var("PM_BTC_5M_EXEC_LOAD_DOTENV", "false");
        assert!(!should_load_dotenv());
        std::env::set_var("PM_BTC_5M_EXEC_LOAD_DOTENV", "0");
        assert!(!should_load_dotenv());
        std::env::set_var("PM_BTC_5M_EXEC_LOAD_DOTENV", "true");
        assert!(should_load_dotenv());
        std::env::remove_var("PM_BTC_5M_EXEC_LOAD_DOTENV");
    }

    #[test]
    fn live_quote_min_order_age_has_rebate_scoring_floor() {
        assert_eq!(
            effective_quote_min_order_age(false, Duration::from_millis(250)),
            Duration::from_millis(5_000)
        );
        assert_eq!(
            effective_quote_min_order_age(false, Duration::from_millis(8_000)),
            Duration::from_millis(8_000)
        );
    }

    #[test]
    fn paper_quote_min_order_age_keeps_fast_replay_setting() {
        assert_eq!(
            effective_quote_min_order_age(true, Duration::from_millis(250)),
            Duration::from_millis(250)
        );
    }
}
