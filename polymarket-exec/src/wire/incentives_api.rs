//! Incentives/rewards API adapter.

use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;

use crate::signals::IncentiveSignal;
use crate::types::MarketId;

#[derive(Clone, Debug)]
pub struct IncentivesApiClient {
    client: Client,
    base_url: String,
}

impl IncentivesApiClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            base_url: base_url.into(),
        }
    }

    pub async fn fetch_market_incentives(&self, market_id: &MarketId) -> Result<IncentiveSignal> {
        let url = format!(
            "{}/markets/{}/incentives",
            self.base_url.trim_end_matches('/'),
            market_id
        );
        let response = self
            .client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("failed to fetch incentives from {url}"))?;
        let status = response.status();
        if !status.is_success() {
            anyhow::bail!("incentives API returned {status} for {url}");
        }
        let payload: IncentiveApiPayload = response
            .json()
            .await
            .with_context(|| format!("failed to parse incentives response from {url}"))?;
        Ok(payload.into_signal())
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
struct IncentiveApiPayload {
    min_incentive_size: Option<f64>,
    min_size: Option<f64>,
    max_incentive_spread: Option<f64>,
    max_spread: Option<f64>,
    maker_rebate_bps: Option<f64>,
    estimated_reward_edge_bps: Option<f64>,
}

impl IncentiveApiPayload {
    fn into_signal(self) -> IncentiveSignal {
        IncentiveSignal {
            min_incentive_size: self.min_incentive_size.or(self.min_size),
            max_incentive_spread: self.max_incentive_spread.or(self.max_spread),
            maker_rebate_bps: self.maker_rebate_bps,
            estimated_reward_edge_bps: self.estimated_reward_edge_bps,
        }
    }
}
