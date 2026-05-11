//! Live Polymarket auth wiring from env/user auth into execution-adapter clients.

use std::str::FromStr;

use alloy::signers::local::PrivateKeySigner;
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
        PolymarketSignatureType::Poly1271 => "poly_1271",
        PolymarketSignatureType::GnosisSafe => "gnosis_safe",
    }
}

fn log_live_venue_config(
    config: &AppConfig,
    signature_type: PolymarketSignatureType,
    funder_address: Option<&str>,
    user_auth_source: &str,
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
        user_auth_source,
        has_user_credentials = user_auth_source != "none",
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
            "POLYMARKET_SIGNATURE_TYPE must be set explicitly (eoa, proxy, gnosis_safe, or poly_1271)"
        )
    })?;
    PolymarketSignatureType::parse(raw.as_str()).map_err(|error| anyhow::anyhow!(error.to_string()))
}

fn live_funder_from_env(auth: Option<&UserWsAuth>) -> Option<String> {
    auth.and_then(|auth| auth.funder_address.clone())
        .or_else(|| std::env::var("POLYMARKET_FUNDER_ADDRESS").ok())
        .filter(|value| !value.trim().is_empty())
}

fn normalize_address(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn signer_address_from_private_key(private_key: &str) -> Result<String> {
    let signer = PrivateKeySigner::from_str(private_key.trim())
        .map_err(|error| anyhow::anyhow!("invalid POLYMARKET_PRIVATE_KEY: {error}"))?;
    Ok(signer.address().to_string())
}

fn resolve_funder_for_signature(
    private_key: &str,
    signature_type: PolymarketSignatureType,
    funder_address: Option<String>,
    proxy_wallet_address: Option<String>,
) -> Result<Option<String>> {
    let signer_address = signer_address_from_private_key(private_key)?;
    match signature_type {
        PolymarketSignatureType::Eoa => {
            for (name, configured) in [
                (
                    "POLYMARKET_FUNDER_ADDRESS",
                    funder_address.as_ref(),
                ),
                (
                    "POLYMARKET_PROXY_WALLET_ADDRESS",
                    proxy_wallet_address.as_ref(),
                ),
            ] {
                let Some(configured) = configured else {
                    continue;
                };
                if normalize_address(configured) != normalize_address(&signer_address) {
                    anyhow::bail!(
                        "invalid live auth config: POLYMARKET_SIGNATURE_TYPE=0 (EOA) requires \
                         funder/holder to equal signer {signer_address}, but {name} is {configured}. \
                         Use signature_type=proxy/gnosis_safe/poly_1271 for smart-wallet-held funds, or remove the \
                         proxy/funder env vars for true EOA trading."
                    );
                }
            }
            Ok(None)
        }
        PolymarketSignatureType::Proxy
        | PolymarketSignatureType::GnosisSafe
        | PolymarketSignatureType::Poly1271 => {
            let resolved = funder_address.or(proxy_wallet_address);
            let Some(resolved) = resolved else {
                anyhow::bail!(
                    "invalid live auth config: proxy/safe/poly_1271 signature types require \
                     POLYMARKET_FUNDER_ADDRESS or POLYMARKET_PROXY_WALLET_ADDRESS"
                );
            };
            Ok(Some(resolved))
        }
    }
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
    let configured_funder_address = live_funder_from_env(auth);
    let funder_address = resolve_funder_for_signature(
        &private_key,
        signature_type,
        configured_funder_address,
        config.proxy_wallet_address.clone(),
    )?;

    log_live_venue_config(
        config,
        signature_type,
        funder_address.as_deref(),
        if auth.is_some() { "env" } else { "derived_l1" },
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
            ctf_collateral_token_address: config.ctf_collateral_token_address.clone(),
            collateral_token_address: config.collateral_token_address.clone(),
            collateral_decimals: config.collateral_decimals,
            proxy_wallet_address: config.proxy_wallet_address.clone(),
            polygon_rpc_url: config.polygon_rpc_url.clone(),
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
                ctf_collateral_token_address: config.ctf_collateral_token_address.clone(),
                collateral_token_address: config.collateral_token_address.clone(),
                collateral_decimals: config.collateral_decimals,
                proxy_wallet_address: config.proxy_wallet_address.clone(),
                polygon_rpc_url: config.polygon_rpc_url.clone(),
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
                        PolymarketSignatureType::Poly1271 => "poly_1271".to_string(),
                        PolymarketSignatureType::GnosisSafe => "gnosis_safe".to_string(),
                    },
                )),
                funder_address,
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{resolve_funder_for_signature, PolymarketSignatureType};

    const TEST_PRIVATE_KEY: &str =
        "0x59c6995e998f97a5a0044966f094538340a3a38f1a07c6d82e841fe4b0d9f10a";
    const TEST_SIGNER_ADDRESS: &str = "0x769C78CF371775A2603BBdD895beD3b34C8F32Df";

    #[test]
    fn eoa_signature_rejects_mismatched_funder() {
        let error = resolve_funder_for_signature(
            TEST_PRIVATE_KEY,
            PolymarketSignatureType::Eoa,
            Some("0xa57189d5b2285A5E64083d3925687bDFCE01fC83".to_string()),
            None,
        )
        .expect_err("mismatched EOA funder must fail live startup");

        assert!(error
            .to_string()
            .contains("requires funder/holder to equal signer"));
    }

    #[test]
    fn eoa_signature_accepts_matching_funder_as_noop() {
        assert_eq!(
            resolve_funder_for_signature(
                TEST_PRIVATE_KEY,
                PolymarketSignatureType::Eoa,
                Some(TEST_SIGNER_ADDRESS.to_string()),
                None,
            )
            .expect("matching EOA holder config"),
            None
        );
    }

    #[test]
    fn proxy_signature_keeps_configured_funder() {
        assert_eq!(
            resolve_funder_for_signature(
                TEST_PRIVATE_KEY,
                PolymarketSignatureType::Proxy,
                Some("0xa57189d5b2285A5E64083d3925687bDFCE01fC83".to_string()),
                None,
            )
            .expect("proxy funder"),
            Some("0xa57189d5b2285A5E64083d3925687bDFCE01fC83".to_string())
        );
    }

    #[test]
    fn proxy_signature_can_use_proxy_wallet_as_funder() {
        assert_eq!(
            resolve_funder_for_signature(
                TEST_PRIVATE_KEY,
                PolymarketSignatureType::Proxy,
                None,
                Some("0xa57189d5b2285A5E64083d3925687bDFCE01fC83".to_string()),
            )
            .expect("proxy wallet funder fallback"),
            Some("0xa57189d5b2285A5E64083d3925687bDFCE01fC83".to_string())
        );
    }

    #[test]
    fn proxy_signature_requires_holder_address() {
        let error = resolve_funder_for_signature(
            TEST_PRIVATE_KEY,
            PolymarketSignatureType::Proxy,
            None,
            None,
        )
        .expect_err("proxy mode without funder must fail");

        assert!(error
            .to_string()
            .contains("proxy/safe signature types require"));
    }
}
