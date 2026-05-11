use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::Result;
use chrono::{DateTime, SecondsFormat, TimeZone, Utc};
use serde_json::Value;
use tokio::sync::{watch, RwLock};
use tracing::{info, warn};

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
    let next = RuntimeMarketUniverse::from_config_and_context(config, &contexts);
    if next.market_assets.is_empty() {
        warn!(
            target: "market_discovery",
            "market discovery returned no tradeable BTC 5m markets; keeping current universe until price_to_beat is available"
        );
        return Ok(None);
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
    let selected = filter_tradeable_price_to_beat_records(select_runtime_market_records(
        records,
        now_ms,
        config.market_discovery_include_prev,
        config.market_discovery_include_next,
    ));
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
            if let Some(mut record) = parse_gamma_market_record(item, slug_prefix, window_ms) {
                if record.price_to_beat.is_none() {
                    match fetch_crypto_open_price(client, slug_prefix, window_ms, &record).await {
                        Ok(Some(open_price)) => {
                            info!(
                                target: "market_discovery",
                                market_id = record.market_id,
                                price_to_beat = open_price,
                                "enriched crypto price_to_beat from Polymarket crypto-price API"
                            );
                            record.price_to_beat = Some(open_price);
                        }
                        Ok(None) => {
                            warn!(
                                target: "market_discovery",
                                market_id = record.market_id,
                                slug_prefix,
                                window_ms,
                                start_ms = ?record.event_start_time_ms,
                                end_ms = ?record.event_end_time_ms,
                                "Polymarket crypto-price API enrichment skipped or returned no openPrice"
                            );
                        }
                        Err(error) => {
                            warn!(
                                target: "market_discovery",
                                market_id = record.market_id,
                                error = %error,
                                "failed to enrich crypto price_to_beat from Polymarket crypto-price API"
                            );
                        }
                    }
                }
                out.push(record);
            }
        }
    }
    Ok(())
}

async fn fetch_crypto_open_price(
    client: &reqwest::Client,
    slug_prefix: &str,
    window_ms: u64,
    record: &MarketContextRecord,
) -> Result<Option<f64>> {
    let Some((symbol, variant)) = crypto_price_query_params(slug_prefix, window_ms) else {
        return Ok(None);
    };
    let (Some(start_ms), Some(end_ms)) = (record.event_start_time_ms, record.event_end_time_ms)
    else {
        return Ok(None);
    };
    let Some(event_start_time) = format_utc_ms(start_ms) else {
        return Ok(None);
    };
    let Some(end_date) = format_utc_ms(end_ms) else {
        return Ok(None);
    };

    let payload = client
        .get("https://polymarket.com/api/crypto/crypto-price")
        .query(&[
            ("symbol", symbol),
            ("eventStartTime", event_start_time.as_str()),
            ("variant", variant),
            ("endDate", end_date.as_str()),
        ])
        .header("User-Agent", "polymarket-agent/1.0")
        .header("Accept", "application/json")
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?;

    Ok(pick_gamma_f64(&payload, &["openPrice", "open_price"]))
}

fn crypto_price_query_params(
    slug_prefix: &str,
    window_ms: u64,
) -> Option<(&'static str, &'static str)> {
    let symbol = if slug_prefix.starts_with("btc-") {
        "BTC"
    } else if slug_prefix.starts_with("eth-") {
        "ETH"
    } else if slug_prefix.starts_with("sol-") {
        "SOL"
    } else {
        return None;
    };
    let variant = match window_ms {
        300_000 => "fiveminute",
        900_000 => "fifteen",
        3_600_000 => "hourly",
        14_400_000 => "fourhour",
        _ => return None,
    };
    Some((symbol, variant))
}

fn format_utc_ms(timestamp_ms: u64) -> Option<String> {
    Utc.timestamp_millis_opt(timestamp_ms as i64)
        .single()
        .map(|timestamp| timestamp.to_rfc3339_opts(SecondsFormat::Secs, true))
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

fn filter_tradeable_price_to_beat_records(
    records: Vec<MarketContextRecord>,
) -> Vec<MarketContextRecord> {
    records
        .into_iter()
        .filter(|record| {
            if record.price_to_beat.is_some() {
                return true;
            }
            warn!(
                target: "market_discovery",
                market_id = record.market_id,
                start_ms = ?record.event_start_time_ms,
                end_ms = ?record.event_end_time_ms,
                "dropping BTC timed market without price_to_beat; discovery will retry before trading"
            );
            false
        })
        .collect()
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
    let token_ids = parse_gamma_token_ids(
        value
            .get("clobTokenIds")
            .or_else(|| value.get("clobTokenIdsJson"))
            .or_else(|| value.get("token_ids_json"))
            .or_else(|| value.get("tokenIds")),
    );
    let outcomes = parse_gamma_string_list(value.get("outcomes").or_else(|| value.get("outcome")));
    let instrument_ids = order_up_down_token_ids(token_ids, outcomes);
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
    parse_gamma_string_list(value)
}

fn parse_gamma_string_list(value: Option<&Value>) -> Vec<String> {
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

fn order_up_down_token_ids(token_ids: Vec<String>, outcomes: Vec<String>) -> Vec<String> {
    if token_ids.len() < 2 || outcomes.len() != token_ids.len() {
        return token_ids;
    }

    let mut up_token = None;
    let mut down_token = None;
    for (token_id, outcome) in token_ids.iter().zip(outcomes.iter()) {
        let normalized = outcome.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "up" | "yes" => up_token = Some(token_id.clone()),
            "down" | "no" => down_token = Some(token_id.clone()),
            _ => {}
        }
    }

    match (up_token, down_token) {
        (Some(up), Some(down)) => vec![up, down],
        _ => token_ids,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crypto_price_query_params_supports_short_crypto_windows() {
        assert_eq!(
            crypto_price_query_params("btc-updown-5m-", 300_000),
            Some(("BTC", "fiveminute"))
        );
        assert_eq!(
            crypto_price_query_params("eth-updown-15m-", 900_000),
            Some(("ETH", "fifteen"))
        );
    }

    #[test]
    fn format_utc_ms_matches_polymarket_crypto_price_api_shape() {
        assert_eq!(
            format_utc_ms(1_777_744_800_000).as_deref(),
            Some("2026-05-02T18:00:00Z")
        );
    }

    #[test]
    fn gamma_record_orders_up_token_before_down_token() {
        let value = serde_json::json!({
            "id": "m",
            "slug": "btc-updown-5m-1777750200",
            "outcomes": "[\"Down\", \"Up\"]",
            "clobTokenIds": "[\"down-token\", \"up-token\"]",
            "priceToBeat": null
        });

        let record = parse_gamma_market_record(&value, "btc-updown-5m-", 300_000).unwrap();

        assert_eq!(record.instrument_ids, vec!["up-token", "down-token"]);
        assert_eq!(record.event_start_time_ms, Some(1_777_750_200_000));
        assert_eq!(record.event_end_time_ms, Some(1_777_750_500_000));
    }

    #[test]
    fn btc_timed_market_without_price_to_beat_is_not_tradeable() {
        let selected = filter_tradeable_price_to_beat_records(vec![
            MarketContextRecord {
                market_id: "missing-strike".to_string(),
                instrument_ids: vec!["up-a".to_string(), "down-a".to_string()],
                price_to_beat: None,
                final_price: None,
                event_start_time_ms: Some(1_777_750_200_000),
                event_end_time_ms: Some(1_777_750_500_000),
            },
            MarketContextRecord {
                market_id: "ready".to_string(),
                instrument_ids: vec!["up-b".to_string(), "down-b".to_string()],
                price_to_beat: Some(78_722.0),
                final_price: None,
                event_start_time_ms: Some(1_777_750_500_000),
                event_end_time_ms: Some(1_777_750_800_000),
            },
        ]);

        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].market_id, "ready");
        assert_eq!(selected[0].price_to_beat, Some(78_722.0));
    }
}
