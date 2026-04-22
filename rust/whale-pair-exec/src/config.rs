use std::env;
use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{bail, Context, Result};

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
            user_auth,
        })
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

#[cfg(test)]
mod tests {
    use super::{parse_log_format, LogFormat};

    #[test]
    fn parses_json_log_format() {
        assert!(matches!(parse_log_format("json").unwrap(), LogFormat::Json));
    }

    #[test]
    fn rejects_unknown_log_format() {
        assert!(parse_log_format("xml").is_err());
    }
}
