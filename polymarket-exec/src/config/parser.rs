//! Typed environment parsing helpers with validation for runtime config inputs.

use std::collections::HashMap;
use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::config::LogFormat;
use crate::config::UserWsAuth;

pub fn env_or(key: &str, default: &str) -> String {
    env_value(key).unwrap_or_else(|| default.to_string())
}

pub fn parse_bool(key: &str, default_value: bool) -> Result<bool> {
    let raw = env_value(key).unwrap_or_else(|| default_value.to_string());
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
    let raw = env_value(key).unwrap_or_else(|| default_ms.to_string());
    let value: u64 = raw
        .parse()
        .with_context(|| format!("failed to parse {key} as integer milliseconds"))?;
    Ok(Duration::from_millis(value))
}

pub fn parse_path_optional(key: &str) -> Option<PathBuf> {
    let value = env_value(key)?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(PathBuf::from(trimmed))
}

pub fn parse_f64(key: &str, default_value: f64) -> Result<f64> {
    let raw = env_value(key).unwrap_or_else(|| default_value.to_string());
    raw.parse()
        .with_context(|| format!("failed to parse {key} as floating point number"))
}

pub fn parse_usize(key: &str, default_value: usize) -> Result<usize> {
    let raw = env_value(key).unwrap_or_else(|| default_value.to_string());
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
    env_value(key)
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

pub fn env_value(key: &str) -> Option<String> {
    env::var(key).ok().filter(|value| !value.trim().is_empty())
}

fn lookup_first_env<F>(lookup: &F, keys: &[&str]) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    keys.iter()
        .find_map(|key| lookup(key))
        .filter(|value| !value.trim().is_empty())
}

fn load_user_auth_from_lookup<F>(lookup: F) -> Option<UserWsAuth>
where
    F: Fn(&str) -> Option<String>,
{
    let api_key = lookup_first_env(&lookup, &["POLYMARKET_API_KEY"])?;
    let api_secret = lookup_first_env(&lookup, &["POLYMARKET_API_SECRET", "POLYMARKET_SECRET"])?;
    let api_passphrase = lookup_first_env(
        &lookup,
        &["POLYMARKET_API_PASSPHRASE", "POLYMARKET_PASSPHRASE"],
    )?;
    if api_key.trim().is_empty() || api_secret.trim().is_empty() || api_passphrase.trim().is_empty()
    {
        return None;
    }
    Some(UserWsAuth {
        api_key,
        api_secret,
        api_passphrase,
        private_key: lookup_first_env(&lookup, &["POLYMARKET_PRIVATE_KEY"]),
        signature_type: lookup_first_env(&lookup, &["POLYMARKET_SIGNATURE_TYPE"]),
        funder_address: lookup_first_env(
            &lookup,
            &["POLYMARKET_FUNDER_ADDRESS", "POLYMARKET_FUNDER"],
        ),
    })
}

pub fn load_user_auth() -> Option<UserWsAuth> {
    load_user_auth_from_lookup(|key| env::var(key).ok())
}

pub fn parse_asset_market_map(raw: &str) -> Result<HashMap<String, String>> {
    let mut mapping = HashMap::new();
    for pair in raw
        .split(',')
        .map(str::trim)
        .filter(|pair| !pair.is_empty())
    {
        let (asset_id, market_id) = pair.split_once(':').with_context(|| {
            format!(
                "failed to parse PM_BTC_5M_INSTRUMENT_MARKETS entry `{pair}` as asset_id:market_id"
            )
        })?;
        let asset_id = asset_id.trim();
        let market_id = market_id.trim();
        if asset_id.is_empty() || market_id.is_empty() {
            bail!(
                "PM_BTC_5M_INSTRUMENT_MARKETS entry `{pair}` must have non-empty asset_id and market_id"
            );
        }
        mapping.insert(asset_id.to_string(), market_id.to_string());
    }
    Ok(mapping)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{
        load_user_auth_from_lookup, parse_asset_market_map, parse_bool, parse_log_format, LogFormat,
    };

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
        assert!(parse_bool("PM_BTC_5M_PAPER_MODE_MISSING", true).unwrap());
    }

    #[test]
    fn parses_bool_from_text() {
        assert!(!parse_bool("PM_BTC_5M_PAPER_MODE_NO", false).unwrap());
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

    #[test]
    fn loads_user_auth_from_canonical_env_names() {
        let values = HashMap::from([
            ("POLYMARKET_API_KEY", "key"),
            ("POLYMARKET_API_SECRET", "secret"),
            ("POLYMARKET_API_PASSPHRASE", "passphrase"),
        ]);

        let auth = load_user_auth_from_lookup(|key| values.get(key).map(|value| value.to_string()))
            .expect("canonical credentials");

        assert_eq!(auth.api_key, "key");
        assert_eq!(auth.api_secret, "secret");
        assert_eq!(auth.api_passphrase, "passphrase");
    }

    #[test]
    fn loads_user_auth_from_polymarket_sdk_aliases() {
        let values = HashMap::from([
            ("POLYMARKET_API_KEY", "key"),
            ("POLYMARKET_SECRET", "secret"),
            ("POLYMARKET_PASSPHRASE", "passphrase"),
            ("POLYMARKET_FUNDER", "0xfunder"),
        ]);

        let auth = load_user_auth_from_lookup(|key| values.get(key).map(|value| value.to_string()))
            .expect("alias credentials");

        assert_eq!(auth.api_secret, "secret");
        assert_eq!(auth.api_passphrase, "passphrase");
        assert_eq!(auth.funder_address.as_deref(), Some("0xfunder"));
    }

    #[test]
    fn canonical_user_auth_names_take_precedence_over_aliases() {
        let values = HashMap::from([
            ("POLYMARKET_API_KEY", "key"),
            ("POLYMARKET_API_SECRET", "canonical-secret"),
            ("POLYMARKET_SECRET", "alias-secret"),
            ("POLYMARKET_API_PASSPHRASE", "canonical-passphrase"),
            ("POLYMARKET_PASSPHRASE", "alias-passphrase"),
        ]);

        let auth = load_user_auth_from_lookup(|key| values.get(key).map(|value| value.to_string()))
            .expect("credentials");

        assert_eq!(auth.api_secret, "canonical-secret");
        assert_eq!(auth.api_passphrase, "canonical-passphrase");
    }
}
