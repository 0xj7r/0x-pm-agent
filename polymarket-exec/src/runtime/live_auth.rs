//! Live Polymarket auth wiring from env/user auth into execution-adapter clients.

use anyhow::Result;

use crate::config::{AppConfig, UserWsAuth};
use crate::wire::execution_adapter::{
    PolymarketCredentials, PolymarketExecutionAdapter, PolymarketL1Credentials,
    PolymarketSignatureType,
};

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
        PolymarketExecutionAdapter::connect(PolymarketCredentials {
            api_key: auth.api_key.clone(),
            api_secret: auth.api_secret.clone(),
            api_passphrase: auth.api_passphrase.clone(),
            private_key,
            signature_type,
            funder_address,
        })
        .await
        .map_err(Into::into)
    } else {
        PolymarketExecutionAdapter::connect_with_l1(PolymarketL1Credentials {
            private_key,
            signature_type,
            funder_address,
        })
        .await
        .map_err(Into::into)
    }
}
