//! Polymarket builder-relayer helpers for CTF merge transactions.

use std::str::FromStr;

use alloy::hex::ToHexExt as _;
use alloy::primitives::{address, keccak256, Address, Bytes, B256, U256};
use alloy::signers::local::PrivateKeySigner;
use alloy::signers::Signer;
use alloy::sol;
use alloy::sol_types::SolCall;
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE};
use serde::{Deserialize, Serialize};

use crate::wire::eoa_polygon::EoaPolygonSubmitter;
use crate::wire::execution_adapter::ExecutionError;

pub const DEFAULT_RELAYER_URL: &str = "https://relayer-v2.polymarket.com";
pub const DEFAULT_CTF_ADDRESS: &str = "0x4D97DCd97eC945f40cF65F87097ACe5EA0476045";
pub const DEFAULT_USDCE_ADDRESS: &str = "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174";
pub const DEFAULT_PUSD_ADDRESS: &str = "0xC011a7E12a19f7B1f670d46F03B03f3342E82DFB";

const POLYMARKET_PROXY_FACTORY: Address = address!("aB45c5A4B0c941a2F231C04C3f49182e1A254052");
const POLYMARKET_RELAY_HUB: Address = address!("D216153c06E857cD7f72665E0aF1d7D82172F494");
const PROXY_INIT_CODE_HASH: &str =
    "0xd21df8dc65880a8606f09fe0ce3df9b8869287ab0b058be05aa9e8af6330a00b";
const DEFAULT_PROXY_GAS_LIMIT: u64 = 10_000_000;

sol! {
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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RelayPayload {
    address: String,
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

    pub async fn merge_positions(
        &self,
        request: CtfMergeRequest,
    ) -> Result<RelayerSubmitAck, ExecutionError> {
        match self.config.signature_type_code {
            1 => {
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
            0 => self.submit_eoa_merge(&request).await,
            other => Err(ExecutionError::BadRequest(format!(
                "CTF relayer merge supports POLYMARKET_SIGNATURE_TYPE=0 (EOA) or =1 (proxy), got {other}",
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
            1 => {
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
            0 => self.submit_eoa_redeem(&request).await,
            other => Err(ExecutionError::BadRequest(format!(
                "CTF relayer redeem supports POLYMARKET_SIGNATURE_TYPE=0 (EOA) or =1 (proxy), got {other}",
            ))),
        }
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

    async fn submit(&self, body: TransactionRequest) -> Result<RelayerSubmitAck, ExecutionError> {
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
}
