//! One-shot CTF redeem tool. Loads tinylive env, constructs the same
//! CtfRelayerClient the bot's auto-redeem worker uses, and submits a single
//! redeem for the given condition_id. Does NOT enter the runtime loop, does
//! NOT subscribe to markets, does NOT touch strategy or quote code paths.
//!
//! Usage (env must be loaded first, e.g. `set -a && source ~/.config/polymarket-exec/btc_5m_mm_tinylive.env && set +a`):
//!   redeem_once --condition-id 0xabc... [--index-sets 1,2] [--collateral 0x...]

use std::str::FromStr;

use anyhow::{anyhow, Context, Result};
use polymarket_exec::wire::execution_adapter::PolymarketSignatureType;
use polymarket_exec::wire::relayer::{
    CtfRedeemRequest, CtfRelayerClient, CtfRelayerConfig, DEFAULT_CTF_ADDRESS, DEFAULT_RELAYER_URL,
};

#[tokio::main]
async fn main() -> Result<()> {
    let mut condition_id: Option<String> = None;
    let mut index_sets: Vec<u64> = vec![1, 2];
    let mut collateral_override: Option<String> = None;

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--condition-id" => {
                condition_id = Some(
                    iter.next()
                        .ok_or_else(|| anyhow!("--condition-id requires a value"))?
                        .clone(),
                );
            }
            "--index-sets" => {
                let raw = iter
                    .next()
                    .ok_or_else(|| anyhow!("--index-sets requires a value"))?;
                index_sets = raw
                    .split(',')
                    .map(|s| s.trim().parse::<u64>())
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .context("--index-sets must be comma-separated integers")?;
            }
            "--collateral" => {
                collateral_override = Some(
                    iter.next()
                        .ok_or_else(|| anyhow!("--collateral requires a value"))?
                        .clone(),
                );
            }
            "-h" | "--help" => {
                println!(
                    "redeem_once --condition-id 0x... [--index-sets 1,2] [--collateral 0x...]"
                );
                return Ok(());
            }
            other => return Err(anyhow!("unknown arg: {other}")),
        }
    }

    let condition_id = condition_id.ok_or_else(|| anyhow!("--condition-id is required"))?;

    let private_key = std::env::var("POLYMARKET_PRIVATE_KEY")
        .or_else(|_| std::env::var("METAMASK_PRIVATE_KEY"))
        .context("POLYMARKET_PRIVATE_KEY must be set in env")?;
    let signer = alloy::signers::local::PrivateKeySigner::from_str(private_key.trim())
        .context("failed to parse POLYMARKET_PRIVATE_KEY")?;
    let signer_address = format!("{:?}", signer.address());

    let signature_type_raw =
        std::env::var("POLYMARKET_SIGNATURE_TYPE").unwrap_or_else(|_| "eoa".to_string());
    let signature_type = PolymarketSignatureType::parse(&signature_type_raw)
        .map_err(|e| anyhow!("failed to parse signature type: {e:?}"))?;
    let signature_type_code = signature_type.as_polymarket_code();

    let config = CtfRelayerConfig {
        relayer_url: std::env::var("POLYMARKET_RELAYER_URL")
            .unwrap_or_else(|_| DEFAULT_RELAYER_URL.to_string()),
        api_key: std::env::var("RELAYER_API_KEY").ok(),
        api_key_address: std::env::var("RELAYER_API_KEY_ADDRESS").ok(),
        ctf_contract_address: std::env::var("POLYMARKET_CTF_CONTRACT_ADDRESS")
            .unwrap_or_else(|_| DEFAULT_CTF_ADDRESS.to_string()),
        collateral_token_address: std::env::var("POLYMARKET_CTF_COLLATERAL_TOKEN_ADDRESS")
            .or_else(|_| std::env::var("POLYMARKET_COLLATERAL_TOKEN_ADDRESS"))
            .context("POLYMARKET_CTF_COLLATERAL_TOKEN_ADDRESS must be set")?,
        collateral_decimals: std::env::var("POLYMARKET_COLLATERAL_DECIMALS")
            .ok()
            .and_then(|v| v.trim().parse::<u8>().ok())
            .unwrap_or(6),
        signature_type_code,
        proxy_wallet_address: std::env::var("POLYMARKET_PROXY_WALLET_ADDRESS")
            .ok()
            .or_else(|| std::env::var("POLYMARKET_PROXY_WALLET").ok())
            .or_else(|| std::env::var("POLYMARKET_FUNDER_ADDRESS").ok())
            .or_else(|| std::env::var("POLYMARKET_FUNDER").ok()),
        polygon_rpc_url: std::env::var("POLYGON_RPC_URL").ok(),
    };

    eprintln!("=== redeem_once configuration ===");
    eprintln!("  condition_id: {condition_id}");
    eprintln!("  index_sets:   {index_sets:?}");
    eprintln!(
        "  collateral:   {} (override: {})",
        collateral_override
            .as_deref()
            .unwrap_or(&config.collateral_token_address),
        collateral_override.is_some()
    );
    eprintln!("  ctf_address:  {}", config.ctf_contract_address);
    eprintln!("  relayer_url:  {}", config.relayer_url);
    eprintln!(
        "  sig_type:     {:?} (code={})",
        signature_type, signature_type_code
    );
    eprintln!("  signer:       {signer_address}");
    eprintln!("  proxy_wallet: {:?}", config.proxy_wallet_address);
    eprintln!();

    let client = CtfRelayerClient::new(config);
    let request = CtfRedeemRequest {
        signer,
        condition_id: condition_id.clone(),
        collateral_token_address: collateral_override,
        index_sets,
        metadata: format!("redeem_once tool condition={condition_id}"),
    };

    eprintln!("submitting redeem...");
    match client.redeem_positions(request).await {
        Ok(ack) => {
            println!("OK");
            println!("  transaction_id:   {:?}", ack.transaction_id);
            println!("  state:            {:?}", ack.state);
            println!("  transaction_hash: {:?}", ack.transaction_hash);
            Ok(())
        }
        Err(e) => {
            eprintln!("ERROR: {e:?}");
            Err(anyhow!("redeem failed: {e:?}"))
        }
    }
}
