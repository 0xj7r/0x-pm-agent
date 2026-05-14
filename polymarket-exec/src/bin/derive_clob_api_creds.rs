//! Derive Polymarket CLOB L2 API credentials from the configured L1 wallet.
//!
//! This is an operator utility for environments without UI access. It signs the
//! Polymarket CLOB auth challenge with `POLYMARKET_PRIVATE_KEY`, then prints the
//! derived `POLYMARKET_API_KEY`, `POLYMARKET_API_SECRET`, and
//! `POLYMARKET_API_PASSPHRASE` as shell export lines.

use anyhow::{Context, Result};
use reqwest::header::{HeaderMap, HeaderValue};
use serde::Deserialize;
use std::str::FromStr as _;
use std::time::{SystemTime, UNIX_EPOCH};

use alloy::core::sol;
use alloy::dyn_abi::Eip712Domain;
use alloy::hex::ToHexExt as _;
use alloy::primitives::{keccak256, U256};
use alloy::sol_types::{SolStruct as _, SolValue as _};
use polymarket_client_sdk_v2::auth::Credentials;
use polymarket_client_sdk_v2::auth::{ExposeSecret as _, LocalSigner, Signer as _, Uuid};
use polymarket_client_sdk_v2::clob::types::SignatureType;
use polymarket_client_sdk_v2::clob::{Client as ClobClient, Config as ClobConfig};
use polymarket_client_sdk_v2::types::{Address, B256};
use polymarket_client_sdk_v2::POLYGON;

const DEPOSIT_WALLET_NAME: &str = "DepositWallet";
const DEPOSIT_WALLET_VERSION: &str = "1";
const CLOBAUTH_TYPE_STRING: &str =
    "ClobAuth(address address,string timestamp,uint256 nonce,string message)";
const SOLADY_CLOBAUTH_TYPE_STRING: &str = concat!(
    "TypedDataSign(ClobAuth contents,string name,string version,uint256 chainId,",
    "address verifyingContract,bytes32 salt)",
    "ClobAuth(address address,string timestamp,uint256 nonce,string message)"
);

sol! {
    #[non_exhaustive]
    struct ClobAuth {
        address address;
        string  timestamp;
        uint256 nonce;
        string  message;
    }
}

#[derive(Debug, Deserialize)]
struct RawCredentials {
    #[serde(alias = "apiKey")]
    api_key: String,
    secret: String,
    passphrase: String,
}

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

fn push_hex(out: &mut String, bytes: &[u8]) {
    const LUT: &[u8; 16] = b"0123456789abcdef";
    out.reserve(bytes.len() * 2);
    for byte in bytes {
        out.push(LUT[(byte >> 4) as usize] as char);
        out.push(LUT[(byte & 0x0f) as usize] as char);
    }
}

async fn credentials_for_mode(
    client: ClobClient,
    signer: &LocalSigner<alloy::signers::k256::ecdsa::SigningKey>,
    mode: &str,
    signature_type: SignatureType,
    funder: Option<Address>,
) -> polymarket_client_sdk_v2::Result<Credentials> {
    if signature_type != SignatureType::Eoa || funder.is_some() {
        let mut auth_builder = client
            .authentication_builder(signer)
            .signature_type(signature_type);
        if let Some(funder) = funder {
            auth_builder = auth_builder.funder(funder);
        }
        return auth_builder
            .authenticate()
            .await
            .map(|authenticated| authenticated.credentials().clone());
    }

    match mode {
        "create" => client.create_api_key(signer, None).await,
        "derive" => client.derive_api_key(signer, None).await,
        "create-or-derive" | "create_or_derive" => {
            client.create_or_derive_api_key(signer, None).await
        }
        _ => client.create_or_derive_api_key(signer, None).await,
    }
}

fn now_unix_s() -> Result<i64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system time before UNIX_EPOCH")?
        .as_secs() as i64)
}

async fn poly1271_l1_headers(
    signer: &LocalSigner<alloy::signers::k256::ecdsa::SigningKey>,
    funder: Address,
    timestamp: i64,
    nonce: u32,
) -> Result<HeaderMap> {
    let chain_id = signer.chain_id().context("signer chain id must be set")?;
    let auth = ClobAuth {
        address: funder,
        timestamp: timestamp.to_string(),
        nonce: U256::from(nonce),
        message: "This message attests that I control the given wallet".to_owned(),
    };
    let app_domain = Eip712Domain {
        name: Some(std::borrow::Cow::Borrowed("ClobAuthDomain")),
        version: Some(std::borrow::Cow::Borrowed("1")),
        chain_id: Some(U256::from(chain_id)),
        ..Eip712Domain::default()
    };
    let contents_hash = auth.eip712_hash_struct();
    let app_domain_separator = app_domain.hash_struct();
    let typed_data_sign_struct_hash = keccak256(
        (
            keccak256(SOLADY_CLOBAUTH_TYPE_STRING.as_bytes()),
            contents_hash,
            keccak256(DEPOSIT_WALLET_NAME.as_bytes()),
            keccak256(DEPOSIT_WALLET_VERSION.as_bytes()),
            U256::from(chain_id),
            funder,
            B256::ZERO,
        )
            .abi_encode(),
    );
    let mut digest_input = [0_u8; 66];
    digest_input[0] = 0x19;
    digest_input[1] = 0x01;
    digest_input[2..34].copy_from_slice(app_domain_separator.as_slice());
    digest_input[34..66].copy_from_slice(typed_data_sign_struct_hash.as_slice());
    let digest = keccak256(digest_input);
    let inner_signature = signer.sign_hash(&digest).await?;
    let mut wrapped =
        String::with_capacity(2 + 130 + 64 + 64 + (CLOBAUTH_TYPE_STRING.len() * 2) + 4);
    wrapped.push_str("0x");
    wrapped.push_str(inner_signature.to_string().trim_start_matches("0x"));
    push_hex(&mut wrapped, app_domain_separator.as_slice());
    push_hex(&mut wrapped, contents_hash.as_slice());
    push_hex(&mut wrapped, CLOBAUTH_TYPE_STRING.as_bytes());
    let contents_type_len =
        u16::try_from(CLOBAUTH_TYPE_STRING.len()).context("ClobAuth type string too long")?;
    push_hex(&mut wrapped, &contents_type_len.to_be_bytes());

    let mut headers = HeaderMap::new();
    headers.insert(
        "POLY_ADDRESS",
        HeaderValue::from_str(&funder.encode_hex_with_prefix())?,
    );
    headers.insert("POLY_NONCE", HeaderValue::from_str(&nonce.to_string())?);
    headers.insert("POLY_SIGNATURE", HeaderValue::from_str(&wrapped)?);
    headers.insert(
        "POLY_TIMESTAMP",
        HeaderValue::from_str(&timestamp.to_string())?,
    );
    Ok(headers)
}

async fn poly1271_credentials_for_mode(
    host: &str,
    signer: &LocalSigner<alloy::signers::k256::ecdsa::SigningKey>,
    funder: Address,
    mode: &str,
) -> Result<Credentials> {
    let http = reqwest::Client::new();
    let host = host.trim_end_matches('/');
    let timestamp = now_unix_s()?;
    let nonce = 0_u32;

    async fn send(
        http: &reqwest::Client,
        method: reqwest::Method,
        url: String,
        headers: HeaderMap,
    ) -> Result<RawCredentials> {
        let response = http.request(method, url).headers(headers).send().await?;
        let status = response.status();
        let text = response.text().await?;
        if !status.is_success() {
            anyhow::bail!("credential request failed {status}: {text}");
        }
        serde_json::from_str(&text).context("failed to parse credential response")
    }

    let create = || async {
        let headers = poly1271_l1_headers(signer, funder, timestamp, nonce).await?;
        send(
            &http,
            reqwest::Method::POST,
            format!("{host}/auth/api-key"),
            headers,
        )
        .await
    };
    let derive = || async {
        let headers = poly1271_l1_headers(signer, funder, timestamp, nonce).await?;
        send(
            &http,
            reqwest::Method::GET,
            format!("{host}/auth/derive-api-key"),
            headers,
        )
        .await
    };

    let raw = match mode {
        "create" => create().await?,
        "derive" => derive().await?,
        "create-or-derive" | "create_or_derive" => match create().await {
            Ok(creds) => creds,
            Err(_) => derive().await?,
        },
        _ => match create().await {
            Ok(creds) => creds,
            Err(_) => derive().await?,
        },
    };
    Ok(Credentials::new(
        Uuid::parse_str(&raw.api_key).context("invalid api key uuid")?,
        raw.secret,
        raw.passphrase,
    ))
}

fn parse_signature_type(raw: &str) -> Result<SignatureType> {
    Ok(match raw.trim().to_ascii_lowercase().as_str() {
        "" | "0" | "eoa" => SignatureType::Eoa,
        "1" | "proxy" | "poly_proxy" | "poly-proxy" => SignatureType::Proxy,
        "2" | "safe" | "gnosis" | "gnosis_safe" | "gnosis-safe" => SignatureType::GnosisSafe,
        "3" | "poly_1271" | "poly-1271" | "poly1271" | "deposit_wallet" | "deposit-wallet" => {
            SignatureType::Poly1271
        }
        other => anyhow::bail!("unsupported POLYMARKET_SIGNATURE_TYPE `{other}`"),
    })
}

fn env_funder() -> Result<Option<Address>> {
    let Some(raw) = env_optional("POLYMARKET_FUNDER_ADDRESS")
        .or_else(|| env_optional("POLYMARKET_FUNDER"))
        .or_else(|| env_optional("POLYMARKET_PROXY_WALLET_ADDRESS"))
    else {
        return Ok(None);
    };
    Address::from_str(raw.as_str())
        .map(Some)
        .with_context(|| format!("invalid configured funder address `{raw}`"))
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
    let signature_type = parse_signature_type(
        env_optional("POLYMARKET_SIGNATURE_TYPE")
            .unwrap_or_else(|| "eoa".to_string())
            .as_str(),
    )?;
    let funder = env_funder()?;

    let signer = LocalSigner::from_str(private_key.trim())
        .context("failed to parse POLYMARKET_PRIVATE_KEY")?
        .with_chain_id(Some(POLYGON));
    let client = ClobClient::new(clob_api_url.as_str(), ClobConfig::default())
        .context("failed to create Polymarket CLOB client")?;
    let credentials = if signature_type == SignatureType::Poly1271 {
        let funder = funder.context("POLYMARKET_FUNDER_ADDRESS is required for poly_1271")?;
        poly1271_credentials_for_mode(clob_api_url.as_str(), &signer, funder, mode.as_str()).await?
    } else {
        credentials_for_mode(client, &signer, mode.as_str(), signature_type, funder)
            .await
            .with_context(|| {
                format!("failed to {mode} Polymarket CLOB API credentials via {clob_api_url}")
            })?
    };

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
