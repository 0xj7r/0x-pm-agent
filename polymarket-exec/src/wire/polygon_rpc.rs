//! Polygon RPC endpoint policy for EOA CTF actions.
//!
//! CLOB trading is off-chain, but EOA merge/redeem/wrap actions are Polygon
//! transactions. This module keeps RPC handling explicit: one primary endpoint
//! for sends, optional failovers for read/preflight calls, and redacted logging
//! so provider keys do not leak into journals.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use serde_json::json;

use crate::wire::execution_adapter::ExecutionError;

pub const DEFAULT_POLYGON_RPC_REQUEST_TIMEOUT: Duration = Duration::from_millis(2_500);
pub const DEFAULT_POLYGON_RPC_RECEIPT_TIMEOUT: Duration = Duration::from_secs(60);
pub const DEFAULT_POLYGON_RPC_GAS_LIMIT: u64 = 250_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolygonRpcEndpoint {
    url: String,
}

impl PolygonRpcEndpoint {
    pub fn new(url: impl Into<String>) -> Option<Self> {
        let url = url.into().trim().to_string();
        if url.is_empty() {
            return None;
        }
        Some(Self { url })
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn redacted_url(&self) -> String {
        redact_rpc_url(&self.url)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolygonRpcConfig {
    endpoints: Vec<PolygonRpcEndpoint>,
    request_timeout: Duration,
    receipt_timeout: Duration,
    gas_limit: u64,
}

impl PolygonRpcConfig {
    pub fn new(primary_url: impl Into<String>) -> Self {
        Self::from_parts(
            primary_url,
            std::iter::empty::<String>(),
            DEFAULT_POLYGON_RPC_REQUEST_TIMEOUT,
            DEFAULT_POLYGON_RPC_RECEIPT_TIMEOUT,
            DEFAULT_POLYGON_RPC_GAS_LIMIT,
        )
    }

    pub fn from_primary_and_env(primary_url: impl Into<String>) -> Self {
        let failovers = std::env::var("POLYGON_RPC_FAILOVER_URLS").unwrap_or_default();
        let request_timeout = duration_ms_from_env(
            "POLYGON_RPC_REQUEST_TIMEOUT_MS",
            DEFAULT_POLYGON_RPC_REQUEST_TIMEOUT,
        );
        let receipt_timeout = duration_ms_from_env(
            "POLYGON_RPC_RECEIPT_TIMEOUT_MS",
            DEFAULT_POLYGON_RPC_RECEIPT_TIMEOUT,
        );
        let gas_limit = std::env::var("POLYGON_RPC_GAS_LIMIT")
            .ok()
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .filter(|value| *value > 21_000)
            .unwrap_or(DEFAULT_POLYGON_RPC_GAS_LIMIT);
        Self::from_parts(
            primary_url,
            failovers
                .split(',')
                .map(str::trim)
                .filter(|url| !url.is_empty())
                .map(ToOwned::to_owned),
            request_timeout,
            receipt_timeout,
            gas_limit,
        )
    }

    pub fn from_parts(
        primary_url: impl Into<String>,
        failover_urls: impl IntoIterator<Item = String>,
        request_timeout: Duration,
        receipt_timeout: Duration,
        gas_limit: u64,
    ) -> Self {
        let mut seen = BTreeSet::new();
        let mut endpoints = Vec::new();
        let primary_url = primary_url.into();
        for url in std::iter::once(primary_url).chain(failover_urls) {
            let trimmed = url.trim();
            if trimmed.is_empty() || !seen.insert(trimmed.to_string()) {
                continue;
            }
            if let Some(endpoint) = PolygonRpcEndpoint::new(trimmed) {
                endpoints.push(endpoint);
            }
        }
        Self {
            endpoints,
            request_timeout,
            receipt_timeout,
            gas_limit: gas_limit.max(21_000),
        }
    }

    pub fn primary(&self) -> Option<&PolygonRpcEndpoint> {
        self.endpoints.first()
    }

    pub fn primary_url(&self) -> Option<&str> {
        self.primary().map(PolygonRpcEndpoint::url)
    }

    pub fn endpoints(&self) -> &[PolygonRpcEndpoint] {
        &self.endpoints
    }

    pub fn request_timeout(&self) -> Duration {
        self.request_timeout
    }

    pub fn receipt_timeout(&self) -> Duration {
        self.receipt_timeout
    }

    pub fn gas_limit(&self) -> u64 {
        self.gas_limit
    }

    pub fn redacted_endpoints(&self) -> Vec<String> {
        self.endpoints
            .iter()
            .map(PolygonRpcEndpoint::redacted_url)
            .collect()
    }

    pub async fn health_check(&self) -> Vec<PolygonRpcHealth> {
        let client = reqwest::Client::builder()
            .timeout(self.request_timeout)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        let mut reports = Vec::with_capacity(self.endpoints.len());
        for endpoint in &self.endpoints {
            reports.push(check_endpoint(&client, endpoint).await);
        }
        reports
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolygonRpcHealth {
    pub endpoint: String,
    pub ok: bool,
    pub latency_ms: Option<u128>,
    pub block_number: Option<u64>,
    pub error: Option<String>,
}

async fn check_endpoint(
    client: &reqwest::Client,
    endpoint: &PolygonRpcEndpoint,
) -> PolygonRpcHealth {
    let started = Instant::now();
    let request = client
        .post(endpoint.url())
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "eth_blockNumber",
            "params": [],
        }))
        .send()
        .await;
    match request {
        Ok(response) => {
            let body = response.json::<serde_json::Value>().await;
            match body {
                Ok(value) => {
                    let block_number = value
                        .get("result")
                        .and_then(|result| result.as_str())
                        .and_then(parse_hex_u64);
                    PolygonRpcHealth {
                        endpoint: endpoint.redacted_url(),
                        ok: block_number.is_some(),
                        latency_ms: Some(started.elapsed().as_millis()),
                        block_number,
                        error: block_number
                            .is_none()
                            .then(|| format!("missing result in RPC response: {value}")),
                    }
                }
                Err(error) => PolygonRpcHealth {
                    endpoint: endpoint.redacted_url(),
                    ok: false,
                    latency_ms: Some(started.elapsed().as_millis()),
                    block_number: None,
                    error: Some(format!("invalid RPC JSON response: {error}")),
                },
            }
        }
        Err(error) => PolygonRpcHealth {
            endpoint: endpoint.redacted_url(),
            ok: false,
            latency_ms: Some(started.elapsed().as_millis()),
            block_number: None,
            error: Some(error.to_string()),
        },
    }
}

pub fn parse_hex_u64(raw: &str) -> Option<u64> {
    let trimmed = raw.trim();
    let hex = trimmed.strip_prefix("0x").unwrap_or(trimmed);
    u64::from_str_radix(hex, 16).ok()
}

pub fn redact_rpc_url(raw: &str) -> String {
    let Ok(mut url) = reqwest::Url::parse(raw) else {
        return "<invalid-rpc-url>".to_string();
    };
    if !url.username().is_empty() {
        let _ = url.set_username("***");
    }
    if url.password().is_some() {
        let _ = url.set_password(Some("***"));
    }
    if url.query().is_some() {
        url.set_query(Some("redacted"));
    }
    url.to_string()
}

fn duration_ms_from_env(key: &str, default: Duration) -> Duration {
    std::env::var(key)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map(Duration::from_millis)
        .unwrap_or(default)
}

pub fn missing_rpc_error(action: &str) -> ExecutionError {
    ExecutionError::BadRequest(format!(
        "EOA mode CTF {action} requires POLYGON_RPC_URL; use a low-latency Polygon provider and optional POLYGON_RPC_FAILOVER_URLS"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_deduplicates_primary_and_failovers() {
        let config = PolygonRpcConfig::from_parts(
            "https://polygon-mainnet.g.alchemy.com/v2/key".to_string(),
            vec![
                "https://polygon-mainnet.g.alchemy.com/v2/key".to_string(),
                "https://polygon-rpc.com".to_string(),
            ],
            Duration::from_millis(100),
            Duration::from_secs(1),
            250_000,
        );
        assert_eq!(config.endpoints().len(), 2);
        assert_eq!(
            config.primary_url(),
            Some("https://polygon-mainnet.g.alchemy.com/v2/key")
        );
    }

    #[test]
    fn redacts_query_and_basic_auth() {
        let redacted = redact_rpc_url("https://user:pass@example.com/path?apiKey=secret");
        assert_eq!(redacted, "https://***:***@example.com/path?redacted");
    }

    #[test]
    fn parses_hex_block_number() {
        assert_eq!(parse_hex_u64("0x10"), Some(16));
    }
}
