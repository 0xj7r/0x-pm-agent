//! Raw CLOB V2 order payload and signing helpers.

use std::borrow::Cow;
use std::str::FromStr;

use alloy::dyn_abi::Eip712Domain;
use alloy::hex::ToHexExt as _;
use alloy::primitives::{Address, B256, U256};
use alloy::signers::Signer;
use alloy::sol;
use alloy::sol_types::SolStruct as _;
use serde::Serialize;

use crate::wire::execution_adapter::{ExecutionError, SubmitOrderRequest, TimeInForce};

pub const CLOB_V2_EXCHANGE: &str = "0xE111180000d2663C0091e4f400237545B87B996B";
pub const CLOB_V2_NEG_RISK_EXCHANGE: &str = "0xe2222d279d744050d28e00520010520000310F59";
pub const BYTES32_ZERO: &str = "0x0000000000000000000000000000000000000000000000000000000000000000";

const ORDER_NAME: Option<Cow<'static, str>> = Some(Cow::Borrowed("Polymarket CTF Exchange"));
const ORDER_VERSION: Option<Cow<'static, str>> = Some(Cow::Borrowed("2"));
const TOKEN_DECIMALS: f64 = 1_000_000.0;

sol! {
    #[derive(Debug, PartialEq)]
    struct V2OrderToSign {
        uint256 salt;
        address maker;
        address signer;
        uint256 tokenId;
        uint256 makerAmount;
        uint256 takerAmount;
        uint8 side;
        uint8 signatureType;
        uint256 timestamp;
        bytes32 metadata;
        bytes32 builder;
    }
}

#[derive(Debug, PartialEq)]
pub struct V2OrderDraft {
    order: V2OrderToSign,
    expiration: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct V2PostOrderBody {
    pub order: V2PostOrder,
    pub owner: String,
    pub order_type: String,
    pub defer_exec: bool,
    pub post_only: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct V2PostOrder {
    pub salt: u64,
    pub maker: String,
    pub signer: String,
    #[serde(rename = "tokenId")]
    pub token_id: String,
    pub maker_amount: String,
    pub taker_amount: String,
    pub side: String,
    pub expiration: String,
    pub signature_type: u8,
    pub timestamp: String,
    pub metadata: String,
    pub builder: String,
    pub signature: String,
    // NOTE: feeRateBps, nonce, taker are deliberately OMITTED.
    // Per official polymarket_client_sdk_v2 v0.5.1, the V2 JSON body
    // does NOT include these fields. Polymarket's /order endpoint
    // routes V1 vs V2 by JSON shape: present = V1 (needs them in
    // EIP-712 hash), absent = V2. Earlier "error parsing fee rate
    // bps () to int64" came from Polymarket interpreting our partial
    // payload as V1. The fix is to NOT send the V1-specific fields
    // so the venue uses its V2 validation path that matches our
    // EIP-712 signed struct.
}

#[derive(Debug, Clone, PartialEq)]
pub struct V2OrderBuildParams {
    pub maker: Address,
    pub signer: Address,
    pub signature_type: u8,
    pub timestamp_ms: u64,
    pub builder_code: B256,
    pub metadata: B256,
    pub salt: u64,
    pub expiration_s: u64,
}

impl V2OrderDraft {
    pub fn from_submit_request(
        req: &SubmitOrderRequest,
        params: V2OrderBuildParams,
    ) -> Result<Self, ExecutionError> {
        let token_id = U256::from_str(req.instrument_id.as_str()).map_err(|error| {
            ExecutionError::BadRequest(format!(
                "invalid Polymarket token id `{}` for CLOB V2 order: {error}",
                req.instrument_id
            ))
        })?;
        let side = match req.side {
            crate::types::TradeSide::Buy => 0_u8,
            crate::types::TradeSide::Sell => 1_u8,
        };
        let maker_amount = scaled_amount(if side == 0 {
            req.quantity * req.limit_price
        } else {
            req.quantity
        })?;
        let taker_amount = scaled_amount(if side == 0 {
            req.quantity
        } else {
            req.quantity * req.limit_price
        })?;

        let expiration_s = match req.time_in_force {
            TimeInForce::Gtd => params.expiration_s,
            TimeInForce::Gtc | TimeInForce::Ioc | TimeInForce::Fok => 0,
        };

        Ok(Self {
            order: V2OrderToSign {
                salt: U256::from(params.salt),
                maker: params.maker,
                signer: params.signer,
                tokenId: token_id,
                makerAmount: U256::from(maker_amount),
                takerAmount: U256::from(taker_amount),
                side,
                signatureType: params.signature_type,
                timestamp: U256::from(params.timestamp_ms),
                metadata: params.metadata,
                builder: params.builder_code,
            },
            expiration: expiration_s.to_string(),
        })
    }

    pub async fn sign<S: Signer>(
        &self,
        signer: &S,
        chain_id: u64,
        exchange: Address,
    ) -> Result<String, ExecutionError> {
        let domain = Eip712Domain {
            name: ORDER_NAME,
            version: ORDER_VERSION,
            chain_id: Some(U256::from(chain_id)),
            verifying_contract: Some(exchange),
            ..Eip712Domain::default()
        };
        signer
            .sign_hash(&self.order.eip712_signing_hash(&domain))
            .await
            .map(|signature| signature.to_string())
            .map_err(|error| {
                ExecutionError::AuthFailure(format!("CLOB V2 order signing failed: {error}"))
            })
    }

    pub fn post_body(
        &self,
        owner: impl Into<String>,
        order_type: impl Into<String>,
        post_only: bool,
        signature: impl Into<String>,
    ) -> Result<V2PostOrderBody, ExecutionError> {
        let salt: u64 = self.order.salt.try_into().map_err(|error| {
            ExecutionError::BadRequest(format!("CLOB V2 order salt does not fit u64: {error}"))
        })?;
        Ok(V2PostOrderBody {
            order: V2PostOrder {
                salt,
                maker: self.order.maker.encode_hex_with_prefix(),
                signer: self.order.signer.encode_hex_with_prefix(),
                token_id: self.order.tokenId.to_string(),
                maker_amount: self.order.makerAmount.to_string(),
                taker_amount: self.order.takerAmount.to_string(),
                side: if self.order.side == 0 { "BUY" } else { "SELL" }.to_string(),
                expiration: self.expiration.clone(),
                signature_type: self.order.signatureType,
                timestamp: self.order.timestamp.to_string(),
                metadata: self.order.metadata.encode_hex_with_prefix(),
                builder: self.order.builder.encode_hex_with_prefix(),
                signature: signature.into(),
            },
            owner: owner.into(),
            order_type: order_type.into(),
            defer_exec: false,
            post_only,
        })
    }
}

pub fn parse_bytes32(raw: &str, field: &str) -> Result<B256, ExecutionError> {
    B256::from_str(raw.trim()).map_err(|error| {
        ExecutionError::BadRequest(format!("invalid CLOB V2 {field} bytes32 `{raw}`: {error}"))
    })
}

fn scaled_amount(value: f64) -> Result<u128, ExecutionError> {
    if !value.is_finite() || value <= 0.0 {
        return Err(ExecutionError::BadRequest(format!(
            "CLOB V2 order amount must be positive, got {value}"
        )));
    }
    Ok((value * TOKEN_DECIMALS).round() as u128)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ClientOrderId, InstrumentId, MarketId, TradeSide};

    fn sample_request() -> SubmitOrderRequest {
        SubmitOrderRequest {
            client_order_id: ClientOrderId::from("client-1"),
            market_id: MarketId::from("condition-1"),
            instrument_id: InstrumentId::from("123456789"),
            side: TradeSide::Buy,
            limit_price: 0.4,
            quantity: 12.5,
            post_only: true,
            time_in_force: TimeInForce::Gtc,
            expires_at_ms: None,
            strategy_tag: "test".to_string(),
            quote_level_tag: None,
            submitted_at_ms: 1_713_398_400_000,
        }
    }

    #[test]
    fn v2_post_body_uses_v2_order_shape() {
        let maker = Address::from_str("0x0000000000000000000000000000000000000001").unwrap();
        let signer = Address::from_str("0x0000000000000000000000000000000000000002").unwrap();
        let draft = V2OrderDraft::from_submit_request(
            &sample_request(),
            V2OrderBuildParams {
                maker,
                signer,
                signature_type: 2,
                timestamp_ms: 1_713_398_400_000,
                builder_code: parse_bytes32(BYTES32_ZERO, "builder").unwrap(),
                metadata: parse_bytes32(BYTES32_ZERO, "metadata").unwrap(),
                salt: 42,
                expiration_s: 0,
            },
        )
        .unwrap();

        let body = draft.post_body("api-owner", "GTC", true, "0xsig").unwrap();
        let json = serde_json::to_value(body).unwrap();
        assert_eq!(json["order"]["side"], "BUY");
        assert_eq!(json["order"]["timestamp"], "1713398400000");
        assert_eq!(json["order"]["metadata"], BYTES32_ZERO);
        assert_eq!(json["order"]["builder"], BYTES32_ZERO);
        assert_eq!(json["order"]["makerAmount"], "5000000");
        assert_eq!(json["order"]["takerAmount"], "12500000");
        assert_eq!(json["order"]["signatureType"], 2);
        // V2 JSON body must NOT include feeRateBps/nonce/taker — those
        // are V1 fields. Polymarket routes V1/V2 by presence; sending
        // them triggers V1 validation (which would mismatch our V2
        // EIP-712 signed payload).
        assert!(json["order"].get("feeRateBps").is_none());
        assert!(json["order"].get("nonce").is_none());
        assert!(json["order"].get("taker").is_none());
        assert_eq!(json["postOnly"], true);
        assert_eq!(json["deferExec"], false);
    }
}
