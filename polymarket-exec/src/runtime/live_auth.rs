//! Live Polymarket auth wiring from env/user auth into execution-adapter clients.

use anyhow::Result;

use crate::config::{AppConfig, UserWsAuth};
use crate::wire::execution_adapter::{
    ClobProtocolVersion, PolymarketConfig, PolymarketCredentials, PolymarketExecutionAdapter,
    PolymarketL1Credentials, PolymarketSignatureType,
};

pub(super) struct LiveConnection {
    pub adapter: PolymarketExecutionAdapter,
    pub user_auth: Option<UserWsAuth>,
}

fn live_private_key_from_env() -> Option<String> {
    std::env::var("POLYMARKET_PRIVATE_KEY")
        .ok()
        .or_else(|| std::env::var("METAMASK_PRIVATE_KEY").ok())
        .filter(|value| !value.trim().is_empty())
}

fn live_signature_type_from_env(auth: Option<&UserWsAuth>) -> Result<PolymarketSignatureType> {
    let raw = auth
        .and_then(|auth| auth.signature_type.clone())
        .or_else(|| std::env::var("POLYMARKET_SIGNATURE_TYPE").ok());
    raw.as_deref()
        .map(PolymarketSignatureType::parse)
        .transpose()
        .map_err(|error| anyhow::anyhow!(error.to_string()))
        .map(|value| value.unwrap_or_default())
}

fn live_funder_from_env(auth: Option<&UserWsAuth>) -> Option<String> {
    auth.and_then(|auth| auth.funder_address.clone())
        .or_else(|| std::env::var("POLYMARKET_FUNDER_ADDRESS").ok())
        .filter(|value| !value.trim().is_empty())
}

pub(super) async fn connect_live_adapter(config: &AppConfig) -> Result<PolymarketExecutionAdapter> {
    connect_live_session(config)
        .await
        .map(|connection| connection.adapter)
}

pub(super) async fn connect_live_session(config: &AppConfig) -> Result<LiveConnection> {
    let auth = config.user_auth.as_ref();
    let private_key = auth
        .and_then(|auth| auth.private_key.clone())
        .or_else(live_private_key_from_env)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "live execution requires POLYMARKET_PRIVATE_KEY (or METAMASK_PRIVATE_KEY alias)"
            )
        })?;
    let signature_type = live_signature_type_from_env(auth)?;
    let funder_address = live_funder_from_env(auth);

    if let Some(auth) = auth {
        let credentials = PolymarketCredentials {
            api_key: auth.api_key.clone(),
            api_secret: auth.api_secret.clone(),
            api_passphrase: auth.api_passphrase.clone(),
            private_key,
            signature_type,
            funder_address,
        };
        let adapter = PolymarketExecutionAdapter::connect_with_config(PolymarketConfig {
            api_url: config.clob_api_url.clone(),
            data_api_url: config.data_api_url.clone(),
            market_id_by_asset: config.market_id_by_asset.clone(),
            protocol: ClobProtocolVersion::parse(&config.clob_version)
                .map_err(|error| anyhow::anyhow!(error.to_string()))?,
            v2_builder_code: config.clob_v2_builder_code.clone(),
            v2_metadata: config.clob_v2_metadata.clone(),
            v2_neg_risk: config.clob_v2_neg_risk,
            credentials: Some(credentials),
        })
        .await?;
        Ok(LiveConnection {
            adapter,
            user_auth: Some(auth.clone()),
        })
    } else {
        let adapter = PolymarketExecutionAdapter::connect_with_l1_config(
            PolymarketConfig {
                api_url: config.clob_api_url.clone(),
                data_api_url: config.data_api_url.clone(),
                market_id_by_asset: config.market_id_by_asset.clone(),
                protocol: ClobProtocolVersion::parse(&config.clob_version)
                    .map_err(|error| anyhow::anyhow!(error.to_string()))?,
                v2_builder_code: config.clob_v2_builder_code.clone(),
                v2_metadata: config.clob_v2_metadata.clone(),
                v2_neg_risk: config.clob_v2_neg_risk,
                credentials: None,
            },
            PolymarketL1Credentials {
                private_key,
                signature_type,
                funder_address: funder_address.clone(),
            },
        )
        .await?;
        let (api_key, api_secret, api_passphrase) = adapter.api_credentials();
        Ok(LiveConnection {
            adapter,
            user_auth: Some(UserWsAuth {
                api_key,
                api_secret,
                api_passphrase,
                private_key: None,
                signature_type: Some(std::env::var("POLYMARKET_SIGNATURE_TYPE").unwrap_or_else(
                    |_| match signature_type {
                        PolymarketSignatureType::Eoa => "eoa".to_string(),
                        PolymarketSignatureType::Proxy => "proxy".to_string(),
                        PolymarketSignatureType::GnosisSafe => "gnosis_safe".to_string(),
                    },
                )),
                funder_address,
            }),
        })
    }
}
