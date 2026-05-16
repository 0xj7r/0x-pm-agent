//! EOA-mode Polygon RPC submitter for CTF merge/redeem transactions.
//!
//! When a wallet runs in EOA mode (signature_type=0), Polymarket's gasless
//! relayer cannot proxy CTF calls (it only accepts proxy meta-tx). The bot
//! must sign its own transactions and submit them to Polygon directly,
//! paying MATIC gas. This module wraps that flow so `relayer.merge_positions`
//! and `relayer.redeem_positions` can dispatch to it transparently.

use alloy::network::EthereumWallet;
use alloy::primitives::{address, Address, Bytes, B256, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use alloy::sol_types::SolCall;

use crate::wire::execution_adapter::ExecutionError;
use crate::wire::polygon_rpc::{
    missing_rpc_error, PolygonRpcConfig, DEFAULT_POLYGON_RPC_GAS_LIMIT,
    DEFAULT_POLYGON_RPC_RECEIPT_TIMEOUT,
};

pub const POLYGON_CHAIN_ID: u64 = 137;
pub const COLLATERAL_ONRAMP: Address = address!("93070a847efEf7F70739046A929D47a521F5B8ee");
pub const USDCE: Address = address!("2791Bca1f2de4661ED88A30C99A7a9449Aa84174");
pub const PUSD: Address = address!("C011a7E12a19f7B1f670d46F03B03f3342E82DFB");

sol! {
    #[derive(Debug, PartialEq)]
    function balanceOf(address account) view returns (uint256);

    #[derive(Debug, PartialEq)]
    function allowance(address owner, address spender) view returns (uint256);

    #[derive(Debug, PartialEq)]
    function approve(address spender, uint256 amount) returns (bool);

    #[derive(Debug, PartialEq)]
    function wrap(address _asset, address _to, uint256 _amount);
}

#[derive(Clone)]
pub struct EoaPolygonSubmitter {
    rpc: PolygonRpcConfig,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PusdWrapReport {
    pub wallet: Address,
    pub usdce_balance_before: U256,
    pub pusd_balance_before: U256,
    pub onramp_allowance_before: U256,
    pub wrapped_amount: U256,
    pub approve_tx_hash: Option<B256>,
    pub wrap_tx_hash: Option<B256>,
}

impl EoaPolygonSubmitter {
    pub fn new(rpc_url: String) -> Self {
        Self {
            rpc: PolygonRpcConfig::from_parts(
                rpc_url,
                std::iter::empty::<String>(),
                crate::wire::polygon_rpc::DEFAULT_POLYGON_RPC_REQUEST_TIMEOUT,
                DEFAULT_POLYGON_RPC_RECEIPT_TIMEOUT,
                DEFAULT_POLYGON_RPC_GAS_LIMIT,
            ),
        }
    }

    pub fn from_env(rpc_url: String) -> Self {
        Self {
            rpc: PolygonRpcConfig::from_primary_and_env(rpc_url),
        }
    }

    pub fn with_rpc_config(rpc: PolygonRpcConfig) -> Self {
        Self { rpc }
    }

    pub fn rpc_url(&self) -> &str {
        self.rpc.primary_url().unwrap_or("")
    }

    pub fn rpc_config(&self) -> &PolygonRpcConfig {
        &self.rpc
    }

    pub async fn ensure_pusd_from_usdce(
        &self,
        signer: &PrivateKeySigner,
        recipient: Address,
        min_wrap_amount: U256,
    ) -> Result<PusdWrapReport, ExecutionError> {
        let wallet = signer.address();
        let usdce_balance = self.erc20_balance(USDCE, wallet).await?;
        let pusd_balance = self.erc20_balance(PUSD, recipient).await?;
        let allowance = self
            .erc20_allowance(USDCE, wallet, COLLATERAL_ONRAMP)
            .await?;
        if usdce_balance < min_wrap_amount || usdce_balance.is_zero() {
            return Ok(PusdWrapReport {
                wallet,
                usdce_balance_before: usdce_balance,
                pusd_balance_before: pusd_balance,
                onramp_allowance_before: allowance,
                wrapped_amount: U256::ZERO,
                approve_tx_hash: None,
                wrap_tx_hash: None,
            });
        }

        let approve_tx_hash = if allowance < usdce_balance {
            let calldata = approveCall {
                spender: COLLATERAL_ONRAMP,
                amount: usdce_balance,
            }
            .abi_encode();
            Some(
                self.submit_call(signer, USDCE, Bytes::from(calldata))
                    .await?,
            )
        } else {
            None
        };
        let calldata = wrapCall {
            _asset: USDCE,
            _to: recipient,
            _amount: usdce_balance,
        }
        .abi_encode();
        let wrap_tx_hash = self
            .submit_call(signer, COLLATERAL_ONRAMP, Bytes::from(calldata))
            .await?;
        Ok(PusdWrapReport {
            wallet,
            usdce_balance_before: usdce_balance,
            pusd_balance_before: pusd_balance,
            onramp_allowance_before: allowance,
            wrapped_amount: usdce_balance,
            approve_tx_hash,
            wrap_tx_hash: Some(wrap_tx_hash),
        })
    }

    pub async fn erc20_balance(
        &self,
        token: Address,
        owner: Address,
    ) -> Result<U256, ExecutionError> {
        let calldata = balanceOfCall { account: owner }.abi_encode();
        let bytes = self.eth_call(token, Bytes::from(calldata)).await?;
        balanceOfCall::abi_decode_returns(&bytes).map_err(|error| {
            ExecutionError::VenueRejection(format!("failed to decode ERC20 balanceOf: {error}"))
        })
    }

    async fn erc20_allowance(
        &self,
        token: Address,
        owner: Address,
        spender: Address,
    ) -> Result<U256, ExecutionError> {
        let calldata = allowanceCall { owner, spender }.abi_encode();
        let bytes = self.eth_call(token, Bytes::from(calldata)).await?;
        allowanceCall::abi_decode_returns(&bytes).map_err(|error| {
            ExecutionError::VenueRejection(format!("failed to decode ERC20 allowance: {error}"))
        })
    }

    async fn eth_call(&self, to: Address, data: Bytes) -> Result<Bytes, ExecutionError> {
        self.eth_call_from(None, to, data).await
    }

    pub async fn simulate_call(
        &self,
        from: Address,
        to: Address,
        data: Bytes,
    ) -> Result<Bytes, ExecutionError> {
        self.eth_call_from(Some(from), to, data).await
    }

    async fn eth_call_from(
        &self,
        from: Option<Address>,
        to: Address,
        data: Bytes,
    ) -> Result<Bytes, ExecutionError> {
        let mut last_error: Option<ExecutionError> = None;
        for endpoint in self.rpc.endpoints() {
            let url = match endpoint.url().parse::<reqwest::Url>() {
                Ok(url) => url,
                Err(error) => {
                    last_error = Some(ExecutionError::BadRequest(format!(
                        "invalid POLYGON_RPC_URL `{}`: {error}",
                        endpoint.redacted_url()
                    )));
                    continue;
                }
            };
            let provider = ProviderBuilder::new().connect_http(url);
            let mut request = TransactionRequest::default()
                .to(to)
                .input(data.clone().into());
            if let Some(from) = from {
                request = request.from(from);
            }
            match provider.call(request).await {
                Ok(bytes) => return Ok(bytes),
                Err(error) => {
                    last_error = Some(ExecutionError::TransientNetwork(format!(
                        "polygon eth_call failed via {}: {error}",
                        endpoint.redacted_url()
                    )));
                }
            }
        }
        Err(last_error.unwrap_or_else(|| missing_rpc_error("eth_call")))
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
        let primary = self
            .rpc
            .primary()
            .ok_or_else(|| missing_rpc_error("transaction submit"))?;
        let url = primary.url().parse::<reqwest::Url>().map_err(|error| {
            ExecutionError::BadRequest(format!(
                "invalid POLYGON_RPC_URL `{}`: {error}",
                primary.redacted_url()
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
            .gas_limit(self.rpc.gas_limit());

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
            .with_timeout(Some(self.rpc.receipt_timeout()))
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

pub fn scaled_usdc_units(amount_usd: f64) -> Result<U256, ExecutionError> {
    if !amount_usd.is_finite() || amount_usd < 0.0 {
        return Err(ExecutionError::BadRequest(format!(
            "USDC amount must be finite and non-negative, got {amount_usd}"
        )));
    }
    Ok(U256::from((amount_usd * 1_000_000.0).ceil() as u128))
}

pub fn usdc_units_to_f64(amount: U256) -> f64 {
    amount.to::<u128>() as f64 / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::hex::ToHexExt as _;

    #[test]
    fn scales_min_wrap_amount_to_usdc_base_units() {
        assert_eq!(scaled_usdc_units(0.01).unwrap(), U256::from(10_000_u64));
        assert_eq!(
            scaled_usdc_units(1.234567).unwrap(),
            U256::from(1_234_567_u64)
        );
    }

    #[test]
    fn wraps_with_documented_onramp_and_usdce_addresses() {
        let wallet = address!("1111111111111111111111111111111111111111");
        let amount = U256::from(5_000_000_u64);
        let approve = approveCall {
            spender: COLLATERAL_ONRAMP,
            amount,
        }
        .abi_encode()
        .encode_hex_with_prefix();
        let wrap = wrapCall {
            _asset: USDCE,
            _to: wallet,
            _amount: amount,
        }
        .abi_encode()
        .encode_hex_with_prefix();
        let decoded_wrap = wrapCall::abi_decode(
            &wrapCall {
                _asset: USDCE,
                _to: wallet,
                _amount: amount,
            }
            .abi_encode(),
        )
        .expect("generated wrap calldata decodes");

        assert!(approve.starts_with("0x095ea7b3"));
        assert!(wrap.starts_with("0x"));
        assert_eq!(decoded_wrap._asset, USDCE);
        assert_eq!(decoded_wrap._to, wallet);
        assert_eq!(decoded_wrap._amount, amount);
        assert_eq!(
            COLLATERAL_ONRAMP.to_string(),
            "0x93070a847efEf7F70739046A929D47a521F5B8ee"
        );
        assert_eq!(
            USDCE.to_string(),
            "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174"
        );
    }
}
