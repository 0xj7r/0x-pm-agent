//! Live Polymarket auth wiring from env/user auth into execution-adapter clients.

use anyhow::Result;
use tracing::info;

use crate::config::{AppConfig, UserWsAuth};
use crate::wire::clob_v2::CLOB_V2_EXCHANGE;
use crate::wire::execution_adapter::{
    ClobProtocolVersion, PolymarketConfig, PolymarketCredentials, PolymarketExecutionAdapter,
    PolymarketL1Credentials, PolymarketSignatureType,
};

fn signature_type_label(sig: PolymarketSignatureType) -> &'static str {
    match sig {
        PolymarketSignatureType::Eoa => "eoa",
        PolymarketSignatureType::Proxy => "proxy",
        PolymarketSignatureType::GnosisSafe => "gnosis_safe",
    }
}

fn log_live_venue_config(
    config: &AppConfig,
    signature_type: PolymarketSignatureType,
    funder_address: Option<&str>,
    has_user_credentials: bool,
) {
    let protocol = ClobProtocolVersion::parse(&config.clob_version)
        .map(|v| format!("{v:?}"))
        .unwrap_or_else(|_| config.clob_version.clone());
    info!(
        target: "live_auth.startup",
        clob_api_url = %config.clob_api_url,
        clob_protocol = %protocol,
        clob_v2_exchange = %CLOB_V2_EXCHANGE,
        clob_v2_neg_risk = config.clob_v2_neg_risk,
        clob_v2_builder_code_present = !config.clob_v2_builder_code.is_empty(),
        clob_v2_metadata_present = !config.clob_v2_metadata.is_empty(),
        signature_type = signature_type_label(signature_type),
        funder_address = funder_address.unwrap_or("<none>"),
        proxy_wallet_address = config.proxy_wallet_address.as_deref().unwrap_or("<none>"),
        relayer_url = %config.relayer_url,
        relayer_api_key_present = config.relayer_api_key.is_some(),
        has_user_credentials,
        "live venue config (verify CLOB V2 + auth before trading)"
    );
}

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
    let raw = raw.ok_or_else(|| {
        anyhow::anyhow!(
            "POLYMARKET_SIGNATURE_TYPE must be set explicitly (eoa, proxy, or gnosis_safe)"
        )
    })?;
    PolymarketSignatureType::parse(raw.as_str()).map_err(|error| anyhow::anyhow!(error.to_string()))
}

fn live_funder_from_env(auth: Option<&UserWsAuth>) -> Option<String> {
    auth.and_then(|auth| auth.funder_address.clone())
        .or_else(|| std::env::var("POLYMARKET_FUNDER_ADDRESS").ok())
        .or_else(|| std::env::var("POLYMARKET_FUNDER").ok())
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

    log_live_venue_config(
        config,
        signature_type,
        funder_address.as_deref(),
        auth.is_some(),
    );

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
            relayer_url: config.relayer_url.clone(),
            relayer_api_key: config.relayer_api_key.clone(),
            relayer_api_key_address: config.relayer_api_key_address.clone(),
            ctf_contract_address: config.ctf_contract_address.clone(),
            collateral_token_address: config.collateral_token_address.clone(),
            collateral_decimals: config.collateral_decimals,
            proxy_wallet_address: config.proxy_wallet_address.clone(),
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
                relayer_url: config.relayer_url.clone(),
                relayer_api_key: config.relayer_api_key.clone(),
                relayer_api_key_address: config.relayer_api_key_address.clone(),
                ctf_contract_address: config.ctf_contract_address.clone(),
                collateral_token_address: config.collateral_token_address.clone(),
                collateral_decimals: config.collateral_decimals,
                proxy_wallet_address: config.proxy_wallet_address.clone(),
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
