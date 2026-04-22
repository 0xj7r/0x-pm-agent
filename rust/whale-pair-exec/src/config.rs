use std::collections::HashMap;
use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};

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
    pub journal_path: Option<PathBuf>,
    pub starting_cash_usd: f64,
    pub event_log_capacity: usize,
    pub market_id_by_asset: HashMap<String, String>,
    pub risk_limits: RiskLimits,
    pub user_auth: Option<UserWsAuth>,
}

impl AppConfig {
    pub fn from_env() -> Result<Self> {
        let _ = dotenvy::dotenv();

        let service_name = env_or("WHALE_PAIR_EXEC_SERVICE_NAME", "whale-pair-exec");
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

        Ok(Self {
            service_name,
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
            journal_path,
            starting_cash_usd,
            event_log_capacity,
            market_id_by_asset,
            risk_limits,
            user_auth,
        })
    }

    pub fn market_id_for_asset(&self, asset_id: &str) -> String {
        self.market_id_by_asset
            .get(asset_id)
            .cloned()
            .unwrap_or_else(|| asset_id.to_string())
    }
}

fn env_or(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
}

fn parse_log_format(raw: &str) -> Result<LogFormat> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "pretty" | "text" | "compact" => Ok(LogFormat::Pretty),
        "json" => Ok(LogFormat::Json),
        other => bail!("unsupported log format `{other}`"),
    }
}

fn parse_socket_addr(key: &str, default: &str) -> Result<SocketAddr> {
    env_or(key, default)
        .parse()
        .with_context(|| format!("failed to parse {key} as host:port"))
}

fn parse_duration_ms(key: &str, default_ms: u64) -> Result<Duration> {
    let raw = env::var(key).unwrap_or_else(|_| default_ms.to_string());
    let value: u64 = raw
        .parse()
        .with_context(|| format!("failed to parse {key} as integer milliseconds"))?;
    Ok(Duration::from_millis(value))
}

fn parse_path_optional(key: &str) -> Option<PathBuf> {
    let value = env::var(key).ok()?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(PathBuf::from(trimmed))
}

fn parse_f64(key: &str, default_value: f64) -> Result<f64> {
    let raw = env::var(key).unwrap_or_else(|_| default_value.to_string());
    raw.parse()
        .with_context(|| format!("failed to parse {key} as floating point number"))
}

fn parse_usize(key: &str, default_value: usize) -> Result<usize> {
    let raw = env::var(key).unwrap_or_else(|_| default_value.to_string());
    raw.parse()
        .with_context(|| format!("failed to parse {key} as non-negative integer"))
}

fn split_csv_required(key: &str) -> Result<Vec<String>> {
    let values = split_csv_optional(key);
    if values.is_empty() {
        bail!("{key} must contain at least one comma-separated identifier");
    }
    Ok(values)
}

fn split_csv_optional(key: &str) -> Vec<String> {
    env::var(key)
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn load_user_auth() -> Option<UserWsAuth> {
    let api_key = env::var("POLYMARKET_API_KEY").ok()?;
    let api_secret = env::var("POLYMARKET_API_SECRET").ok()?;
    let api_passphrase = env::var("POLYMARKET_API_PASSPHRASE").ok()?;
    if api_key.trim().is_empty() || api_secret.trim().is_empty() || api_passphrase.trim().is_empty() {
        return None;
    }
    Some(UserWsAuth {
        api_key,
        api_secret,
        api_passphrase,
    })
}

fn parse_asset_market_map(raw: &str) -> Result<HashMap<String, String>> {
    let mut mapping = HashMap::new();
    for pair in raw.split(',').map(str::trim).filter(|pair| !pair.is_empty()) {
        let (asset_id, market_id) = pair
            .split_once(':')
            .with_context(|| {
                format!(
                    "failed to parse WHALE_PAIR_INSTRUMENT_MARKETS entry `{pair}` as asset_id:market_id"
                )
            })?;
        let asset_id = asset_id.trim();
        let market_id = market_id.trim();
        if asset_id.is_empty() || market_id.is_empty() {
            bail!(
                "WHALE_PAIR_INSTRUMENT_MARKETS entry `{pair}` must have non-empty asset_id and market_id"
            );
        }
        mapping.insert(asset_id.to_string(), market_id.to_string());
    }
    Ok(mapping)
}

#[cfg(test)]
mod tests {
    use super::{parse_asset_market_map, parse_log_format, LogFormat};

    #[test]
    fn parses_json_log_format() {
        assert!(matches!(parse_log_format("json").unwrap(), LogFormat::Json));
    }

    #[test]
    fn rejects_unknown_log_format() {
        assert!(parse_log_format("xml").is_err());
    }

    #[test]
    fn parses_asset_market_mapping() {
        let mapping = parse_asset_market_map("token-up:market-1,token-down:market-1").unwrap();
        assert_eq!(mapping.get("token-up").unwrap(), "market-1");
        assert_eq!(mapping.get("token-down").unwrap(), "market-1");
    }

    #[test]
    fn rejects_bad_asset_market_mapping() {
        assert!(parse_asset_market_map("token-up").is_err());
    }
}
