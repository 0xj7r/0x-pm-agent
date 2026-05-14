//! One-shot CTF merge tool. Builds the same merge calldata the live runtime
//! uses, optionally simulates it with Polygon `eth_call`, and can submit the
//! merge without entering the strategy/runtime loop.
//!
//! Usage (env must be loaded first):
//!   merge_once --condition-id 0xabc... --quantity 5 --dry-run
//!   merge_once --condition-id 0xabc... --quantity 5 --relayer-envelope-only
//!   merge_once --condition-id 0xabc... --quantity 5

use std::str::FromStr;

use anyhow::{anyhow, Context, Result};
use polymarket_exec::wire::execution_adapter::PolymarketSignatureType;
use polymarket_exec::wire::polygon_rpc::redact_rpc_url;
use polymarket_exec::wire::relayer::{
    CtfMergeRequest, CtfRelayerClient, CtfRelayerConfig, DEFAULT_CTF_ADDRESS, DEFAULT_PUSD_ADDRESS,
    DEFAULT_RELAYER_URL,
};

#[tokio::main]
async fn main() -> Result<()> {
    let mut condition_id: Option<String> = None;
    let mut quantity: Option<f64> = None;
    let mut dry_run = false;
    let mut relayer_envelope_only = false;

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
            "--quantity" => {
                let raw = iter
                    .next()
                    .ok_or_else(|| anyhow!("--quantity requires a value"))?;
                quantity = Some(
                    raw.trim()
                        .parse::<f64>()
                        .context("--quantity must be a positive decimal")?,
                );
            }
            "--dry-run" => dry_run = true,
            "--relayer-envelope-only" => relayer_envelope_only = true,
            "-h" | "--help" => {
                println!(
                    "merge_once --condition-id 0x... --quantity 5 [--dry-run|--relayer-envelope-only]"
                );
                return Ok(());
            }
            other => return Err(anyhow!("unknown arg: {other}")),
        }
    }

    let condition_id = condition_id.ok_or_else(|| anyhow!("--condition-id is required"))?;
    let quantity = quantity
        .filter(|value| value.is_finite() && *value > 0.0)
        .ok_or_else(|| anyhow!("--quantity must be positive"))?;

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
    let signature_type_code = signature_type.as_ctf_relayer_code();

    let polygon_rpc_url = std::env::var("POLYGON_RPC_URL").ok();
    let config = CtfRelayerConfig {
        relayer_url: std::env::var("POLYMARKET_RELAYER_URL")
            .unwrap_or_else(|_| DEFAULT_RELAYER_URL.to_string()),
        api_key: std::env::var("RELAYER_API_KEY")
            .or_else(|_| std::env::var("POLYMARKET_RELAYER_API_KEY"))
            .ok(),
        api_key_address: std::env::var("RELAYER_API_KEY_ADDRESS")
            .or_else(|_| std::env::var("POLYMARKET_RELAYER_API_KEY_ADDRESS"))
            .ok(),
        ctf_contract_address: std::env::var("POLYMARKET_CTF_CONTRACT_ADDRESS")
            .unwrap_or_else(|_| DEFAULT_CTF_ADDRESS.to_string()),
        collateral_token_address: std::env::var("POLYMARKET_CTF_COLLATERAL_TOKEN_ADDRESS")
            .or_else(|_| std::env::var("POLYMARKET_COLLATERAL_TOKEN_ADDRESS"))
            .unwrap_or_else(|_| DEFAULT_PUSD_ADDRESS.to_string()),
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
        polygon_rpc_url,
    };

    eprintln!("=== merge_once configuration ===");
    eprintln!("  condition_id: {condition_id}");
    eprintln!("  quantity:     {quantity}");
    eprintln!("  collateral:   {}", config.collateral_token_address);
    eprintln!("  ctf_address:  {}", config.ctf_contract_address);
    eprintln!("  relayer_url:  {}", config.relayer_url);
    eprintln!(
        "  rpc_url:      {}",
        config
            .polygon_rpc_url
            .as_deref()
            .map(redact_rpc_url)
            .unwrap_or_else(|| "(none)".to_string())
    );
    eprintln!(
        "  sig_type:     {:?} (code={})",
        signature_type, signature_type_code
    );
    eprintln!("  signer:       {signer_address}");
    eprintln!("  proxy_wallet: {:?}", config.proxy_wallet_address);
    eprintln!("  dry_run:      {dry_run}");
    eprintln!("  envelope_only:{relayer_envelope_only}");
    eprintln!();

    let client = CtfRelayerClient::new(config);
    let request = CtfMergeRequest {
        signer,
        condition_id: condition_id.clone(),
        quantity,
        metadata: format!("merge_once tool condition={condition_id} quantity={quantity}"),
    };

    if relayer_envelope_only {
        if signature_type_code != 3 {
            return Err(anyhow!(
                "--relayer-envelope-only is implemented for POLY_1271/WALLET mode"
            ));
        }
        eprintln!("building POLY_1271 WALLET relayer envelope without submitting...");
        let envelope = client.dry_run_merge_submission_envelope(&request).await?;
        println!("OK relayer-envelope dry-run");
        println!("  type:         {}", envelope.tx_type);
        println!("  from:         {:?}", envelope.from);
        println!("  to:           {:?}", envelope.to);
        println!("  wallet:       {:?}", envelope.deposit_wallet);
        println!("  nonce:        {}", envelope.nonce);
        println!("  calls:        {}", envelope.call_count);
        println!("  sig_bytes:    {}", envelope.signature_bytes);
        return Ok(());
    }

    if dry_run {
        eprintln!("simulating merge via eth_call...");
        match client.dry_run_merge_positions(&request).await {
            Ok(report) => {
                println!("OK dry-run");
                println!("  from:         {:?}", report.from);
                println!("  to:           {:?}", report.to);
                println!("  calldata:     {}", report.calldata_hex);
                if signature_type_code == 3 {
                    eprintln!("building POLY_1271 WALLET relayer envelope without submitting...");
                    let envelope = client.dry_run_merge_submission_envelope(&request).await?;
                    println!("OK relayer-envelope dry-run");
                    println!("  type:         {}", envelope.tx_type);
                    println!("  from:         {:?}", envelope.from);
                    println!("  to:           {:?}", envelope.to);
                    println!("  wallet:       {:?}", envelope.deposit_wallet);
                    println!("  nonce:        {}", envelope.nonce);
                    println!("  calls:        {}", envelope.call_count);
                    println!("  sig_bytes:    {}", envelope.signature_bytes);
                }
                return Ok(());
            }
            Err(error) => {
                eprintln!("ERROR: {error:?}");
                return Err(anyhow!("merge dry-run failed: {error:?}"));
            }
        }
    }

    eprintln!("submitting merge...");
    match client.merge_positions(request).await {
        Ok(ack) => {
            println!("OK");
            println!("  transaction_id:   {:?}", ack.transaction_id);
            println!("  state:            {:?}", ack.state);
            println!("  transaction_hash: {:?}", ack.transaction_hash);
            Ok(())
        }
        Err(error) => {
            eprintln!("ERROR: {error:?}");
            Err(anyhow!("merge failed: {error:?}"))
        }
    }
}
