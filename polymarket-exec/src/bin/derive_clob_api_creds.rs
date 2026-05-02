//! Derive Polymarket CLOB L2 API credentials from the configured L1 wallet.
//!
//! This is an operator utility for environments without UI access. It signs the
//! Polymarket CLOB auth challenge with `POLYMARKET_PRIVATE_KEY`, then prints the
//! derived `POLYMARKET_API_KEY`, `POLYMARKET_API_SECRET`, and
//! `POLYMARKET_API_PASSPHRASE` as shell export lines.

use anyhow::{Context, Result};
use std::str::FromStr as _;

use polymarket_client_sdk_v2::auth::Credentials;
use polymarket_client_sdk_v2::auth::{ExposeSecret as _, LocalSigner, Signer as _};
use polymarket_client_sdk_v2::clob::{Client as ClobClient, Config as ClobConfig};
use polymarket_client_sdk_v2::POLYGON;

fn env_optional(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn env_required(key: &str) -> Result<String> {
    env_optional(key).with_context(|| format!("{key} must be set"))
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

async fn credentials_for_mode(
    client: &ClobClient,
    signer: &LocalSigner<alloy::signers::k256::ecdsa::SigningKey>,
    mode: &str,
) -> polymarket_client_sdk_v2::Result<Credentials> {
    match mode {
        "create" => client.create_api_key(signer, None).await,
        "derive" => client.derive_api_key(signer, None).await,
        "create-or-derive" | "create_or_derive" => {
            client.create_or_derive_api_key(signer, None).await
        }
        _ => client.create_or_derive_api_key(signer, None).await,
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    let private_key = env_required("POLYMARKET_PRIVATE_KEY")
        .or_else(|_| env_required("METAMASK_PRIVATE_KEY"))
        .context("POLYMARKET_PRIVATE_KEY or METAMASK_PRIVATE_KEY must be set")?;
    let clob_api_url = env_optional("POLYMARKET_CLOB_API_URL")
        .unwrap_or_else(|| "https://clob.polymarket.com".to_string());
    let mode = env_optional("POLYMARKET_CLOB_API_KEY_MODE")
        .unwrap_or_else(|| "create-or-derive".to_string())
        .to_ascii_lowercase();

    let signer = LocalSigner::from_str(private_key.trim())
        .context("failed to parse POLYMARKET_PRIVATE_KEY")?
        .with_chain_id(Some(POLYGON));
    let client = ClobClient::new(clob_api_url.as_str(), ClobConfig::default())
        .context("failed to create Polymarket CLOB client")?;
    let credentials = credentials_for_mode(&client, &signer, mode.as_str())
        .await
        .with_context(|| {
            format!("failed to {mode} Polymarket CLOB API credentials via {clob_api_url}")
        })?;

    println!("# Derived from wallet; keep these out of git.");
    println!(
        "export POLYMARKET_API_KEY={}",
        shell_single_quote(&credentials.key().to_string())
    );
    println!(
        "export POLYMARKET_API_SECRET={}",
        shell_single_quote(credentials.secret().expose_secret())
    );
    println!(
        "export POLYMARKET_API_PASSPHRASE={}",
        shell_single_quote(credentials.passphrase().expose_secret())
    );

    Ok(())
}
