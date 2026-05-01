use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::Value;
use tokio::sync::{watch, RwLock};
use tracing::info;

use crate::config::AppConfig;
use crate::market_context::{MarketContextRecord, MarketContextStore};
use crate::runtime::{Runtime, RuntimeOutcome};
use crate::strategy::StrategyMode;
use crate::types::InstrumentId;

#[derive(Debug, Clone)]
pub(super) struct RuntimeMarketUniverse {
    pub(super) market_assets: Vec<String>,
    pub(super) user_markets: Vec<String>,
    pub(super) market_id_by_asset: HashMap<String, String>,
}

impl RuntimeMarketUniverse {
    pub(super) fn from_config_and_context(
        config: &AppConfig,
        contexts: &MarketContextStore,
    ) -> Self {
        let context_assets = contexts.asset_ids();
        let context_markets = contexts.market_ids();
        let context_map = contexts.asset_market_map();
        Self {
            market_assets: if context_assets.is_empty() {
                config.market_assets.clone()
            } else {
                context_assets
            },
            user_markets: if context_markets.is_empty() {
                config.user_markets.clone()
            } else {
                context_markets
            },
            market_id_by_asset: if context_map.is_empty() {
                config.market_id_by_asset.clone()
            } else {
                context_map
            },
        }
    }

    pub(super) fn market_id_for_asset(&self, config: &AppConfig, asset_id: &str) -> String {
        self.market_id_by_asset
            .get(asset_id)
            .cloned()
            .or_else(|| config.market_id_by_asset.get(asset_id).cloned())
            .unwrap_or_else(|| asset_id.to_string())
    }
}

pub(super) async fn refresh_runtime_market_universe(
    config: &AppConfig,
    runtime: &mut Runtime<StrategyMode>,
    market_universe: &Arc<RwLock<RuntimeMarketUniverse>>,
    market_assets_tx: &watch::Sender<Vec<String>>,
    user_markets_tx: &watch::Sender<Vec<String>>,
    now_ms: u64,
) -> Result<Option<RuntimeOutcome>> {
    let contexts = fetch_btc_5m_market_contexts(config, now_ms).await?;
    if contexts.len() == 0 {
        anyhow::bail!("market discovery returned no BTC 5m markets");
    }
    let next = RuntimeMarketUniverse::from_config_and_context(config, &contexts);
    if next.market_assets.is_empty() {
        anyhow::bail!("market discovery returned no token ids");
    }

    let mut guard = market_universe.write().await;
    let changed = guard.market_assets != next.market_assets
        || guard.user_markets != next.user_markets
        || guard.market_id_by_asset != next.market_id_by_asset;
    if !changed {
        return Ok(None);
    }

    let active_instruments = next
        .market_assets
        .iter()
        .map(|asset| InstrumentId::from(asset.as_str()))
        .collect::<HashSet<_>>();
    let mut outcome = runtime.replace_market_contexts(contexts, now_ms, "gamma market discovery");
    outcome.extend(runtime.request_cancel_orders_not_in_instruments(
        &active_instruments,
        now_ms,
        "market universe rolled; cancel stale-market quote",
    ));

    *guard = next.clone();
    let _ = market_assets_tx.send(next.market_assets.clone());
    let _ = user_markets_tx.send(next.user_markets.clone());
    info!(
        target: "market_discovery",
        asset_count = next.market_assets.len(),
        market_count = next.user_markets.len(),
        markets = ?next.user_markets,
        "runtime market universe refreshed"
    );
    Ok(Some(outcome))
}

pub(super) async fn fetch_btc_5m_market_contexts(
    config: &AppConfig,
    now_ms: u64,
) -> Result<MarketContextStore> {
    let records = fetch_btc_5m_gamma_records(config, now_ms).await?;
    let selected = select_runtime_market_records(
        records,
        now_ms,
        config.market_discovery_include_prev,
        config.market_discovery_include_next,
    );
    Ok(MarketContextStore::from_records(
        selected,
        Some("gamma-api:engine-discovery".to_string()),
        Some(now_ms),
    ))
}

async fn fetch_btc_5m_gamma_records(
    config: &AppConfig,
    now_ms: u64,
) -> Result<Vec<MarketContextRecord>> {
    let client = reqwest::Client::new();
    let mut records = Vec::new();
    if config.market_discovery_families.is_empty() {
        let window_ms = config.market_discovery_window.as_millis().max(1) as u64;
        sweep_family_into(
            &client,
            &config.market_discovery_gamma_url,
            &config.market_discovery_slug_prefix,
            window_ms,
            config.market_discovery_include_prev,
            config.market_discovery_include_next,
            now_ms,
            &mut records,
        )
        .await?;
    } else {
        for family in &config.market_discovery_families {
            let window_ms = family.window.as_millis().max(1) as u64;
            sweep_family_into(
                &client,
                &config.market_discovery_gamma_url,
                &family.prefix,
                window_ms,
                config.market_discovery_include_prev,
                config.market_discovery_include_next,
                now_ms,
                &mut records,
            )
            .await?;
        }
    }
    records.sort_by_key(|record| {
        (
            record.event_start_time_ms.unwrap_or_default(),
            record.event_end_time_ms.unwrap_or_default(),
            record.market_id.clone(),
        )
    });
    records.dedup_by(|left, right| left.market_id == right.market_id);
    Ok(records)
}

#[allow(clippy::too_many_arguments)]
async fn sweep_family_into(
    client: &reqwest::Client,
    gamma_url: &str,
    slug_prefix: &str,
    window_ms: u64,
    include_prev: usize,
    include_next: usize,
    now_ms: u64,
    out: &mut Vec<MarketContextRecord>,
) -> Result<()> {
    let current_start_ms = now_ms - (now_ms % window_ms);
    let start_offset = -(include_prev as i64);
    let end_offset = include_next as i64;
    for offset in start_offset..=end_offset {
        let start_ms = if offset < 0 {
            current_start_ms.saturating_sub((-offset as u64) * window_ms)
        } else {
            current_start_ms.saturating_add((offset as u64) * window_ms)
        };
        let slug = format!("{slug_prefix}{}", start_ms / 1_000);
        let payload = client
            .get(gamma_url)
            .query(&[("slug", slug.as_str())])
            .header("User-Agent", "polymarket-agent/1.0")
            .header("Accept", "application/json")
            .send()
            .await?
            .error_for_status()?
            .json::<Value>()
            .await?;
        let Some(items) = payload.as_array() else {
            continue;
        };
        for item in items {
            if let Some(record) = parse_gamma_market_record(item, slug_prefix, window_ms) {
                out.push(record);
            }
        }
    }
    Ok(())
}

fn select_runtime_market_records(
    records: Vec<MarketContextRecord>,
    now_ms: u64,
    include_prev: usize,
    include_next: usize,
) -> Vec<MarketContextRecord> {
    let mut previous = records
        .iter()
        .filter(|record| record.event_end_time_ms.is_some_and(|end| end < now_ms))
        .cloned()
        .collect::<Vec<_>>();
    let active = records
        .iter()
        .filter(|record| record.is_active_btc_5m_window(now_ms))
        .cloned()
        .collect::<Vec<_>>();
    let mut upcoming = records
        .into_iter()
        .filter(|record| {
            record
                .event_start_time_ms
                .is_some_and(|start| start > now_ms)
        })
        .collect::<Vec<_>>();
    previous.sort_by_key(|record| std::cmp::Reverse(record.event_end_time_ms.unwrap_or_default()));
    upcoming.sort_by_key(|record| record.event_start_time_ms.unwrap_or_default());

    let mut selected = previous.into_iter().take(include_prev).collect::<Vec<_>>();
    selected.reverse();
    selected.extend(active);
    selected.extend(upcoming.into_iter().take(include_next));
    selected
}

fn parse_gamma_market_record(
    value: &Value,
    slug_prefix: &str,
    window_ms: u64,
) -> Option<MarketContextRecord> {
    let slug = value.get("slug")?.as_str()?.trim();
    if !slug.starts_with(slug_prefix) {
        return None;
    }
    let market_id = value
        .get("id")
        .or_else(|| value.get("market_id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    let instrument_ids = parse_gamma_token_ids(
        value
            .get("clobTokenIds")
            .or_else(|| value.get("clobTokenIdsJson"))
            .or_else(|| value.get("token_ids_json"))
            .or_else(|| value.get("tokenIds")),
    );
    if instrument_ids.len() < 2 {
        return None;
    }
    let start_ms = parse_gamma_time_ms(
        value
            .get("event_start_time")
            .or_else(|| value.get("eventStartTime"))
            .or_else(|| value.get("startTime"))
            .or_else(|| value.get("startDate"))
            .or_else(|| value.get("start_date")),
    )
    .or_else(|| parse_start_ms_from_btc_slug(slug));
    let end_ms = parse_gamma_time_ms(
        value
            .get("endDate")
            .or_else(|| value.get("end_date"))
            .or_else(|| value.get("endTime"))
            .or_else(|| value.get("end_time")),
    )
    .or_else(|| start_ms.map(|start| start.saturating_add(window_ms)));
    let start_ms = start_ms.or_else(|| end_ms.map(|end| end.saturating_sub(window_ms)));

    Some(MarketContextRecord {
        market_id: market_id.to_string(),
        instrument_ids: instrument_ids.into_iter().take(2).collect(),
        price_to_beat: pick_gamma_f64(value, &["priceToBeat", "price_to_beat"]),
        final_price: pick_gamma_f64(value, &["finalPrice", "final_price"]),
        event_start_time_ms: start_ms,
        event_end_time_ms: end_ms,
    })
}

fn parse_gamma_token_ids(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| item.as_str().map(str::to_string))
            .filter(|item| !item.trim().is_empty())
            .collect(),
        Some(Value::String(raw)) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return Vec::new();
            }
            if let Ok(parsed) = serde_json::from_str::<Value>(trimmed) {
                return parse_gamma_token_ids(Some(&parsed));
            }
            trimmed
                .split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(str::to_string)
                .collect()
        }
        _ => Vec::new(),
    }
}

fn parse_gamma_time_ms(value: Option<&Value>) -> Option<u64> {
    let raw = value?.as_str()?;
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|ts| ts.with_timezone(&Utc).timestamp_millis().max(0) as u64)
}

fn parse_start_ms_from_btc_slug(slug: &str) -> Option<u64> {
    slug.rsplit('-')
        .next()
        .and_then(|part| part.parse::<u64>().ok())
        .map(|seconds| seconds.saturating_mul(1_000))
}

fn pick_gamma_f64(value: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|key| match value.get(*key)? {
        Value::Number(number) => number.as_f64(),
        Value::String(raw) => raw.parse::<f64>().ok(),
        _ => None,
    })
}
