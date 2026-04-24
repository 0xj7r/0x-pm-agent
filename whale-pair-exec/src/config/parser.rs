use std::collections::HashMap;
use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::config::LogFormat;
use crate::config::UserWsAuth;

pub fn env_or(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
}

pub fn parse_bool(key: &str, default_value: bool) -> Result<bool> {
    let raw = env::var(key).unwrap_or_else(|_| default_value.to_string());
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "on" | "yes" | "y" => Ok(true),
        "0" | "false" | "off" | "no" | "n" => Ok(false),
        other => bail!(
            "failed to parse {key} as bool (accepted: 0/1, true/false, on/off, yes/no): {other}"
        ),
    }
}

pub fn parse_log_format(raw: &str) -> Result<LogFormat> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "pretty" | "text" | "compact" => Ok(LogFormat::Pretty),
        "json" => Ok(LogFormat::Json),
        other => bail!("unsupported log format `{other}`"),
    }
}

pub fn parse_socket_addr(key: &str, default: &str) -> Result<SocketAddr> {
    env_or(key, default)
        .parse()
        .with_context(|| format!("failed to parse {key} as host:port"))
}

pub fn parse_duration_ms(key: &str, default_ms: u64) -> Result<Duration> {
    let raw = env::var(key).unwrap_or_else(|_| default_ms.to_string());
    let value: u64 = raw
        .parse()
        .with_context(|| format!("failed to parse {key} as integer milliseconds"))?;
    Ok(Duration::from_millis(value))
}

pub fn parse_path_optional(key: &str) -> Option<PathBuf> {
    let value = env::var(key).ok()?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(PathBuf::from(trimmed))
}

pub fn parse_f64(key: &str, default_value: f64) -> Result<f64> {
    let raw = env::var(key).unwrap_or_else(|_| default_value.to_string());
    raw.parse()
        .with_context(|| format!("failed to parse {key} as floating point number"))
}

pub fn parse_usize(key: &str, default_value: usize) -> Result<usize> {
    let raw = env::var(key).unwrap_or_else(|_| default_value.to_string());
    raw.parse()
        .with_context(|| format!("failed to parse {key} as non-negative integer"))
}

pub fn split_csv_required(key: &str) -> Result<Vec<String>> {
    let values = split_csv_optional(key);
    if values.is_empty() {
        bail!("{key} must contain at least one comma-separated identifier");
    }
    Ok(values)
}

pub fn split_csv_optional(key: &str) -> Vec<String> {
    env::var(key)
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

pub fn load_user_auth() -> Option<UserWsAuth> {
    let api_key = env::var("POLYMARKET_API_KEY").ok()?;
    let api_secret = env::var("POLYMARKET_API_SECRET").ok()?;
    let api_passphrase = env::var("POLYMARKET_API_PASSPHRASE").ok()?;
    if api_key.trim().is_empty() || api_secret.trim().is_empty() || api_passphrase.trim().is_empty()
    {
        return None;
    }
    Some(UserWsAuth {
        api_key,
        api_secret,
        api_passphrase,
        private_key: env::var("POLYMARKET_PRIVATE_KEY")
            .ok()
            .filter(|value| !value.trim().is_empty()),
        signature_type: env::var("POLYMARKET_SIGNATURE_TYPE")
            .ok()
            .filter(|value| !value.trim().is_empty()),
        funder_address: env::var("POLYMARKET_FUNDER_ADDRESS")
            .ok()
            .filter(|value| !value.trim().is_empty()),
    })
}

pub fn parse_asset_market_map(raw: &str) -> Result<HashMap<String, String>> {
    let mut mapping = HashMap::new();
    for pair in raw
        .split(',')
        .map(str::trim)
        .filter(|pair| !pair.is_empty())
    {
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
    use super::{parse_asset_market_map, parse_bool, parse_log_format, LogFormat};

    #[test]
    fn parses_json_log_format() {
        assert!(matches!(parse_log_format("json").unwrap(), LogFormat::Json));
    }

    #[test]
    fn rejects_unknown_log_format() {
        assert!(parse_log_format("xml").is_err());
    }

    #[test]
    fn parses_missing_bool_with_default() {
        assert!(parse_bool("WHALE_PAIR_PAPER_MODE_MISSING", true).unwrap());
    }

    #[test]
    fn parses_bool_from_text() {
        assert!(!parse_bool("WHALE_PAIR_PAPER_MODE_NO", false).unwrap());
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
