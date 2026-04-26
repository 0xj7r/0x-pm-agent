//! EOA-mode Polygon RPC submitter for CTF merge/redeem transactions.
//!
//! When a wallet runs in EOA mode (signature_type=0), Polymarket's gasless
//! relayer cannot proxy CTF calls (it only accepts proxy meta-tx). The bot
//! must sign its own transactions and submit them to Polygon directly,
//! paying MATIC gas. This module wraps that flow so `relayer.merge_positions`
//! and `relayer.redeem_positions` can dispatch to it transparently.

use std::time::Duration;

use alloy::network::EthereumWallet;
use alloy::primitives::{Address, Bytes, B256, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;

use crate::wire::execution_adapter::ExecutionError;

const DEFAULT_RECEIPT_TIMEOUT: Duration = Duration::from_secs(60);
const DEFAULT_GAS_LIMIT: u64 = 250_000;
pub const POLYGON_CHAIN_ID: u64 = 137;

#[derive(Clone)]
pub struct EoaPolygonSubmitter {
    rpc_url: String,
    gas_limit: u64,
    receipt_timeout: Duration,
}

impl EoaPolygonSubmitter {
    pub fn new(rpc_url: String) -> Self {
        Self {
            rpc_url,
            gas_limit: DEFAULT_GAS_LIMIT,
            receipt_timeout: DEFAULT_RECEIPT_TIMEOUT,
        }
    }

    pub fn rpc_url(&self) -> &str {
        &self.rpc_url
    }

    /// Build, sign, submit, and await receipt for a single contract call.
    /// Returns the transaction hash on success. Errors map to ExecutionError
    /// so the caller can route them through the existing risk-off ladder.
    pub async fn submit_call(
        &self,
        signer: &PrivateKeySigner,
        to: Address,
        data: Bytes,
    ) -> Result<B256, ExecutionError> {
        let url = self.rpc_url.parse::<reqwest::Url>().map_err(|error| {
            ExecutionError::BadRequest(format!(
                "invalid POLYGON_RPC_URL `{}`: {error}",
                self.rpc_url
            ))
        })?;
        let wallet = EthereumWallet::from(signer.clone());
        let provider = ProviderBuilder::new().wallet(wallet).connect_http(url);

        let from = signer.address();
        let request = TransactionRequest::default()
            .to(to)
            .from(from)
            .input(data.into())
            .value(U256::ZERO)
            .gas_limit(self.gas_limit);

        let pending = provider.send_transaction(request).await.map_err(|error| {
            let text = error.to_string();
            // Polygon throttles delegated accounts; bubble as transient so the
            // runner does not immediately go risk-off.
            if text.contains("in-flight transaction limit")
                || text.contains("nonce too low")
                || text.contains("replacement transaction underpriced")
            {
                ExecutionError::TransientNetwork(format!("polygon rpc throttle: {text}"))
            } else {
                ExecutionError::VenueRejection(format!("polygon submit failed: {text}"))
            }
        })?;

        let tx_hash = *pending.tx_hash();
        let receipt = pending
            .with_timeout(Some(self.receipt_timeout))
            .get_receipt()
            .await
            .map_err(|error| {
                ExecutionError::TransientNetwork(format!(
                    "polygon receipt timeout for {tx_hash:?}: {error}"
                ))
            })?;
        if !receipt.status() {
            return Err(ExecutionError::VenueRejection(format!(
                "polygon tx {tx_hash:?} reverted (block {})",
                receipt.block_number.unwrap_or_default()
            )));
        }
        Ok(tx_hash)
    }
}
