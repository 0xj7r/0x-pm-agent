//! Gamma lookup for legacy `would_enter` rows missing `token_id` / redeem fields.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::sync::Mutex;

const GAMMA_MARKETS_URL: &str = "https://gamma-api.polymarket.com/markets";

#[derive(Debug, Clone)]
pub struct GammaMarketMeta {
    pub up_token: String,
    pub down_token: String,
    pub up_index_set: u64,
    pub down_index_set: u64,
    pub condition_id: Option<String>,
    pub close_ts_s: i64,
}

#[derive(Clone, Default)]
pub struct GammaResolver {
    client: reqwest::Client,
    cache: Arc<Mutex<HashMap<String, GammaMarketMeta>>>,
}

impl GammaResolver {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn resolve(&self, slug: &str) -> Result<GammaMarketMeta> {
        if let Some(hit) = self.cache.lock().await.get(slug).cloned() {
            return Ok(hit);
        }
        let meta = fetch_gamma_market(&self.client, slug)
            .await?
            .with_context(|| format!("gamma has no market for slug={slug}"))?;
        self.cache.lock().await.insert(slug.to_string(), meta.clone());
        Ok(meta)
    }
}

async fn fetch_gamma_market(client: &reqwest::Client, slug: &str) -> Result<Option<GammaMarketMeta>> {
    let payload = client
        .get(GAMMA_MARKETS_URL)
        .query(&[("slug", slug)])
        .header("User-Agent", "shadow-exec-tail/1.0")
        .header("Accept", "application/json")
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?;
    let Some(items) = payload.as_array() else {
        return Ok(None);
    };
    Ok(items.iter().find_map(|item| parse_gamma_market(item, slug)))
}

fn parse_gamma_market(item: &Value, slug: &str) -> Option<GammaMarketMeta> {
    if item.get("slug").and_then(Value::as_str) != Some(slug) {
        return None;
    }
    let open_ts_s: i64 = slug.rsplit('-').next()?.parse().ok()?;
    let tokens = parse_json_string_list(item.get("clobTokenIds"));
    let outcomes = parse_json_string_list(item.get("outcomes"));
    let (up_token, down_token) = order_up_down(&tokens, &outcomes)?;
    let up_index_set = if up_token == tokens[0] { 1 } else { 2 };
    let down_index_set = if down_token == tokens[0] { 1 } else { 2 };
    let condition_id = item
        .get("conditionId")
        .or_else(|| item.get("condition_id"))
        .and_then(Value::as_str)
        .map(str::to_string);
    Some(GammaMarketMeta {
        up_token,
        down_token,
        up_index_set,
        down_index_set,
        condition_id,
        close_ts_s: open_ts_s + 300,
    })
}

fn parse_json_string_list(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        Some(Value::String(raw)) => serde_json::from_str::<Value>(raw)
            .ok()
            .map(|v| parse_json_string_list(Some(&v)))
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn order_up_down(tokens: &[String], outcomes: &[String]) -> Option<(String, String)> {
    if tokens.len() < 2 {
        return None;
    }
    if outcomes.len() == tokens.len() {
        let mut up = None;
        let mut down = None;
        for (token, outcome) in tokens.iter().zip(outcomes) {
            match outcome.trim().to_ascii_lowercase().as_str() {
                "up" | "yes" => up = Some(token.clone()),
                "down" | "no" => down = Some(token.clone()),
                _ => {}
            }
        }
        if let (Some(up), Some(down)) = (up, down) {
            return Some((up, down));
        }
    }
    Some((tokens[0].clone(), tokens[1].clone()))
}

pub fn close_ts_from_slug(slug: &str) -> Option<i64> {
    let open_ts_s: i64 = slug.rsplit('-').next()?.parse().ok()?;
    Some(open_ts_s + 300)
}