//! Polymarket builder-relayer helpers for CTF split/merge/redeem transactions.

use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use alloy::dyn_abi::Eip712Domain;
use alloy::hex::ToHexExt as _;
use alloy::primitives::{address, keccak256, Address, Bytes, B256, U256};
use alloy::signers::local::PrivateKeySigner;
use alloy::signers::Signer;
use alloy::sol;
use alloy::sol_types::{SolCall, SolStruct as _};
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE};
use serde::{Deserialize, Serialize};

use crate::wire::eoa_polygon::EoaPolygonSubmitter;
use crate::wire::execution_adapter::ExecutionError;

pub const DEFAULT_RELAYER_URL: &str = "https://relayer-v2.polymarket.com";
pub const DEFAULT_CTF_ADDRESS: &str = "0x4D97DCd97eC945f40cF65F87097ACe5EA0476045";
pub const DEFAULT_USDCE_ADDRESS: &str = "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174";
pub const DEFAULT_PUSD_ADDRESS: &str = "0xC011a7E12a19f7B1f670d46F03B03f3342E82DFB";
pub const COLLATERAL_ONRAMP_ADDRESS: &str = "0x93070a847efEf7F70739046A929D47a521F5B8ee";

const POLYMARKET_PROXY_FACTORY: Address = address!("aB45c5A4B0c941a2F231C04C3f49182e1A254052");
const POLYMARKET_RELAY_HUB: Address = address!("D216153c06E857cD7f72665E0aF1d7D82172F494");
const POLYMARKET_DEPOSIT_WALLET_FACTORY: Address =
    address!("00000000000Fb5C9ADea0298D729A0CB3823Cc07");
const PROXY_INIT_CODE_HASH: &str =
    "0xd21df8dc65880a8606f09fe0ce3df9b8869287ab0b058be05aa9e8af6330a00b";
const DEFAULT_PROXY_GAS_LIMIT: u64 = 10_000_000;
const DEFAULT_WALLET_BATCH_DEADLINE_SECS: u64 = 600;

sol! {
    #[derive(Debug, PartialEq)]
    function splitPosition(
        address collateralToken,
        bytes32 parentCollectionId,
        bytes32 conditionId,
        uint256[] partition,
        uint256 amount
    );

    #[derive(Debug, PartialEq)]
    function mergePositions(
        address collateralToken,
        bytes32 parentCollectionId,
        bytes32 conditionId,
        uint256[] partition,
        uint256 amount
    );

    #[derive(Debug, PartialEq)]
    function redeemPositions(
        address collateralToken,
        bytes32 parentCollectionId,
        bytes32 conditionId,
        uint256[] indexSets
    );

    #[derive(Debug, PartialEq)]
    struct ProxyTransactionCall {
        address to;
        uint8 typeCode;
        bytes data;
        uint256 value;
    }

    #[derive(Debug, PartialEq)]
    function proxy(ProxyTransactionCall[] transactions);

    #[derive(Debug, PartialEq)]
    struct Call {
        address target;
        uint256 value;
        bytes data;
    }

    #[derive(Debug, PartialEq)]
    struct Batch {
        address wallet;
        uint256 nonce;
        uint256 deadline;
        Call[] calls;
    }

    #[derive(Debug, PartialEq)]
    function approve(address spender, uint256 amount) external returns (bool);

    #[derive(Debug, PartialEq)]
    function wrap(address _asset, address _to, uint256 _amount) external;
}

#[derive(Clone, Debug, PartialEq)]
pub struct CtfRelayerConfig {
    pub relayer_url: String,
    pub api_key: Option<String>,
    pub api_key_address: Option<String>,
    pub ctf_contract_address: String,
    pub collateral_token_address: String,
    pub collateral_decimals: u8,
    pub signature_type_code: u8,
    pub proxy_wallet_address: Option<String>,
    pub polygon_rpc_url: Option<String>,
}

#[derive(Clone)]
pub struct CtfRelayerClient {
    config: CtfRelayerConfig,
    http: reqwest::Client,
    eoa_submitter: Option<EoaPolygonSubmitter>,
}

#[derive(Clone, Debug)]
pub struct CtfMergeRequest {
    pub signer: PrivateKeySigner,
    pub condition_id: String,
    pub quantity: f64,
    pub metadata: String,
}

/// Split pUSD into a full Up+Down set (same shape as merge).
#[derive(Clone, Debug)]
pub struct CtfSplitRequest {
    pub signer: PrivateKeySigner,
    pub condition_id: String,
    pub quantity: f64,
    pub metadata: String,
}

#[derive(Clone, Debug)]
pub struct CtfRedeemRequest {
    pub signer: PrivateKeySigner,
    pub condition_id: String,
    /// Optional collateral override for legacy redemptions. Defaults to the
    /// relayer config collateral, which is the active trading collateral.
    pub collateral_token_address: Option<String>,
    /// Outcome index sets to redeem. For binary markets pass `vec![1, 2]`
    /// to claim both legs (winning leg pays, losing leg returns nothing
    /// but the call still succeeds atomically).
    pub index_sets: Vec<u64>,
    pub metadata: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RelayerSubmitAck {
    pub transaction_id: Option<String>,
    pub state: Option<String>,
    pub transaction_hash: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CtfMergeDryRunReport {
    pub from: Address,
    pub to: Address,
    pub calldata_hex: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CtfRelayerEnvelopeDryRunReport {
    pub tx_type: String,
    pub from: Address,
    pub to: Address,
    pub deposit_wallet: Option<Address>,
    pub nonce: String,
    pub call_count: usize,
    pub signature_bytes: usize,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RelayPayload {
    address: String,
    nonce: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NoncePayload {
    nonce: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubmitResponse {
    #[serde(default, alias = "transactionID")]
    transaction_id: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default, alias = "hash")]
    transaction_hash: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProxySignatureParams {
    gas_price: String,
    gas_limit: String,
    relayer_fee: String,
    relay_hub: String,
    relay: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TransactionRequest {
    #[serde(rename = "type")]
    tx_type: String,
    from: String,
    to: String,
    proxy_wallet: String,
    data: String,
    nonce: String,
    signature: String,
    signature_params: ProxySignatureParams,
    metadata: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WalletTransactionRequest {
    #[serde(rename = "type")]
    tx_type: String,
    from: String,
    to: String,
    nonce: String,
    signature: String,
    deposit_wallet_params: DepositWalletParams,
    metadata: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DepositWalletParams {
    deposit_wallet: String,
    deadline: String,
    calls: Vec<DepositWalletCallRequest>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DepositWalletCallRequest {
    target: String,
    value: String,
    data: String,
}

impl CtfRelayerClient {
    pub fn new(config: CtfRelayerConfig) -> Self {
        let eoa_submitter = if config.signature_type_code == 0 {
            config
                .polygon_rpc_url
                .as_ref()
                .filter(|url| !url.trim().is_empty())
                .map(|url| EoaPolygonSubmitter::from_env(url.clone()))
        } else {
            None
        };
        Self {
            config,
            http: reqwest::Client::new(),
            eoa_submitter,
        }
    }

    pub async fn split_positions(
        &self,
        request: CtfSplitRequest,
    ) -> Result<RelayerSubmitAck, ExecutionError> {
        match self.config.signature_type_code {
            1 | 2 => {
                let from = request.signer.address();
                let relay_payload = self.relay_payload(from, "PROXY").await?;
                let body = self
                    .build_proxy_split_transaction_request(
                        &request.signer,
                        relay_payload,
                        &request.condition_id,
                        request.quantity,
                        request.metadata,
                    )
                    .await?;
                self.submit(body).await
            }
            3 => self.submit_wallet_split(&request).await,
            0 => self.submit_eoa_split(&request).await,
            other => Err(ExecutionError::BadRequest(format!(
                "CTF relayer split supports POLYMARKET_SIGNATURE_TYPE=0 (EOA), =1 (proxy), =2 (gnosis_safe), or =3 (poly_1271), got {other}",
            ))),
        }
    }

    pub async fn dry_run_split_positions(
        &self,
        request: &CtfSplitRequest,
    ) -> Result<CtfMergeDryRunReport, ExecutionError> {
        let rpc_url = self.config.polygon_rpc_url.as_ref().ok_or_else(|| {
            ExecutionError::BadRequest(
                "CTF split dry-run requires POLYGON_RPC_URL to be configured".to_string(),
            )
        })?;
        let submitter = EoaPolygonSubmitter::from_env(rpc_url.clone());
        let to = parse_address(&self.config.ctf_contract_address, "CTF contract")?;
        let from = self.merge_actor_address(&request.signer)?;
        let calldata = self.split_positions_calldata(&request.condition_id, request.quantity)?;
        submitter
            .simulate_call(from, to, Bytes::from(calldata.clone()))
            .await?;
        Ok(CtfMergeDryRunReport {
            from,
            to,
            calldata_hex: calldata.encode_hex_with_prefix(),
        })
    }

    pub async fn dry_run_split_submission_envelope(
        &self,
        request: &CtfSplitRequest,
    ) -> Result<CtfRelayerEnvelopeDryRunReport, ExecutionError> {
        match self.config.signature_type_code {
            3 => {
                let calldata =
                    self.split_positions_calldata(&request.condition_id, request.quantity)?;
                let body = self
                    .build_wallet_transaction_request(
                        &request.signer,
                        vec![wallet_call_request(
                            &self.config.ctf_contract_address,
                            Bytes::from(calldata),
                        )?],
                        request.metadata.clone(),
                    )
                    .await?;
                Ok(CtfRelayerEnvelopeDryRunReport {
                    tx_type: body.tx_type,
                    from: parse_address(&body.from, "dry-run from")?,
                    to: parse_address(&body.to, "dry-run to")?,
                    deposit_wallet: Some(parse_address(
                        &body.deposit_wallet_params.deposit_wallet,
                        "dry-run deposit wallet",
                    )?),
                    nonce: body.nonce,
                    call_count: body.deposit_wallet_params.calls.len(),
                    signature_bytes: signature_hex_len_bytes(&body.signature)?,
                })
            }
            other => Err(ExecutionError::BadRequest(format!(
                "split relayer envelope dry-run is implemented for POLYMARKET_SIGNATURE_TYPE=3 (poly_1271), got {other}",
            ))),
        }
    }

    pub async fn merge_positions(
        &self,
        request: CtfMergeRequest,
    ) -> Result<RelayerSubmitAck, ExecutionError> {
        match self.config.signature_type_code {
            1 | 2 => {
                let from = request.signer.address();
                let relay_payload = self.relay_payload(from, "PROXY").await?;
                let body = self
                    .build_proxy_merge_transaction_request(
                        &request.signer,
                        relay_payload,
                        &request.condition_id,
                        request.quantity,
                        request.metadata,
                    )
                    .await?;
                self.submit(body).await
            }
            3 => self.submit_wallet_merge(&request).await,
            0 => self.submit_eoa_merge(&request).await,
            other => Err(ExecutionError::BadRequest(format!(
                "CTF relayer merge supports POLYMARKET_SIGNATURE_TYPE=0 (EOA), =1 (proxy), =2 (gnosis_safe), or =3 (poly_1271), got {other}",
            ))),
        }
    }

    pub async fn dry_run_merge_positions(
        &self,
        request: &CtfMergeRequest,
    ) -> Result<CtfMergeDryRunReport, ExecutionError> {
        let rpc_url = self.config.polygon_rpc_url.as_ref().ok_or_else(|| {
            ExecutionError::BadRequest(
                "CTF merge dry-run requires POLYGON_RPC_URL to be configured".to_string(),
            )
        })?;
        let submitter = EoaPolygonSubmitter::from_env(rpc_url.clone());
        let to = parse_address(&self.config.ctf_contract_address, "CTF contract")?;
        let from = self.merge_actor_address(&request.signer)?;
        let calldata = self.merge_positions_calldata(&request.condition_id, request.quantity)?;
        submitter
            .simulate_call(from, to, Bytes::from(calldata.clone()))
            .await?;
        Ok(CtfMergeDryRunReport {
            from,
            to,
            calldata_hex: calldata.encode_hex_with_prefix(),
        })
    }

    pub async fn dry_run_merge_submission_envelope(
        &self,
        request: &CtfMergeRequest,
    ) -> Result<CtfRelayerEnvelopeDryRunReport, ExecutionError> {
        match self.config.signature_type_code {
            3 => {
                let calldata =
                    self.merge_positions_calldata(&request.condition_id, request.quantity)?;
                let body = self
                    .build_wallet_transaction_request(
                        &request.signer,
                        vec![wallet_call_request(
                            &self.config.ctf_contract_address,
                            Bytes::from(calldata),
                        )?],
                        request.metadata.clone(),
                    )
                    .await?;
                Ok(CtfRelayerEnvelopeDryRunReport {
                    tx_type: body.tx_type,
                    from: parse_address(&body.from, "dry-run from")?,
                    to: parse_address(&body.to, "dry-run to")?,
                    deposit_wallet: Some(parse_address(
                        &body.deposit_wallet_params.deposit_wallet,
                        "dry-run deposit wallet",
                    )?),
                    nonce: body.nonce,
                    call_count: body.deposit_wallet_params.calls.len(),
                    signature_bytes: signature_hex_len_bytes(&body.signature)?,
                })
            }
            other => Err(ExecutionError::BadRequest(format!(
                "merge relayer envelope dry-run is implemented for POLYMARKET_SIGNATURE_TYPE=3 (poly_1271), got {other}",
            ))),
        }
    }

    pub async fn redeem_positions(
        &self,
        request: CtfRedeemRequest,
    ) -> Result<RelayerSubmitAck, ExecutionError> {
        if request.index_sets.is_empty() {
            return Err(ExecutionError::BadRequest(
                "redeem index_sets must be non-empty".to_string(),
            ));
        }
        match self.config.signature_type_code {
            1 | 2 => {
                let from = request.signer.address();
                let relay_payload = self.relay_payload(from, "PROXY").await?;
                let body = self
                    .build_proxy_redeem_transaction_request(
                        &request.signer,
                        relay_payload,
                        &request.condition_id,
                        request.collateral_token_address.as_deref(),
                        &request.index_sets,
                        request.metadata,
                    )
                    .await?;
                self.submit(body).await
            }
            3 => self.submit_wallet_redeem(&request).await,
            0 => self.submit_eoa_redeem(&request).await,
            other => Err(ExecutionError::BadRequest(format!(
                "CTF relayer redeem supports POLYMARKET_SIGNATURE_TYPE=0 (EOA), =1 (proxy), =2 (gnosis_safe), or =3 (poly_1271), got {other}",
            ))),
        }
    }

    async fn submit_eoa_split(
        &self,
        request: &CtfSplitRequest,
    ) -> Result<RelayerSubmitAck, ExecutionError> {
        let submitter = self.eoa_submitter.as_ref().ok_or_else(|| {
            ExecutionError::BadRequest(
                "EOA mode CTF split requires POLYGON_RPC_URL to be configured".to_string(),
            )
        })?;
        let ctf = parse_address(&self.config.ctf_contract_address, "CTF contract")?;
        let calldata = self.split_positions_calldata(&request.condition_id, request.quantity)?;
        let tx_hash = submitter
            .submit_call(&request.signer, ctf, Bytes::from(calldata))
            .await?;
        Ok(RelayerSubmitAck {
            transaction_id: None,
            state: Some("MINED".to_string()),
            transaction_hash: Some(tx_hash.encode_hex_with_prefix()),
        })
    }

    async fn submit_eoa_merge(
        &self,
        request: &CtfMergeRequest,
    ) -> Result<RelayerSubmitAck, ExecutionError> {
        let submitter = self.eoa_submitter.as_ref().ok_or_else(|| {
            ExecutionError::BadRequest(
                "EOA mode CTF merge requires POLYGON_RPC_URL to be configured".to_string(),
            )
        })?;
        let ctf = parse_address(&self.config.ctf_contract_address, "CTF contract")?;
        let calldata = self.merge_positions_calldata(&request.condition_id, request.quantity)?;
        let tx_hash = submitter
            .submit_call(&request.signer, ctf, Bytes::from(calldata))
            .await?;
        Ok(RelayerSubmitAck {
            transaction_id: None,
            state: Some("MINED".to_string()),
            transaction_hash: Some(tx_hash.encode_hex_with_prefix()),
        })
    }

    fn merge_actor_address(&self, signer: &PrivateKeySigner) -> Result<Address, ExecutionError> {
        match self.config.signature_type_code {
            0 => Ok(signer.address()),
            1 | 2 => self.proxy_wallet(signer.address()),
            3 => self.deposit_wallet(),
            other => Err(ExecutionError::BadRequest(format!(
                "CTF merge dry-run supports POLYMARKET_SIGNATURE_TYPE=0 (EOA), =1 (proxy), =2 (gnosis_safe), or =3 (poly_1271), got {other}",
            ))),
        }
    }

    async fn submit_eoa_redeem(
        &self,
        request: &CtfRedeemRequest,
    ) -> Result<RelayerSubmitAck, ExecutionError> {
        let submitter = self.eoa_submitter.as_ref().ok_or_else(|| {
            ExecutionError::BadRequest(
                "EOA mode CTF redeem requires POLYGON_RPC_URL to be configured".to_string(),
            )
        })?;
        let ctf = parse_address(&self.config.ctf_contract_address, "CTF contract")?;
        let calldata = self.redeem_positions_calldata(
            request.collateral_token_address.as_deref(),
            &request.condition_id,
            &request.index_sets,
        )?;
        let tx_hash = submitter
            .submit_call(&request.signer, ctf, Bytes::from(calldata))
            .await?;
        Ok(RelayerSubmitAck {
            transaction_id: None,
            state: Some("MINED".to_string()),
            transaction_hash: Some(tx_hash.encode_hex_with_prefix()),
        })
    }

    async fn submit_wallet_split(
        &self,
        request: &CtfSplitRequest,
    ) -> Result<RelayerSubmitAck, ExecutionError> {
        let calldata = self.split_positions_calldata(&request.condition_id, request.quantity)?;
        let body = self
            .build_wallet_transaction_request(
                &request.signer,
                vec![wallet_call_request(
                    &self.config.ctf_contract_address,
                    Bytes::from(calldata),
                )?],
                request.metadata.clone(),
            )
            .await?;
        self.submit(body).await
    }

    async fn submit_wallet_merge(
        &self,
        request: &CtfMergeRequest,
    ) -> Result<RelayerSubmitAck, ExecutionError> {
        let calldata = self.merge_positions_calldata(&request.condition_id, request.quantity)?;
        let body = self
            .build_wallet_transaction_request(
                &request.signer,
                vec![wallet_call_request(
                    &self.config.ctf_contract_address,
                    Bytes::from(calldata),
                )?],
                request.metadata.clone(),
            )
            .await?;
        self.submit(body).await
    }

    /// Wrap USDC.e held by the deposit wallet (proxy) into pUSD via Polymarket's
    /// CollateralOnramp. This is the programmatic equivalent of clicking
    /// "Activate Funds" in the UI: merge proceeds land as USDC.e in the proxy
    /// and must be wrapped to pUSD before they show up as tradable balance.
    ///
    /// The batched WALLET tx contains two calls signed once by the EOA owner:
    ///   1. `USDC.e.approve(CollateralOnramp, amount)` from the proxy wallet
    ///   2. `CollateralOnramp.wrap(USDC.e, proxy_wallet, amount)`
    pub async fn wrap_usdce_to_pusd(
        &self,
        signer: &PrivateKeySigner,
        amount: U256,
        metadata: String,
    ) -> Result<RelayerSubmitAck, ExecutionError> {
        if self.config.signature_type_code != 3 {
            return Err(ExecutionError::BadRequest(format!(
                "wrap_usdce_to_pusd requires POLYMARKET_SIGNATURE_TYPE=3 (poly_1271), got {}",
                self.config.signature_type_code
            )));
        }
        if amount.is_zero() {
            return Err(ExecutionError::BadRequest(
                "wrap_usdce_to_pusd: amount must be > 0".to_string(),
            ));
        }
        let deposit_wallet = self.deposit_wallet()?;
        let onramp = parse_address(COLLATERAL_ONRAMP_ADDRESS, "collateral onramp")?;
        let usdce = parse_address(DEFAULT_USDCE_ADDRESS, "USDC.e")?;
        let approve_calldata = approveCall {
            spender: onramp,
            amount,
        }
        .abi_encode();
        let wrap_calldata = wrapCall {
            _asset: usdce,
            _to: deposit_wallet,
            _amount: amount,
        }
        .abi_encode();
        let body = self
            .build_wallet_transaction_request(
                signer,
                vec![
                    wallet_call_request(DEFAULT_USDCE_ADDRESS, Bytes::from(approve_calldata))?,
                    wallet_call_request(COLLATERAL_ONRAMP_ADDRESS, Bytes::from(wrap_calldata))?,
                ],
                metadata,
            )
            .await?;
        self.submit(body).await
    }

    /// Read the USDC.e balance currently held by the proxy/deposit wallet.
    /// Non-zero indicates pending merge proceeds awaiting `wrap_usdce_to_pusd`.
    pub async fn deposit_wallet_usdce_balance(&self) -> Result<U256, ExecutionError> {
        let rpc_url = self
            .config
            .polygon_rpc_url
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                ExecutionError::BadRequest(
                    "deposit_wallet_usdce_balance requires POLYGON_RPC_URL".to_string(),
                )
            })?;
        let submitter = EoaPolygonSubmitter::from_env(rpc_url.to_string());
        let usdce = parse_address(DEFAULT_USDCE_ADDRESS, "USDC.e")?;
        let wallet = self.deposit_wallet()?;
        submitter.erc20_balance(usdce, wallet).await
    }

    async fn submit_wallet_redeem(
        &self,
        request: &CtfRedeemRequest,
    ) -> Result<RelayerSubmitAck, ExecutionError> {
        let calldata = self.redeem_positions_calldata(
            request.collateral_token_address.as_deref(),
            &request.condition_id,
            &request.index_sets,
        )?;
        let body = self
            .build_wallet_transaction_request(
                &request.signer,
                vec![wallet_call_request(
                    &self.config.ctf_contract_address,
                    Bytes::from(calldata),
                )?],
                request.metadata.clone(),
            )
            .await?;
        self.submit(body).await
    }

    async fn build_wallet_transaction_request(
        &self,
        signer: &PrivateKeySigner,
        calls: Vec<DepositWalletCallRequest>,
        metadata: String,
    ) -> Result<WalletTransactionRequest, ExecutionError> {
        let owner = signer.address();
        let nonce = self.wallet_nonce(owner).await?;
        let deadline = now_unix_secs()?.saturating_add(DEFAULT_WALLET_BATCH_DEADLINE_SECS);
        self.build_wallet_transaction_request_with_nonce_deadline(
            signer, calls, nonce, deadline, metadata,
        )
        .await
    }

    async fn build_wallet_transaction_request_with_nonce_deadline(
        &self,
        signer: &PrivateKeySigner,
        calls: Vec<DepositWalletCallRequest>,
        nonce: String,
        deadline: u64,
        metadata: String,
    ) -> Result<WalletTransactionRequest, ExecutionError> {
        let owner = signer.address();
        let deposit_wallet = self.deposit_wallet()?;
        let typed_calls = calls
            .iter()
            .map(|call| {
                Ok(Call {
                    target: parse_address(&call.target, "deposit wallet call target")?,
                    value: U256::from_str(&call.value).map_err(|error| {
                        ExecutionError::BadRequest(format!(
                            "invalid deposit wallet call value `{}`: {error}",
                            call.value
                        ))
                    })?,
                    data: Bytes::from_str(&call.data).map_err(|error| {
                        ExecutionError::BadRequest(format!(
                            "invalid deposit wallet call data `{}`: {error}",
                            call.data
                        ))
                    })?,
                })
            })
            .collect::<Result<Vec<_>, ExecutionError>>()?;
        let batch = Batch {
            wallet: deposit_wallet,
            nonce: U256::from_str(&nonce).map_err(|error| {
                ExecutionError::BadRequest(format!(
                    "invalid WALLET relayer nonce `{nonce}`: {error}"
                ))
            })?,
            deadline: U256::from(deadline),
            calls: typed_calls,
        };
        let domain = Eip712Domain {
            name: Some(std::borrow::Cow::Borrowed("DepositWallet")),
            version: Some(std::borrow::Cow::Borrowed("1")),
            chain_id: Some(U256::from(137_u64)),
            verifying_contract: Some(deposit_wallet),
            ..Eip712Domain::default()
        };
        let signature = signer
            .sign_hash(&batch.eip712_signing_hash(&domain))
            .await
            .map_err(|error| {
                ExecutionError::AuthFailure(format!("failed to sign deposit wallet batch: {error}"))
            })?
            .to_string();

        Ok(WalletTransactionRequest {
            tx_type: "WALLET".to_string(),
            from: owner.to_string(),
            to: POLYMARKET_DEPOSIT_WALLET_FACTORY.to_string(),
            nonce,
            signature,
            deposit_wallet_params: DepositWalletParams {
                deposit_wallet: deposit_wallet.to_string(),
                deadline: deadline.to_string(),
                calls,
            },
            metadata,
        })
    }

    async fn build_proxy_split_transaction_request(
        &self,
        signer: &PrivateKeySigner,
        relay_payload: RelayPayload,
        condition_id_raw: &str,
        quantity: f64,
        metadata: String,
    ) -> Result<TransactionRequest, ExecutionError> {
        let from = signer.address();
        let relay = parse_address(&relay_payload.address, "relayer relay address")?;
        let nonce = relay_payload.nonce;
        let ctf = parse_address(&self.config.ctf_contract_address, "CTF contract")?;
        let split_data = self.split_positions_calldata(condition_id_raw, quantity)?;
        let proxy_data = proxyCall {
            transactions: vec![ProxyTransactionCall {
                to: ctf,
                typeCode: 1,
                data: Bytes::from(split_data),
                value: U256::ZERO,
            }],
        }
        .abi_encode();
        let proxy_data_hex = proxy_data.encode_hex_with_prefix();
        let tx_hash = proxy_relay_hash(
            from,
            POLYMARKET_PROXY_FACTORY,
            &proxy_data,
            &nonce,
            POLYMARKET_RELAY_HUB,
            relay,
        )?;
        let signature = signer
            .sign_message(tx_hash.as_slice())
            .await
            .map_err(|error| {
                ExecutionError::AuthFailure(format!("failed to sign CTF proxy split: {error}"))
            })?
            .to_string();

        Ok(TransactionRequest {
            tx_type: "PROXY".to_string(),
            from: from.to_string(),
            to: POLYMARKET_PROXY_FACTORY.to_string(),
            proxy_wallet: self.proxy_wallet(from)?.to_string(),
            data: proxy_data_hex,
            nonce,
            signature,
            signature_params: ProxySignatureParams {
                gas_price: "0".to_string(),
                gas_limit: DEFAULT_PROXY_GAS_LIMIT.to_string(),
                relayer_fee: "0".to_string(),
                relay_hub: POLYMARKET_RELAY_HUB.to_string(),
                relay: relay.to_string(),
            },
            metadata,
        })
    }

    async fn build_proxy_merge_transaction_request(
        &self,
        signer: &PrivateKeySigner,
        relay_payload: RelayPayload,
        condition_id_raw: &str,
        quantity: f64,
        metadata: String,
    ) -> Result<TransactionRequest, ExecutionError> {
        let from = signer.address();
        let relay = parse_address(&relay_payload.address, "relayer relay address")?;
        let nonce = relay_payload.nonce;
        let ctf = parse_address(&self.config.ctf_contract_address, "CTF contract")?;
        let merge_data = self.merge_positions_calldata(condition_id_raw, quantity)?;
        let proxy_data = proxyCall {
            transactions: vec![ProxyTransactionCall {
                to: ctf,
                typeCode: 1,
                data: Bytes::from(merge_data),
                value: U256::ZERO,
            }],
        }
        .abi_encode();
        let proxy_data_hex = proxy_data.encode_hex_with_prefix();
        let tx_hash = proxy_relay_hash(
            from,
            POLYMARKET_PROXY_FACTORY,
            &proxy_data,
            &nonce,
            POLYMARKET_RELAY_HUB,
            relay,
        )?;
        let signature = signer
            .sign_message(tx_hash.as_slice())
            .await
            .map_err(|error| {
                ExecutionError::AuthFailure(format!("failed to sign CTF proxy merge: {error}"))
            })?
            .to_string();

        Ok(TransactionRequest {
            tx_type: "PROXY".to_string(),
            from: from.to_string(),
            to: POLYMARKET_PROXY_FACTORY.to_string(),
            proxy_wallet: self.proxy_wallet(from)?.to_string(),
            data: proxy_data_hex,
            nonce,
            signature,
            signature_params: ProxySignatureParams {
                gas_price: "0".to_string(),
                gas_limit: DEFAULT_PROXY_GAS_LIMIT.to_string(),
                relayer_fee: "0".to_string(),
                relay_hub: POLYMARKET_RELAY_HUB.to_string(),
                relay: relay.to_string(),
            },
            metadata,
        })
    }

    async fn build_proxy_redeem_transaction_request(
        &self,
        signer: &PrivateKeySigner,
        relay_payload: RelayPayload,
        condition_id_raw: &str,
        collateral_token_address: Option<&str>,
        index_sets: &[u64],
        metadata: String,
    ) -> Result<TransactionRequest, ExecutionError> {
        let from = signer.address();
        let relay = parse_address(&relay_payload.address, "relayer relay address")?;
        let nonce = relay_payload.nonce;
        let ctf = parse_address(&self.config.ctf_contract_address, "CTF contract")?;
        let redeem_data =
            self.redeem_positions_calldata(collateral_token_address, condition_id_raw, index_sets)?;
        let proxy_data = proxyCall {
            transactions: vec![ProxyTransactionCall {
                to: ctf,
                typeCode: 1,
                data: Bytes::from(redeem_data),
                value: U256::ZERO,
            }],
        }
        .abi_encode();
        let proxy_data_hex = proxy_data.encode_hex_with_prefix();
        let tx_hash = proxy_relay_hash(
            from,
            POLYMARKET_PROXY_FACTORY,
            &proxy_data,
            &nonce,
            POLYMARKET_RELAY_HUB,
            relay,
        )?;
        let signature = signer
            .sign_message(tx_hash.as_slice())
            .await
            .map_err(|error| {
                ExecutionError::AuthFailure(format!("failed to sign CTF proxy redeem: {error}"))
            })?
            .to_string();

        Ok(TransactionRequest {
            tx_type: "PROXY".to_string(),
            from: from.to_string(),
            to: POLYMARKET_PROXY_FACTORY.to_string(),
            proxy_wallet: self.proxy_wallet(from)?.to_string(),
            data: proxy_data_hex,
            nonce,
            signature,
            signature_params: ProxySignatureParams {
                gas_price: "0".to_string(),
                gas_limit: DEFAULT_PROXY_GAS_LIMIT.to_string(),
                relayer_fee: "0".to_string(),
                relay_hub: POLYMARKET_RELAY_HUB.to_string(),
                relay: relay.to_string(),
            },
            metadata,
        })
    }

    fn redeem_positions_calldata(
        &self,
        collateral_token_address: Option<&str>,
        condition_id_raw: &str,
        index_sets: &[u64],
    ) -> Result<Vec<u8>, ExecutionError> {
        let collateral = parse_address(
            collateral_token_address.unwrap_or(&self.config.collateral_token_address),
            "collateral token",
        )?;
        let condition_id = parse_b256(condition_id_raw, "condition id")?;
        let index_sets_u256: Vec<U256> = index_sets.iter().copied().map(U256::from).collect();
        Ok(redeemPositionsCall {
            collateralToken: collateral,
            parentCollectionId: B256::ZERO,
            conditionId: condition_id,
            indexSets: index_sets_u256,
        }
        .abi_encode())
    }

    fn split_positions_calldata(
        &self,
        condition_id_raw: &str,
        quantity: f64,
    ) -> Result<Vec<u8>, ExecutionError> {
        let collateral = parse_address(&self.config.collateral_token_address, "collateral token")?;
        let condition_id = parse_b256(condition_id_raw, "condition id")?;
        let amount = scaled_token_amount(quantity, self.config.collateral_decimals)?;
        Ok(splitPositionCall {
            collateralToken: collateral,
            parentCollectionId: B256::ZERO,
            conditionId: condition_id,
            partition: vec![U256::from(1_u8), U256::from(2_u8)],
            amount,
        }
        .abi_encode())
    }

    fn merge_positions_calldata(
        &self,
        condition_id_raw: &str,
        quantity: f64,
    ) -> Result<Vec<u8>, ExecutionError> {
        let collateral = parse_address(&self.config.collateral_token_address, "collateral token")?;
        let condition_id = parse_b256(condition_id_raw, "condition id")?;
        let amount = scaled_token_amount(quantity, self.config.collateral_decimals)?;
        Ok(mergePositionsCall {
            collateralToken: collateral,
            parentCollectionId: B256::ZERO,
            conditionId: condition_id,
            partition: vec![U256::from(1_u8), U256::from(2_u8)],
            amount,
        }
        .abi_encode())
    }

    async fn relay_payload(
        &self,
        signer: Address,
        wallet_type: &str,
    ) -> Result<RelayPayload, ExecutionError> {
        let url = format!(
            "{}/relay-payload",
            self.config.relayer_url.trim_end_matches('/')
        );
        let response = self
            .http
            .get(&url)
            .query(&[
                ("address", signer.to_string()),
                ("type", wallet_type.to_string()),
            ])
            .headers(self.builder_headers()?)
            .send()
            .await
            .map_err(|error| ExecutionError::TransientNetwork(error.to_string()))?;
        decode_response(response, "relayer relay-payload").await
    }

    async fn wallet_nonce(&self, signer: Address) -> Result<String, ExecutionError> {
        let url = format!("{}/nonce", self.config.relayer_url.trim_end_matches('/'));
        let response = self
            .http
            .get(&url)
            .query(&[
                ("address", signer.to_string()),
                ("type", "WALLET".to_string()),
            ])
            .headers(self.builder_headers()?)
            .send()
            .await
            .map_err(|error| ExecutionError::TransientNetwork(error.to_string()))?;
        let payload: NoncePayload = decode_response(response, "relayer wallet nonce").await?;
        Ok(payload.nonce)
    }

    async fn submit<T: Serialize>(&self, body: T) -> Result<RelayerSubmitAck, ExecutionError> {
        let url = format!("{}/submit", self.config.relayer_url.trim_end_matches('/'));
        let body = serde_json::to_string(&body).map_err(|error| {
            ExecutionError::BadRequest(format!("failed to serialize relayer submit body: {error}"))
        })?;
        let response = self
            .http
            .post(&url)
            .headers(self.builder_headers()?)
            .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
            .body(body)
            .send()
            .await
            .map_err(|error| ExecutionError::TransientNetwork(error.to_string()))?;
        let decoded: SubmitResponse = decode_response(response, "relayer submit").await?;
        Ok(RelayerSubmitAck {
            transaction_id: decoded.transaction_id,
            state: decoded.state,
            transaction_hash: decoded.transaction_hash,
        })
    }

    fn builder_headers(&self) -> Result<HeaderMap, ExecutionError> {
        let api_key = self
            .config
            .api_key
            .as_deref()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                ExecutionError::AuthFailure(
                    "relayer merge requires RELAYER_API_KEY or POLYMARKET_RELAYER_API_KEY"
                        .to_string(),
                )
            })?;
        let address = self
            .config
            .api_key_address
            .as_deref()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                ExecutionError::AuthFailure(
                    "relayer merge requires RELAYER_API_KEY_ADDRESS or POLYMARKET_RELAYER_API_KEY_ADDRESS"
                        .to_string(),
                )
            })?;
        let mut headers = HeaderMap::new();
        headers.insert(
            "RELAYER_API_KEY",
            HeaderValue::from_str(api_key)
                .map_err(|error| ExecutionError::AuthFailure(error.to_string()))?,
        );
        headers.insert(
            "RELAYER_API_KEY_ADDRESS",
            HeaderValue::from_str(address)
                .map_err(|error| ExecutionError::AuthFailure(error.to_string()))?,
        );
        Ok(headers)
    }

    fn proxy_wallet(&self, from: Address) -> Result<Address, ExecutionError> {
        self.config
            .proxy_wallet_address
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(|value| parse_address(value, "proxy wallet"))
            .transpose()
            .map(|value| value.unwrap_or_else(|| derive_proxy_wallet(from)))
    }

    fn deposit_wallet(&self) -> Result<Address, ExecutionError> {
        self.config
            .proxy_wallet_address
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(|value| parse_address(value, "deposit wallet"))
            .transpose()?
            .ok_or_else(|| {
                ExecutionError::BadRequest(
                    "POLY_1271 CTF relayer actions require POLYMARKET_PROXY_WALLET_ADDRESS/POLYMARKET_FUNDER_ADDRESS to be the deposit wallet address".to_string(),
                )
            })
    }
}

async fn decode_response<T: for<'de> Deserialize<'de>>(
    response: reqwest::Response,
    context: &str,
) -> Result<T, ExecutionError> {
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|error| ExecutionError::TransientNetwork(error.to_string()))?;
    if !status.is_success() {
        return Err(ExecutionError::VenueRejection(format!(
            "{context} failed {status}: {text}"
        )));
    }
    serde_json::from_str(&text).map_err(|error| {
        ExecutionError::VenueRejection(format!(
            "failed to decode {context} response `{text}`: {error}"
        ))
    })
}

fn scaled_token_amount(quantity: f64, decimals: u8) -> Result<U256, ExecutionError> {
    if !quantity.is_finite() || quantity <= 0.0 {
        return Err(ExecutionError::BadRequest(format!(
            "merge quantity must be positive and finite, got {quantity}"
        )));
    }
    let scale = 10_u128.checked_pow(decimals as u32).ok_or_else(|| {
        ExecutionError::BadRequest(format!("unsupported collateral decimals {decimals}"))
    })?;
    Ok(U256::from((quantity * scale as f64).round() as u128))
}

fn parse_address(raw: &str, field: &str) -> Result<Address, ExecutionError> {
    Address::from_str(raw.trim())
        .map_err(|error| ExecutionError::BadRequest(format!("invalid {field} `{raw}`: {error}")))
}

fn parse_b256(raw: &str, field: &str) -> Result<B256, ExecutionError> {
    B256::from_str(raw.trim())
        .map_err(|error| ExecutionError::BadRequest(format!("invalid {field} `{raw}`: {error}")))
}

fn proxy_relay_hash(
    from: Address,
    to: Address,
    data: &[u8],
    nonce: &str,
    relay_hub: Address,
    relay: Address,
) -> Result<B256, ExecutionError> {
    let mut encoded = Vec::new();
    encoded.extend_from_slice(b"rlx:");
    encoded.extend_from_slice(from.as_slice());
    encoded.extend_from_slice(to.as_slice());
    encoded.extend_from_slice(data);
    encoded.extend_from_slice(&u256_be_bytes(U256::ZERO));
    encoded.extend_from_slice(&u256_be_bytes(U256::ZERO));
    encoded.extend_from_slice(&u256_be_bytes(U256::from(DEFAULT_PROXY_GAS_LIMIT)));
    encoded.extend_from_slice(&u256_be_bytes(U256::from_str(nonce).map_err(|error| {
        ExecutionError::BadRequest(format!("invalid relayer nonce `{nonce}`: {error}"))
    })?));
    encoded.extend_from_slice(relay_hub.as_slice());
    encoded.extend_from_slice(relay.as_slice());
    Ok(keccak256(encoded))
}

fn derive_proxy_wallet(owner: Address) -> Address {
    let salt = keccak256(owner.as_slice());
    create2_address(
        POLYMARKET_PROXY_FACTORY,
        salt,
        B256::from_str(PROXY_INIT_CODE_HASH).expect("valid proxy init code hash"),
    )
}

fn create2_address(factory: Address, salt: B256, init_code_hash: B256) -> Address {
    let mut bytes = Vec::with_capacity(85);
    bytes.push(0xff);
    bytes.extend_from_slice(factory.as_slice());
    bytes.extend_from_slice(salt.as_slice());
    bytes.extend_from_slice(init_code_hash.as_slice());
    let hash = keccak256(bytes);
    Address::from_slice(&hash.as_slice()[12..])
}

fn u256_be_bytes(value: U256) -> [u8; 32] {
    value.to_be_bytes::<32>()
}

fn wallet_call_request(
    target: &str,
    data: Bytes,
) -> Result<DepositWalletCallRequest, ExecutionError> {
    let target = parse_address(target, "deposit wallet call target")?;
    Ok(DepositWalletCallRequest {
        target: target.to_string(),
        value: "0".to_string(),
        data: data.encode_hex_with_prefix(),
    })
}

fn now_unix_secs() -> Result<u64, ExecutionError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|error| {
            ExecutionError::BadRequest(format!("system clock before UNIX_EPOCH: {error}"))
        })
}

fn signature_hex_len_bytes(signature: &str) -> Result<usize, ExecutionError> {
    let value = signature
        .trim()
        .strip_prefix("0x")
        .unwrap_or(signature.trim());
    if value.len() % 2 != 0 {
        return Err(ExecutionError::BadRequest(
            "signature hex has odd length".to_string(),
        ));
    }
    Ok(value.len() / 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> CtfRelayerConfig {
        CtfRelayerConfig {
            relayer_url: DEFAULT_RELAYER_URL.to_string(),
            api_key: Some("test-key".to_string()),
            api_key_address: Some("0x97fBC6Bc0d3A70F17EC83aB9cF9d6838328b47c5".to_string()),
            ctf_contract_address: DEFAULT_CTF_ADDRESS.to_string(),
            collateral_token_address: DEFAULT_USDCE_ADDRESS.to_string(),
            collateral_decimals: 6,
            signature_type_code: 1,
            proxy_wallet_address: None,
            polygon_rpc_url: None,
        }
    }

    #[test]
    fn scales_merge_quantity_to_collateral_units() {
        assert_eq!(
            scaled_token_amount(6.5, 6).unwrap(),
            U256::from(6_500_000_u64)
        );
    }

    #[test]
    fn derives_known_proxy_wallet_for_current_signer() {
        let owner = Address::from_str("0x97fBC6Bc0d3A70F17EC83aB9cF9d6838328b47c5").unwrap();
        assert_eq!(
            derive_proxy_wallet(owner).to_string(),
            "0x28ABcBda241C65BD5377a7dc4eEC49545e4B95Ac"
        );
    }

    #[test]
    fn relayer_auth_requires_key_owner_address() {
        let mut config = test_config();
        config.api_key_address = None;
        let client = CtfRelayerClient::new(config);
        let error = client.builder_headers().expect_err("missing key owner");
        assert!(error.to_string().contains("RELAYER_API_KEY_ADDRESS"));
    }

    #[test]
    fn explicit_proxy_wallet_overrides_derived_address() {
        let mut config = test_config();
        config.proxy_wallet_address =
            Some("0xa57189d5b2285A5E64083d3925687bDFCE01fC83".to_string());
        let client = CtfRelayerClient::new(config);
        let owner = Address::from_str("0x97fBC6Bc0d3A70F17EC83aB9cF9d6838328b47c5").unwrap();
        assert_eq!(
            client.proxy_wallet(owner).unwrap().to_string(),
            "0xa57189d5b2285A5E64083d3925687bDFCE01fC83"
        );
    }

    #[tokio::test]
    async fn proxy_redeem_builds_expected_relayer_transaction_request() {
        let mut config = test_config();
        config.proxy_wallet_address =
            Some("0xa57189d5b2285A5E64083d3925687bDFCE01fC83".to_string());
        let client = CtfRelayerClient::new(config);
        let signer = PrivateKeySigner::from_str(
            "0x59c6995e998f97a5a0044966f094538340a3a38f1a07c6d82e841fe4b0d9f10a",
        )
        .expect("test signer");

        let body = client
            .build_proxy_redeem_transaction_request(
                &signer,
                RelayPayload {
                    address: "0x1234567890123456789012345678901234567890".to_string(),
                    nonce: "11".to_string(),
                },
                "0x2222222222222222222222222222222222222222222222222222222222222222",
                None,
                &[1u64, 2u64],
                "{\"redeem\":true}".to_string(),
            )
            .await
            .expect("redeem transaction request");

        assert_eq!(body.tx_type, "PROXY");
        assert_eq!(
            body.proxy_wallet,
            "0xa57189d5b2285A5E64083d3925687bDFCE01fC83"
        );
        assert_eq!(body.nonce, "11");
        assert_eq!(body.signature_params.gas_limit, "10000000");
        assert_eq!(body.metadata, "{\"redeem\":true}");
        assert!(body.data.starts_with("0x"));
        assert!(body.signature.starts_with("0x"));
    }

    #[tokio::test]
    async fn redeem_rejects_empty_index_sets() {
        let client = CtfRelayerClient::new(test_config());
        let signer = PrivateKeySigner::from_str(
            "0x59c6995e998f97a5a0044966f094538340a3a38f1a07c6d82e841fe4b0d9f10a",
        )
        .expect("test signer");
        let request = CtfRedeemRequest {
            signer,
            condition_id: "0x2222222222222222222222222222222222222222222222222222222222222222"
                .to_string(),
            collateral_token_address: None,
            index_sets: vec![],
            metadata: "{}".to_string(),
        };
        let error = client
            .redeem_positions(request)
            .await
            .expect_err("expected empty index_sets rejection");
        assert!(error.to_string().contains("index_sets"));
    }

    #[test]
    fn redeem_calldata_can_override_collateral_token_for_legacy_positions() {
        let client = CtfRelayerClient::new(test_config());
        let calldata = client
            .redeem_positions_calldata(
                Some(DEFAULT_PUSD_ADDRESS),
                "0x2222222222222222222222222222222222222222222222222222222222222222",
                &[1, 2],
            )
            .expect("redeem calldata");
        let decoded =
            redeemPositionsCall::abi_decode(&calldata).expect("generated calldata decodes");

        assert_eq!(
            decoded.collateralToken,
            Address::from_str(DEFAULT_PUSD_ADDRESS).unwrap()
        );
        assert_eq!(
            decoded.conditionId,
            B256::from_str("0x2222222222222222222222222222222222222222222222222222222222222222")
                .unwrap()
        );
        assert_eq!(decoded.indexSets, vec![U256::from(1_u8), U256::from(2_u8)]);
    }

    #[test]
    fn split_calldata_uses_configured_ctf_collateral_token() {
        let mut config = test_config();
        config.collateral_token_address = DEFAULT_PUSD_ADDRESS.to_string();
        let client = CtfRelayerClient::new(config);
        let calldata = client
            .split_positions_calldata(
                "0xf3eb9227564ea848dc5d95a577c06e11b67d3223046ff2decf53c63144d14908",
                1111.0,
            )
            .expect("split calldata");
        let decoded =
            splitPositionCall::abi_decode(&calldata).expect("generated calldata decodes");

        assert_eq!(
            decoded.collateralToken,
            Address::from_str(DEFAULT_PUSD_ADDRESS).unwrap()
        );
        assert_eq!(decoded.amount, U256::from(1_111_000_000_u64));
        assert_eq!(decoded.partition, vec![U256::from(1_u8), U256::from(2_u8)]);
    }

    #[test]
    fn merge_calldata_uses_configured_ctf_collateral_token() {
        let mut config = test_config();
        config.collateral_token_address = DEFAULT_USDCE_ADDRESS.to_string();
        let client = CtfRelayerClient::new(config);
        let calldata = client
            .merge_positions_calldata(
                "0xf3eb9227564ea848dc5d95a577c06e11b67d3223046ff2decf53c63144d14908",
                10.5,
            )
            .expect("merge calldata");
        let decoded =
            mergePositionsCall::abi_decode(&calldata).expect("generated calldata decodes");

        assert_eq!(
            decoded.collateralToken,
            Address::from_str(DEFAULT_USDCE_ADDRESS).unwrap()
        );
        assert_eq!(decoded.amount, U256::from(10_500_000_u64));
        assert_eq!(decoded.partition, vec![U256::from(1_u8), U256::from(2_u8)]);
    }

    #[tokio::test]
    async fn proxy_merge_builds_expected_relayer_transaction_request() {
        let mut config = test_config();
        config.proxy_wallet_address =
            Some("0xa57189d5b2285A5E64083d3925687bDFCE01fC83".to_string());
        let client = CtfRelayerClient::new(config);
        let signer = PrivateKeySigner::from_str(
            "0x59c6995e998f97a5a0044966f094538340a3a38f1a07c6d82e841fe4b0d9f10a",
        )
        .expect("test signer");

        let body = client
            .build_proxy_merge_transaction_request(
                &signer,
                RelayPayload {
                    address: "0x1234567890123456789012345678901234567890".to_string(),
                    nonce: "7".to_string(),
                },
                "0x1111111111111111111111111111111111111111111111111111111111111111",
                6.5,
                "{\"test\":true}".to_string(),
            )
            .await
            .expect("merge transaction request");

        assert_eq!(body.tx_type, "PROXY");
        assert_eq!(
            body.proxy_wallet,
            "0xa57189d5b2285A5E64083d3925687bDFCE01fC83"
        );
        assert_eq!(body.nonce, "7");
        assert_eq!(body.signature_params.gas_limit, "10000000");
        assert_eq!(body.signature_params.gas_price, "0");
        assert_eq!(body.signature_params.relayer_fee, "0");
        assert_eq!(body.metadata, "{\"test\":true}");
        assert!(body.data.starts_with("0x"));
        assert!(body.signature.starts_with("0x"));
    }

    #[tokio::test]
    async fn wrap_usdce_to_pusd_builds_approve_then_wrap_batch() {
        let mut config = test_config();
        config.signature_type_code = 3;
        config.proxy_wallet_address =
            Some("0xa57189d5b2285A5E64083d3925687bDFCE01fC83".to_string());
        let client = CtfRelayerClient::new(config);
        let signer = PrivateKeySigner::from_str(
            "0x59c6995e998f97a5a0044966f094538340a3a38f1a07c6d82e841fe4b0d9f10a",
        )
        .expect("test signer");
        let amount = U256::from(6_240_000_u64);
        let body = client
            .build_wallet_transaction_request_with_nonce_deadline(
                &signer,
                vec![
                    wallet_call_request(
                        DEFAULT_USDCE_ADDRESS,
                        Bytes::from(
                            approveCall {
                                spender: Address::from_str(COLLATERAL_ONRAMP_ADDRESS).unwrap(),
                                amount,
                            }
                            .abi_encode(),
                        ),
                    )
                    .unwrap(),
                    wallet_call_request(
                        COLLATERAL_ONRAMP_ADDRESS,
                        Bytes::from(
                            wrapCall {
                                _asset: Address::from_str(DEFAULT_USDCE_ADDRESS).unwrap(),
                                _to: Address::from_str(
                                    "0xa57189d5b2285A5E64083d3925687bDFCE01fC83",
                                )
                                .unwrap(),
                                _amount: amount,
                            }
                            .abi_encode(),
                        ),
                    )
                    .unwrap(),
                ],
                "3".to_string(),
                1_760_000_000,
                "{\"wrap\":true}".to_string(),
            )
            .await
            .expect("wrap batch");

        assert_eq!(body.tx_type, "WALLET");
        assert_eq!(body.deposit_wallet_params.calls.len(), 2);
        assert_eq!(
            body.deposit_wallet_params.calls[0].target.to_lowercase(),
            DEFAULT_USDCE_ADDRESS.to_lowercase()
        );
        assert_eq!(
            body.deposit_wallet_params.calls[1].target.to_lowercase(),
            COLLATERAL_ONRAMP_ADDRESS.to_lowercase()
        );
        let approve_decoded = approveCall::abi_decode(
            &Bytes::from_str(&body.deposit_wallet_params.calls[0].data).unwrap(),
        )
        .expect("approve decodes");
        assert_eq!(
            approve_decoded.spender,
            Address::from_str(COLLATERAL_ONRAMP_ADDRESS).unwrap()
        );
        assert_eq!(approve_decoded.amount, amount);
        let wrap_decoded = wrapCall::abi_decode(
            &Bytes::from_str(&body.deposit_wallet_params.calls[1].data).unwrap(),
        )
        .expect("wrap decodes");
        assert_eq!(
            wrap_decoded._asset,
            Address::from_str(DEFAULT_USDCE_ADDRESS).unwrap()
        );
        assert_eq!(
            wrap_decoded._to,
            Address::from_str("0xa57189d5b2285A5E64083d3925687bDFCE01fC83").unwrap()
        );
        assert_eq!(wrap_decoded._amount, amount);
    }

    #[tokio::test]
    async fn wrap_usdce_to_pusd_rejects_non_poly1271_signature_type() {
        let mut config = test_config();
        config.signature_type_code = 1;
        let client = CtfRelayerClient::new(config);
        let signer = PrivateKeySigner::from_str(
            "0x59c6995e998f97a5a0044966f094538340a3a38f1a07c6d82e841fe4b0d9f10a",
        )
        .expect("test signer");
        let err = client
            .wrap_usdce_to_pusd(&signer, U256::from(1_000_000_u64), "{}".to_string())
            .await
            .expect_err("non-poly1271 rejected");
        assert!(err.to_string().contains("poly_1271"));
    }

    #[tokio::test]
    async fn wrap_usdce_to_pusd_rejects_zero_amount() {
        let mut config = test_config();
        config.signature_type_code = 3;
        config.proxy_wallet_address =
            Some("0xa57189d5b2285A5E64083d3925687bDFCE01fC83".to_string());
        let client = CtfRelayerClient::new(config);
        let signer = PrivateKeySigner::from_str(
            "0x59c6995e998f97a5a0044966f094538340a3a38f1a07c6d82e841fe4b0d9f10a",
        )
        .expect("test signer");
        let err = client
            .wrap_usdce_to_pusd(&signer, U256::ZERO, "{}".to_string())
            .await
            .expect_err("zero amount rejected");
        assert!(err.to_string().contains("amount must be > 0"));
    }

    #[tokio::test]
    async fn wallet_merge_builds_poly1271_wallet_batch_request() {
        let mut config = test_config();
        config.signature_type_code = 3;
        config.proxy_wallet_address =
            Some("0xa57189d5b2285A5E64083d3925687bDFCE01fC83".to_string());
        let client = CtfRelayerClient::new(config);
        let signer = PrivateKeySigner::from_str(
            "0x59c6995e998f97a5a0044966f094538340a3a38f1a07c6d82e841fe4b0d9f10a",
        )
        .expect("test signer");
        let calldata = client
            .merge_positions_calldata(
                "0xf3eb9227564ea848dc5d95a577c06e11b67d3223046ff2decf53c63144d14908",
                5.0,
            )
            .expect("merge calldata");

        let body = client
            .build_wallet_transaction_request_with_nonce_deadline(
                &signer,
                vec![wallet_call_request(DEFAULT_CTF_ADDRESS, Bytes::from(calldata)).unwrap()],
                "7".to_string(),
                1_760_000_000,
                "{\"wallet\":true}".to_string(),
            )
            .await
            .expect("wallet transaction request");

        assert_eq!(body.tx_type, "WALLET");
        assert_eq!(body.to, POLYMARKET_DEPOSIT_WALLET_FACTORY.to_string());
        assert_eq!(
            body.deposit_wallet_params.deposit_wallet,
            "0xa57189d5b2285A5E64083d3925687bDFCE01fC83"
        );
        assert_eq!(body.nonce, "7");
        assert_eq!(body.deposit_wallet_params.deadline, "1760000000");
        assert_eq!(body.deposit_wallet_params.calls.len(), 1);
        assert_eq!(
            body.deposit_wallet_params.calls[0].target,
            DEFAULT_CTF_ADDRESS
        );
        assert_eq!(body.deposit_wallet_params.calls[0].value, "0");
        assert_eq!(body.metadata, "{\"wallet\":true}");
        assert_eq!(signature_hex_len_bytes(&body.signature).unwrap(), 65);
    }
}
