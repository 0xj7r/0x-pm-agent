//! Live CLOB execution adapter contract (Section 8.1).

use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use alloy::signers::local::PrivateKeySigner;
use async_trait::async_trait;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use polymarket_client_sdk::auth::{Credentials as SdkCredentials, ExposeSecret, Signer, Uuid};
use polymarket_client_sdk::clob::types::request::{BalanceAllowanceRequest, OrdersRequest};
use polymarket_client_sdk::clob::types::{
    OrderStatusType, OrderType as SdkOrderType, Side as SdkSide, SignatureType as SdkSignatureType,
};
use polymarket_client_sdk::clob::{Client as SdkClobClient, Config as SdkClobConfig};
use polymarket_client_sdk::data::types::request::PositionsRequest as DataPositionsRequest;
use polymarket_client_sdk::data::types::response::Position as DataPosition;
use polymarket_client_sdk::data::Client as SdkDataClient;
use polymarket_client_sdk::types::{
    Address as SdkAddress, DateTime as SdkDateTime, Decimal as SdkDecimal, Utc as SdkUtc,
    U256 as SdkU256,
};
use polymarket_client_sdk::{auth, POLYGON};
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE};
use reqwest::Method;
use serde::Deserialize;
use sha2::Sha256;
use tokio::sync::RwLock;

use crate::types::{ClientOrderId, EpochMillis, InstrumentId, MarketId, OrderId, TradeSide};
use crate::wire::clob_v2::{
    parse_bytes32, V2OrderBuildParams, V2OrderDraft, BYTES32_ZERO, CLOB_V2_EXCHANGE,
    CLOB_V2_NEG_RISK_EXCHANGE,
};
use crate::wire::eoa_polygon::{scaled_usdc_units, EoaPolygonSubmitter, PusdWrapReport};
pub use crate::wire::execution_types::{
    CancelOrderAck, CancelOrderRequest, ExecutionAdapter, ExecutionError, MarketMetadata,
    MergePositionsAck, MergePositionsRequest, RedeemPositionsAck, RedeemPositionsRequest,
    SubmitOrderAck, SubmitOrderRequest, TimeInForce, VenueBalances, VenueFill, VenueOpenOrder,
    VenuePosition,
};
use crate::wire::raw_trades::{parse_raw_trades_page, RawTradeFilter};
use crate::wire::relayer::{
    CtfMergeRequest, CtfRedeemRequest, CtfRelayerClient, CtfRelayerConfig, DEFAULT_CTF_ADDRESS,
    DEFAULT_PUSD_ADDRESS, DEFAULT_RELAYER_URL, DEFAULT_USDCE_ADDRESS,
};

#[derive(Clone, Debug, PartialEq)]
pub struct PolymarketCredentials {
    pub api_key: String,
    pub api_secret: String,
    pub api_passphrase: String,
    pub private_key: String,
    pub signature_type: PolymarketSignatureType,
    pub funder_address: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PolymarketL1Credentials {
    pub private_key: String,
    pub signature_type: PolymarketSignatureType,
    pub funder_address: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PolymarketSignatureType {
    Eoa,
    Proxy,
    Poly1271,
    #[default]
    GnosisSafe,
}

impl PolymarketSignatureType {
    pub fn parse(raw: &str) -> Result<Self, ExecutionError> {
        let normalized = raw
            .split_once('#')
            .map(|(value, _)| value)
            .unwrap_or(raw)
            .trim()
            .to_ascii_lowercase();
        match normalized.as_str() {
            "0" | "eoa" => Ok(Self::Eoa),
            "1" | "proxy" | "poly_proxy" | "poly-proxy" => Ok(Self::Proxy),
            "2" | "safe" | "gnosis" | "gnosis_safe" | "gnosis-safe" => Ok(Self::GnosisSafe),
            "3" | "poly_1271" | "poly-1271" | "poly1271" | "deposit_wallet" | "deposit-wallet" => {
                Ok(Self::Poly1271)
            }
            other => Err(ExecutionError::BadRequest(format!(
                "unsupported POLYMARKET_SIGNATURE_TYPE `{other}`"
            ))),
        }
    }

    fn as_legacy_sdk(self) -> Result<SdkSignatureType, ExecutionError> {
        Ok(match self {
            Self::Eoa => SdkSignatureType::Eoa,
            Self::Proxy => SdkSignatureType::Proxy,
            Self::GnosisSafe => SdkSignatureType::GnosisSafe,
            Self::Poly1271 => {
                return Err(ExecutionError::BadRequest(
                    "POLYMARKET_SIGNATURE_TYPE=poly_1271 requires the CLOB V2 client path"
                        .to_string(),
                ))
            }
        })
    }

    pub fn as_polymarket_code(self) -> u8 {
        match self {
            Self::Eoa => 0,
            Self::Proxy => 1,
            Self::GnosisSafe => 2,
            Self::Poly1271 => 3,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PolymarketConfig {
    pub api_url: String,
    pub data_api_url: String,
    pub relayer_url: String,
    pub relayer_api_key: Option<String>,
    pub relayer_api_key_address: Option<String>,
    pub ctf_contract_address: String,
    pub ctf_collateral_token_address: String,
    pub collateral_token_address: String,
    pub collateral_decimals: u8,
    pub proxy_wallet_address: Option<String>,
    pub polygon_rpc_url: Option<String>,
    pub market_id_by_asset: HashMap<String, String>,
    pub protocol: ClobProtocolVersion,
    pub v2_builder_code: String,
    pub v2_metadata: String,
    pub v2_neg_risk: bool,
    pub credentials: Option<PolymarketCredentials>,
}

impl Default for PolymarketConfig {
    fn default() -> Self {
        Self {
            api_url: "https://clob.polymarket.com".to_string(),
            data_api_url: "https://data-api.polymarket.com".to_string(),
            relayer_url: DEFAULT_RELAYER_URL.to_string(),
            relayer_api_key: None,
            relayer_api_key_address: None,
            ctf_contract_address: DEFAULT_CTF_ADDRESS.to_string(),
            ctf_collateral_token_address: DEFAULT_USDCE_ADDRESS.to_string(),
            collateral_token_address: DEFAULT_PUSD_ADDRESS.to_string(),
            collateral_decimals: 6,
            proxy_wallet_address: None,
            polygon_rpc_url: None,
            market_id_by_asset: HashMap::new(),
            protocol: ClobProtocolVersion::V1,
            v2_builder_code: BYTES32_ZERO.to_string(),
            v2_metadata: BYTES32_ZERO.to_string(),
            v2_neg_risk: false,
            credentials: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ClobProtocolVersion {
    #[default]
    V1,
    V2,
}

impl ClobProtocolVersion {
    pub fn parse(raw: &str) -> Result<Self, ExecutionError> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "1" | "v1" | "clob_v1" | "clob-v1" => Ok(Self::V1),
            "2" | "v2" | "clob_v2" | "clob-v2" => Ok(Self::V2),
            other => Err(ExecutionError::BadRequest(format!(
                "unsupported POLYMARKET_CLOB_VERSION `{other}`"
            ))),
        }
    }
}

#[derive(Default)]
struct AdapterState {
    venue_order_map: HashMap<ClientOrderId, OrderId>,
}

pub struct PaperExecutionAdapter;

impl Default for PaperExecutionAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl PaperExecutionAdapter {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ExecutionAdapter for PaperExecutionAdapter {
    async fn submit(&self, req: SubmitOrderRequest) -> Result<SubmitOrderAck, ExecutionError> {
        Ok(SubmitOrderAck {
            client_order_id: req.client_order_id,
            venue_order_id: None,
            accepted: true,
            accepted_at_ms: req.submitted_at_ms,
            venue_message: None,
        })
    }

    async fn cancel(&self, req: CancelOrderRequest) -> Result<CancelOrderAck, ExecutionError> {
        Ok(CancelOrderAck {
            client_order_id: req.client_order_id,
            venue_order_id: req.venue_order_id,
            accepted: true,
            accepted_at_ms: req.submitted_at_ms,
            venue_message: Some("paper adapter cancel accepted".to_string()),
        })
    }

    async fn merge_positions(
        &self,
        req: MergePositionsRequest,
    ) -> Result<MergePositionsAck, ExecutionError> {
        Ok(MergePositionsAck {
            command_id: req.command_id,
            accepted: true,
            accepted_at_ms: req.submitted_at_ms,
            venue_message: Some("paper adapter merge accepted".to_string()),
        })
    }

    async fn redeem_positions(
        &self,
        req: RedeemPositionsRequest,
    ) -> Result<RedeemPositionsAck, ExecutionError> {
        Ok(RedeemPositionsAck {
            command_id: req.command_id,
            accepted: true,
            accepted_at_ms: req.submitted_at_ms,
            venue_message: Some("paper adapter redeem accepted".to_string()),
        })
    }

    async fn sync_open_orders(&self) -> Result<Vec<VenueOpenOrder>, ExecutionError> {
        Ok(Vec::new())
    }

    async fn sync_balances(&self) -> Result<VenueBalances, ExecutionError> {
        Ok(VenueBalances {
            cash_usd: 0.0,
            positions: Vec::new(),
            positions_authoritative: false,
            observed_at_ms: now_unix_ms(),
        })
    }

    async fn sync_recent_fills(
        &self,
        _after_ms: EpochMillis,
    ) -> Result<Vec<VenueFill>, ExecutionError> {
        Ok(Vec::new())
    }
}

/// Type alias for the V2 SDK's authenticated CLOB client. Wrapping the
/// long generic path keeps field declarations + helper signatures readable.
type V2SdkAuthClient = polymarket_client_sdk_v2::clob::Client<
    polymarket_client_sdk_v2::auth::state::Authenticated<polymarket_client_sdk_v2::auth::Normal>,
>;

pub struct PolymarketExecutionAdapter {
    _config: PolymarketConfig,
    signer: PrivateKeySigner,
    signature_type: PolymarketSignatureType,
    client: SdkClobClient<auth::state::Authenticated<auth::Normal>>,
    data_client: SdkDataClient,
    relayer_client: CtfRelayerClient,
    raw_http: reqwest::Client,
    trade_address: Option<SdkAddress>,
    state: Arc<RwLock<AdapterState>>,
    /// Raw private key hex retained so submit_v2_via_sdk can reconstruct
    /// a fresh LocalSigner for the V2 SDK's typestate-based auth flow.
    /// Stored privately; never logged or serialized.
    _stored_private_key: Option<String>,
    /// Cached authenticated V2 SDK client. Lazy-initialised on the first
    /// V2 submit so we pay the ~150ms auth roundtrip once per adapter
    /// lifetime, then reuse cheap `Clone` instances for every subsequent
    /// order. Also keeps the SDK's automatic heartbeat task alive for
    /// the duration of the trading session — process crash → server-side
    /// auto-cancel within the venue's heartbeat timeout.
    v2_sdk_client: Arc<tokio::sync::OnceCell<V2SdkAuthClient>>,
}

impl PolymarketExecutionAdapter {
    pub async fn connect(credentials: PolymarketCredentials) -> Result<Self, ExecutionError> {
        Self::connect_with_config(PolymarketConfig {
            api_url: "https://clob.polymarket.com".to_string(),
            data_api_url: "https://data-api.polymarket.com".to_string(),
            relayer_url: DEFAULT_RELAYER_URL.to_string(),
            relayer_api_key: None,
            relayer_api_key_address: None,
            ctf_contract_address: DEFAULT_CTF_ADDRESS.to_string(),
            ctf_collateral_token_address: DEFAULT_USDCE_ADDRESS.to_string(),
            collateral_token_address: DEFAULT_PUSD_ADDRESS.to_string(),
            collateral_decimals: 6,
            proxy_wallet_address: None,
            polygon_rpc_url: None,
            market_id_by_asset: HashMap::new(),
            protocol: ClobProtocolVersion::V1,
            v2_builder_code: BYTES32_ZERO.to_string(),
            v2_metadata: BYTES32_ZERO.to_string(),
            v2_neg_risk: false,
            credentials: Some(credentials),
        })
        .await
    }

    pub async fn connect_with_l1(
        credentials: PolymarketL1Credentials,
    ) -> Result<Self, ExecutionError> {
        Self::connect_with_l1_url("https://clob.polymarket.com", credentials).await
    }

    pub async fn connect_with_l1_url(
        api_url: impl Into<String>,
        credentials: PolymarketL1Credentials,
    ) -> Result<Self, ExecutionError> {
        Self::connect_with_l1_urls(
            api_url,
            "https://data-api.polymarket.com",
            HashMap::new(),
            credentials,
        )
        .await
    }

    pub async fn connect_with_l1_urls(
        api_url: impl Into<String>,
        data_api_url: impl Into<String>,
        market_id_by_asset: HashMap<String, String>,
        credentials: PolymarketL1Credentials,
    ) -> Result<Self, ExecutionError> {
        let api_url = api_url.into();
        let data_api_url = data_api_url.into();
        Self::connect_with_l1_config(
            PolymarketConfig {
                api_url,
                data_api_url,
                market_id_by_asset,
                ..PolymarketConfig::default()
            },
            credentials,
        )
        .await
    }

    pub async fn connect_with_l1_config(
        config: PolymarketConfig,
        credentials: PolymarketL1Credentials,
    ) -> Result<Self, ExecutionError> {
        let api_url = config.api_url.clone();
        let data_api_url = config.data_api_url.clone();
        let signer = PrivateKeySigner::from_str(credentials.private_key.trim())
            .map_err(|error| {
                ExecutionError::AuthFailure(format!("invalid POLYMARKET_PRIVATE_KEY: {error}"))
            })?
            .with_chain_id(Some(POLYGON));

        let mut auth_builder = SdkClobClient::new(
            api_url.as_str(),
            SdkClobConfig::builder().use_server_time(true).build(),
        )
        .map_err(map_sdk_error)?
        .authentication_builder(&signer)
        .signature_type(credentials.signature_type.as_legacy_sdk()?);

        let mut trade_address = Some(signer.address());
        if let Some(funder) = credentials
            .funder_address
            .as_ref()
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
        {
            let funder = SdkAddress::from_str(funder).map_err(|error| {
                ExecutionError::AuthFailure(format!("invalid POLYMARKET_FUNDER_ADDRESS: {error}"))
            })?;
            trade_address = Some(funder);
            auth_builder = auth_builder.funder(funder);
        }

        let client = auth_builder.authenticate().await.map_err(map_sdk_error)?;
        let data_client = SdkDataClient::new(data_api_url.as_str()).map_err(map_sdk_error)?;
        let relayer_client = CtfRelayerClient::new(CtfRelayerConfig {
            relayer_url: config.relayer_url.clone(),
            api_key: config.relayer_api_key.clone(),
            api_key_address: config.relayer_api_key_address.clone(),
            ctf_contract_address: config.ctf_contract_address.clone(),
            collateral_token_address: config.ctf_collateral_token_address.clone(),
            collateral_decimals: config.collateral_decimals,
            proxy_wallet_address: relayer_proxy_wallet_address(
                &config,
                &credentials.funder_address,
            ),
            signature_type_code: credentials.signature_type.as_polymarket_code(),
            polygon_rpc_url: config.polygon_rpc_url.clone(),
        });

        Ok(Self {
            _config: PolymarketConfig {
                credentials: None,
                ..config
            },
            signer,
            signature_type: credentials.signature_type,
            client,
            data_client,
            relayer_client,
            raw_http: reqwest::Client::new(),
            trade_address,
            state: Arc::new(RwLock::new(AdapterState::default())),
            _stored_private_key: Some(credentials.private_key.clone()),
            v2_sdk_client: Arc::new(tokio::sync::OnceCell::new()),
        })
    }

    pub async fn connect_with_config(config: PolymarketConfig) -> Result<Self, ExecutionError> {
        let credentials = config.credentials.clone().ok_or_else(|| {
            ExecutionError::AuthFailure("missing Polymarket live credentials".to_string())
        })?;
        let api_key = Uuid::parse_str(credentials.api_key.trim()).map_err(|error| {
            ExecutionError::AuthFailure(format!("invalid POLYMARKET_API_KEY: {error}"))
        })?;
        let sdk_credentials = SdkCredentials::new(
            api_key,
            credentials.api_secret.clone(),
            credentials.api_passphrase.clone(),
        );
        let signer = PrivateKeySigner::from_str(credentials.private_key.trim())
            .map_err(|error| {
                ExecutionError::AuthFailure(format!("invalid POLYMARKET_PRIVATE_KEY: {error}"))
            })?
            .with_chain_id(Some(POLYGON));

        let mut auth_builder = SdkClobClient::new(
            config.api_url.as_str(),
            SdkClobConfig::builder().use_server_time(true).build(),
        )
        .map_err(map_sdk_error)?
        .authentication_builder(&signer)
        .credentials(sdk_credentials)
        .signature_type(credentials.signature_type.as_legacy_sdk()?);

        let mut trade_address = Some(signer.address());
        if let Some(funder) = credentials
            .funder_address
            .as_ref()
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
        {
            let funder = SdkAddress::from_str(funder).map_err(|error| {
                ExecutionError::AuthFailure(format!("invalid POLYMARKET_FUNDER_ADDRESS: {error}"))
            })?;
            trade_address = Some(funder);
            auth_builder = auth_builder.funder(funder);
        }

        let client = auth_builder.authenticate().await.map_err(map_sdk_error)?;
        let data_client =
            SdkDataClient::new(config.data_api_url.as_str()).map_err(map_sdk_error)?;
        let relayer_client = CtfRelayerClient::new(CtfRelayerConfig {
            relayer_url: config.relayer_url.clone(),
            api_key: config.relayer_api_key.clone(),
            api_key_address: config.relayer_api_key_address.clone(),
            ctf_contract_address: config.ctf_contract_address.clone(),
            collateral_token_address: config.ctf_collateral_token_address.clone(),
            collateral_decimals: config.collateral_decimals,
            proxy_wallet_address: relayer_proxy_wallet_address(
                &config,
                &credentials.funder_address,
            ),
            signature_type_code: credentials.signature_type.as_polymarket_code(),
            polygon_rpc_url: config.polygon_rpc_url.clone(),
        });

        Ok(Self {
            _config: config,
            signer,
            signature_type: credentials.signature_type,
            client,
            data_client,
            relayer_client,
            raw_http: reqwest::Client::new(),
            trade_address,
            state: Arc::new(RwLock::new(AdapterState::default())),
            _stored_private_key: Some(credentials.private_key.clone()),
            v2_sdk_client: Arc::new(tokio::sync::OnceCell::new()),
        })
    }

    /// Lazy-initialises and caches the authenticated V2 SDK client.
    /// First call performs the auth roundtrip (~150ms) and, with the
    /// `heartbeats` feature on, spawns a background task that keeps
    /// our session alive on the venue. Subsequent calls return a cheap
    /// `Clone` of the cached `Arc`-backed client.
    async fn ensure_v2_sdk_client(
        &self,
        sdk_signer: &alloy::signers::local::LocalSigner<alloy::signers::k256::ecdsa::SigningKey>,
        signature_type_v2: polymarket_client_sdk_v2::clob::types::SignatureType,
    ) -> Result<V2SdkAuthClient, ExecutionError> {
        use polymarket_client_sdk_v2::auth::{Credentials as SdkV2Credentials, Uuid as SdkV2Uuid};
        use polymarket_client_sdk_v2::clob::{Client as SdkV2Client, Config as SdkV2Config};

        let cell = self.v2_sdk_client.clone();
        let api_url = self._config.api_url.clone();
        let creds_opt = self._config.credentials.clone();
        // V2 SDK rejects 'funder' on EOA orders (it's only meaningful for
        // proxy/Safe wallets). Only pass it when we're in proxy/Safe mode
        // AND the funder is actually different from the signer's own address.
        let signer_addr = sdk_signer.address();
        let funder = match self.signature_type {
            PolymarketSignatureType::Eoa => None,
            _ => self.trade_address.filter(|addr| *addr != signer_addr),
        };
        let signer = sdk_signer.clone();

        let client_ref = cell
            .get_or_try_init(|| async move {
                let mut auth_builder = SdkV2Client::new(&api_url, SdkV2Config::default())
                    .map_err(|error| {
                        ExecutionError::TransientNetwork(format!(
                            "V2 SDK Client::new failed: {error}"
                        ))
                    })?
                    .authentication_builder(&signer)
                    .signature_type(signature_type_v2);
                if let Some(credentials) = creds_opt.as_ref() {
                    let api_key =
                        SdkV2Uuid::parse_str(credentials.api_key.trim()).map_err(|error| {
                            ExecutionError::AuthFailure(format!(
                                "invalid POLYMARKET_API_KEY: {error}"
                            ))
                        })?;
                    auth_builder = auth_builder.credentials(SdkV2Credentials::new(
                        api_key,
                        credentials.api_secret.clone(),
                        credentials.api_passphrase.clone(),
                    ));
                }
                if let Some(funder) = funder {
                    auth_builder = auth_builder.funder(funder);
                }
                let client = auth_builder.authenticate().await.map_err(|error| {
                    ExecutionError::AuthFailure(format!("V2 SDK authenticate failed: {error}"))
                })?;
                // Refresh venue's cached on-chain balance/allowance view.
                // Without this the venue may reject /order with
                // "not enough balance / allowance" even when our approvals
                // are correctly set on chain (their balance check is
                // cached and lazily-refreshed otherwise).
                use polymarket_client_sdk_v2::clob::types::request::UpdateBalanceAllowanceRequest;
                use polymarket_client_sdk_v2::clob::types::AssetType;
                let req = UpdateBalanceAllowanceRequest::builder()
                    .asset_type(AssetType::Collateral)
                    .build();
                if let Err(e) = client.update_balance_allowance(req).await {
                    tracing::warn!(error = %e, "update_balance_allowance failed (non-fatal)");
                }
                Ok(client)
            })
            .await?;
        Ok(client_ref.clone())
    }

    /// Checks whether a single live order is currently scoring for
    /// maker rewards. Returns the venue's boolean. Logs a structured
    /// event so operators can see (a) which orders qualify and (b)
    /// whether the strategy is actually capturing the rebate side of
    /// the edge whales rely on. Cheap GET (~30ms after warm-up since
    /// the cached client is reused).
    pub async fn check_order_scoring(&self, venue_order_id: &str) -> Result<bool, ExecutionError> {
        use polymarket_client_sdk_v2::clob::types::SignatureType as SdkV2SigType;
        let signature_type_v2 = match self.signature_type {
            PolymarketSignatureType::Eoa => SdkV2SigType::Eoa,
            PolymarketSignatureType::Proxy => SdkV2SigType::Proxy,
            PolymarketSignatureType::GnosisSafe => SdkV2SigType::GnosisSafe,
            PolymarketSignatureType::Poly1271 => SdkV2SigType::Poly1271,
        };
        let pk_hex = self._stored_private_key.as_deref().ok_or_else(|| {
            ExecutionError::AuthFailure(
                "check_order_scoring requires _stored_private_key".to_string(),
            )
        })?;
        let sdk_signer =
            alloy::signers::local::LocalSigner::from_str(pk_hex.trim()).map_err(|error| {
                ExecutionError::AuthFailure(format!("invalid private key for V2 SDK: {error}"))
            })?;
        use alloy::signers::Signer as _;
        let sdk_signer = sdk_signer.with_chain_id(Some(polymarket_client_sdk_v2::POLYGON));
        let client = self
            .ensure_v2_sdk_client(&sdk_signer, signature_type_v2)
            .await?;
        let resp = client
            .is_order_scoring(venue_order_id)
            .await
            .map_err(|error| {
                ExecutionError::TransientNetwork(format!("is_order_scoring: {error}"))
            })?;
        tracing::info!(
            target: "order_scoring",
            venue_order_id = %venue_order_id,
            scoring = resp.scoring,
            "rebate eligibility checked"
        );
        Ok(resp.scoring)
    }

    /// Batch version of [`Self::check_order_scoring`]. Returns a map of
    /// `order_id` -> `is_scoring`. One HTTP request regardless of
    /// `venue_order_ids.len()` — preferred when checking many orders
    /// (e.g. periodic sweep over open orders).
    pub async fn check_orders_scoring(
        &self,
        venue_order_ids: &[&str],
    ) -> Result<std::collections::HashMap<String, bool>, ExecutionError> {
        if venue_order_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        use polymarket_client_sdk_v2::clob::types::SignatureType as SdkV2SigType;
        let signature_type_v2 = match self.signature_type {
            PolymarketSignatureType::Eoa => SdkV2SigType::Eoa,
            PolymarketSignatureType::Proxy => SdkV2SigType::Proxy,
            PolymarketSignatureType::GnosisSafe => SdkV2SigType::GnosisSafe,
            PolymarketSignatureType::Poly1271 => SdkV2SigType::Poly1271,
        };
        let pk_hex = self._stored_private_key.as_deref().ok_or_else(|| {
            ExecutionError::AuthFailure(
                "check_orders_scoring requires _stored_private_key".to_string(),
            )
        })?;
        let sdk_signer =
            alloy::signers::local::LocalSigner::from_str(pk_hex.trim()).map_err(|error| {
                ExecutionError::AuthFailure(format!("invalid private key for V2 SDK: {error}"))
            })?;
        use alloy::signers::Signer as _;
        let sdk_signer = sdk_signer.with_chain_id(Some(polymarket_client_sdk_v2::POLYGON));
        let client = self
            .ensure_v2_sdk_client(&sdk_signer, signature_type_v2)
            .await?;
        let map = client
            .are_orders_scoring(venue_order_ids)
            .await
            .map_err(|error| {
                ExecutionError::TransientNetwork(format!("are_orders_scoring: {error}"))
            })?;
        let scoring_count = map.values().filter(|s| **s).count();
        tracing::info!(
            target: "order_scoring",
            checked = venue_order_ids.len(),
            scoring = scoring_count,
            non_scoring = venue_order_ids.len() - scoring_count,
            "batch rebate eligibility checked"
        );
        Ok(map)
    }

    /// Posts a V2 order via the official polymarket_client_sdk_v2.
    /// Delegates all EIP-712 signing + JSON serialization to the SDK so
    /// our orders match exactly what the venue expects. Slower than
    /// our custom path (one auth roundtrip per call) but correct.
    async fn submit_v2_via_sdk(
        &self,
        req: SubmitOrderRequest,
    ) -> Result<SubmitOrderAck, ExecutionError> {
        use alloy::signers::local::LocalSigner as SdkLocalSigner;
        use alloy::signers::Signer as _;
        use polymarket_client_sdk_v2::clob::types::{
            Amount as SdkV2Amount, OrderType as SdkV2OrderType, Side as SdkV2Side,
            SignatureType as SdkV2SigType,
        };
        use polymarket_client_sdk_v2::types::{Decimal as SdkV2Decimal, U256 as SdkV2U256};
        use polymarket_client_sdk_v2::POLYGON as SDK_V2_POLYGON;

        // Map our types to SDK V2 types.
        let token_id = SdkV2U256::from_str(req.instrument_id.as_str()).map_err(|error| {
            ExecutionError::BadRequest(format!(
                "invalid Polymarket token id `{}`: {error}",
                req.instrument_id
            ))
        })?;
        let side = match req.side {
            crate::types::TradeSide::Buy => SdkV2Side::Buy,
            crate::types::TradeSide::Sell => SdkV2Side::Sell,
        };
        let order_type = match req.time_in_force {
            TimeInForce::Ioc => SdkV2OrderType::FAK,
            TimeInForce::Fok => SdkV2OrderType::FOK,
            TimeInForce::Gtc => SdkV2OrderType::GTC,
            TimeInForce::Gtd => SdkV2OrderType::GTD,
        };
        let signature_type_v2 = match self.signature_type {
            PolymarketSignatureType::Eoa => SdkV2SigType::Eoa,
            PolymarketSignatureType::Proxy => SdkV2SigType::Proxy,
            PolymarketSignatureType::GnosisSafe => SdkV2SigType::GnosisSafe,
            PolymarketSignatureType::Poly1271 => SdkV2SigType::Poly1271,
        };

        // Re-derive a LocalSigner from the stored private key. Alloy's
        // LocalSigner == PrivateKeySigner; this is just a fresh instance
        // with the chain_id set for the SDK.
        let pk_hex = self._stored_private_key.as_deref().ok_or_else(|| {
            ExecutionError::AuthFailure(
                "submit_v2_via_sdk requires _stored_private_key (set during connect_with_*)"
                    .to_string(),
            )
        })?;
        let sdk_signer = SdkLocalSigner::from_str(pk_hex.trim())
            .map_err(|error| {
                ExecutionError::AuthFailure(format!("invalid private key for V2 SDK: {error}"))
            })?
            .with_chain_id(Some(SDK_V2_POLYGON));

        // Use cached authenticated V2 SDK client. First call pays the
        // ~150ms auth roundtrip + starts the SDK's automatic heartbeat
        // task; subsequent calls clone cheaply (Client uses Arc inside).
        let client = self
            .ensure_v2_sdk_client(&sdk_signer, signature_type_v2)
            .await?;

        // Build, sign, and post the order via the SDK.
        let price = SdkV2Decimal::from_str(&Self::v2_decimal_string(req.limit_price, 2, false)?)
            .map_err(|error| {
                ExecutionError::BadRequest(format!("invalid V2 SDK price: {error}"))
            })?;
        let size = SdkV2Decimal::from_str(&Self::v2_decimal_string(req.quantity, 2, false)?)
            .map_err(|error| ExecutionError::BadRequest(format!("invalid V2 SDK size: {error}")))?;
        let market_buy_amount = SdkV2Decimal::from_str(&Self::v2_market_buy_amount_usdc(
            req.limit_price,
            req.quantity,
        )?)
        .map_err(|error| {
            ExecutionError::BadRequest(format!("invalid V2 SDK market buy amount: {error}"))
        })?;

        // V2 keeps expiration outside the signed order. GTC uses epoch
        // expiration (0), matching the public migration docs; GTD uses
        // the requested future expiry or defaults to 1h ahead.
        // V2 SDK validates: "Only GTD orders may have a non-zero expiration".
        // Both GTC and IOC must pass expiration=0 (epoch). Previously this
        // only zeroed for GTC; IOC fell through to the 1h default and the
        // V2 SDK rejected every hedge-rescue submit.
        let expiration_dt = Self::v2_sdk_expiration_dt(&req, now_unix_ms());

        // Set builder_code from our config (defaults to ZERO if not set,
        // but ZERO causes some venue-side mismatches in V2).
        let builder_code_b256 = parse_bytes32(&self._config.v2_builder_code, "builder")?;

        let resp = if req.side == TradeSide::Buy
            && matches!(req.time_in_force, TimeInForce::Ioc | TimeInForce::Fok)
            && !req.post_only
        {
            // V2 validates market-buy collateral on the maker side, not just
            // share size. A BUY FAK encoded as a limit order with size=5.88
            // and price=0.81 creates makerAmount=$4.7628 and is rejected:
            // "maker amount supports a max accuracy of 2 decimals". Use the
            // SDK market-order path with a cent-rounded USDC amount so close /
            // hedge-rescue orders are venue-valid.
            let amount = SdkV2Amount::usdc(market_buy_amount).map_err(|error| {
                ExecutionError::BadRequest(format!("invalid V2 SDK USDC amount: {error}"))
            })?;
            client
                .market_order()
                .token_id(token_id)
                .side(side)
                .price(price)
                .amount(amount)
                .order_type(order_type)
                .builder_code(builder_code_b256)
                .build_sign_and_post(&sdk_signer)
                .await
        } else {
            client
                .limit_order()
                .token_id(token_id)
                .side(side)
                .price(price)
                .size(size)
                .order_type(order_type)
                .expiration(expiration_dt)
                .post_only(req.post_only)
                .builder_code(builder_code_b256)
                .build_sign_and_post(&sdk_signer)
                .await
        }
        .map_err(|error| {
            ExecutionError::VenueRejection(format!("V2 SDK build_sign_and_post: {error}"))
        })?;

        let now_ms = now_unix_ms();
        Ok(SubmitOrderAck {
            client_order_id: req.client_order_id,
            venue_order_id: Some(OrderId::from(resp.order_id.to_string())),
            accepted: true,
            accepted_at_ms: now_ms,
            venue_message: Some(format!("v2-sdk status={:?}", resp.status)),
        })
    }

    async fn submit_v2(&self, req: SubmitOrderRequest) -> Result<SubmitOrderAck, ExecutionError> {
        // Prefer the V2 SDK path when stored private key is available
        // (set by connect_with_config / connect_with_l1_config). Falls
        // back to the custom path otherwise (e.g. tests without a key).
        if self._stored_private_key.is_some() {
            return self.submit_v2_via_sdk(req).await;
        }
        let order_type = Self::v2_order_type(&req)?;
        let builder_code = parse_bytes32(&self._config.v2_builder_code, "builder")?;
        let metadata = parse_bytes32(&self._config.v2_metadata, "metadata")?;
        let maker = self.trade_address.unwrap_or_else(|| self.signer.address());
        let signer = self.signer.address();
        let now_ms = now_unix_ms();
        let expiration_s = if matches!(req.time_in_force, TimeInForce::Gtd) {
            let expires_at_ms = req.expires_at_ms.ok_or_else(|| {
                ExecutionError::BadRequest(
                    "GTD CLOB V2 order requires expires_at_ms on submit request".to_string(),
                )
            })?;
            Self::venue_gtd_expiration_ms(expires_at_ms) / 1_000
        } else {
            0
        };
        let draft = V2OrderDraft::from_submit_request(
            &req,
            V2OrderBuildParams {
                maker,
                signer,
                signature_type: self.signature_type as u8,
                timestamp_ms: now_ms,
                builder_code,
                metadata,
                salt: v2_salt(),
                expiration_s,
            },
        )?;
        let exchange = if self._config.v2_neg_risk {
            SdkAddress::from_str(CLOB_V2_NEG_RISK_EXCHANGE)
        } else {
            SdkAddress::from_str(CLOB_V2_EXCHANGE)
        }
        .map_err(|error| {
            ExecutionError::BadRequest(format!("invalid configured CLOB V2 exchange: {error}"))
        })?;
        let signature = draft.sign(&self.signer, POLYGON, exchange).await?;
        let credentials = self.client.credentials();
        let body = draft.post_body(
            credentials.key().to_string(),
            order_type,
            req.post_only,
            signature,
        )?;
        let body_json = serde_json::to_string(&body).map_err(|error| {
            ExecutionError::BadRequest(format!("failed to serialize CLOB V2 order body: {error}"))
        })?;
        let url = join_url(&self._config.api_url, "order");
        let timestamp_s = (now_ms / 1_000) as i64;
        let headers = self.v2_l2_headers(Method::POST, &url, &body_json, timestamp_s)?;
        let response = self
            .raw_http
            .post(&url)
            .headers(headers)
            .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
            .body(body_json)
            .send()
            .await
            .map_err(|error| ExecutionError::TransientNetwork(error.to_string()))?;
        let status = response.status();
        let response_text = response
            .text()
            .await
            .map_err(|error| ExecutionError::TransientNetwork(error.to_string()))?;
        if !status.is_success() {
            return Err(ExecutionError::VenueRejection(format!(
                "CLOB V2 order POST failed {status}: {response_text}"
            )));
        }
        let response: RawPostOrderResponse =
            serde_json::from_str(&response_text).map_err(|error| {
                ExecutionError::VenueRejection(format!(
                    "failed to decode CLOB V2 order response `{response_text}`: {error}"
                ))
            })?;
        let ack = SubmitOrderAck {
            client_order_id: req.client_order_id.clone(),
            venue_order_id: if response.order_id.is_empty() {
                None
            } else {
                Some(OrderId::from(response.order_id.clone()))
            },
            accepted: response.success,
            accepted_at_ms: now_unix_ms(),
            venue_message: response.error_msg,
        };
        if ack.accepted {
            if let Some(order_id) = ack.venue_order_id.clone() {
                self.state
                    .write()
                    .await
                    .venue_order_map
                    .insert(req.client_order_id, order_id);
            }
        }
        Ok(ack)
    }

    fn v2_l2_headers(
        &self,
        method: Method,
        url: &str,
        body: &str,
        timestamp_s: i64,
    ) -> Result<HeaderMap, ExecutionError> {
        let request = self
            .raw_http
            .request(method.clone(), url)
            .body(body.to_string())
            .build()
            .map_err(|error| ExecutionError::BadRequest(error.to_string()))?;
        let path = request.url().path();
        let message = format!("{timestamp_s}{method}{path}{body}");
        let credentials = self.client.credentials();
        let signature = l2_hmac(credentials.secret().expose_secret(), &message)?;
        let mut headers = HeaderMap::new();
        headers.insert(
            "POLY_ADDRESS",
            HeaderValue::from_str(&self.signer.address().to_string())
                .map_err(|error| ExecutionError::AuthFailure(error.to_string()))?,
        );
        headers.insert(
            "POLY_API_KEY",
            HeaderValue::from_str(&credentials.key().to_string())
                .map_err(|error| ExecutionError::AuthFailure(error.to_string()))?,
        );
        headers.insert(
            "POLY_PASSPHRASE",
            HeaderValue::from_str(credentials.passphrase().expose_secret())
                .map_err(|error| ExecutionError::AuthFailure(error.to_string()))?,
        );
        headers.insert(
            "POLY_SIGNATURE",
            HeaderValue::from_str(&signature)
                .map_err(|error| ExecutionError::AuthFailure(error.to_string()))?,
        );
        headers.insert(
            "POLY_TIMESTAMP",
            HeaderValue::from_str(&timestamp_s.to_string())
                .map_err(|error| ExecutionError::AuthFailure(error.to_string()))?,
        );
        Ok(headers)
    }

    pub fn api_credentials(&self) -> (String, String, String) {
        let credentials = self.client.credentials();
        (
            credentials.key().to_string(),
            credentials.secret().expose_secret().to_string(),
            credentials.passphrase().expose_secret().to_string(),
        )
    }

    pub async fn ensure_pusd_collateral_from_usdce(
        &self,
        min_wrap_usd: f64,
    ) -> Result<Option<PusdWrapReport>, ExecutionError> {
        if self.signature_type != PolymarketSignatureType::Eoa {
            return Ok(None);
        }
        let rpc_url = self
            ._config
            .polygon_rpc_url
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                ExecutionError::BadRequest(
                    "pUSD auto-wrap requires POLYGON_RPC_URL in EOA mode".to_string(),
                )
            })?;
        let recipient = self.trade_address.unwrap_or_else(|| self.signer.address());
        let min_wrap_amount = scaled_usdc_units(min_wrap_usd)?;
        let submitter = EoaPolygonSubmitter::from_env(rpc_url.to_string());
        submitter
            .ensure_pusd_from_usdce(&self.signer, recipient, min_wrap_amount)
            .await
            .map(Some)
    }

    fn map_order_type(req: &SubmitOrderRequest) -> Result<SdkOrderType, ExecutionError> {
        match req.time_in_force {
            TimeInForce::Gtc => Ok(SdkOrderType::GTC),
            TimeInForce::Ioc => {
                if req.post_only {
                    Err(ExecutionError::BadRequest(
                        "post-only IOC/FAK orders are invalid on Polymarket".to_string(),
                    ))
                } else {
                    Ok(SdkOrderType::FAK)
                }
            }
            TimeInForce::Fok => {
                if req.post_only {
                    Err(ExecutionError::BadRequest(
                        "post-only FOK orders are invalid on Polymarket".to_string(),
                    ))
                } else {
                    Ok(SdkOrderType::FOK)
                }
            }
            TimeInForce::Gtd => Ok(SdkOrderType::GTD),
        }
    }

    fn v2_order_type(req: &SubmitOrderRequest) -> Result<&'static str, ExecutionError> {
        match req.time_in_force {
            TimeInForce::Gtc => Ok("GTC"),
            TimeInForce::Gtd => Ok("GTD"),
            TimeInForce::Ioc => {
                if req.post_only {
                    Err(ExecutionError::BadRequest(
                        "post-only IOC/FAK orders are invalid on Polymarket CLOB V2".to_string(),
                    ))
                } else {
                    Ok("FAK")
                }
            }
            TimeInForce::Fok => {
                if req.post_only {
                    Err(ExecutionError::BadRequest(
                        "post-only FOK orders are invalid on Polymarket CLOB V2".to_string(),
                    ))
                } else {
                    Ok("FOK")
                }
            }
        }
    }

    fn v2_sdk_expiration_dt(
        req: &SubmitOrderRequest,
        now_ms: EpochMillis,
    ) -> chrono::DateTime<chrono::Utc> {
        use chrono::{TimeZone, Utc};

        let expiration_ms = match req.time_in_force {
            TimeInForce::Gtc | TimeInForce::Ioc | TimeInForce::Fok => 0,
            TimeInForce::Gtd => req
                .expires_at_ms
                .filter(|ms| *ms > now_ms + 60_000)
                .unwrap_or_else(|| now_ms + 3_600_000),
        };
        Utc.timestamp_millis_opt(expiration_ms as i64)
            .single()
            .unwrap_or_else(|| Utc.timestamp_opt(0, 0).unwrap())
    }

    fn v2_decimal_string(
        value: f64,
        decimal_places: u32,
        round_up: bool,
    ) -> Result<String, ExecutionError> {
        if !value.is_finite() || value <= 0.0 {
            return Err(ExecutionError::BadRequest(format!(
                "V2 decimal must be positive and finite, got {value}"
            )));
        }
        let scale = 10_f64.powi(decimal_places as i32);
        let scaled = if round_up {
            (value * scale).ceil()
        } else {
            (value * scale).floor()
        };
        if !scaled.is_finite() || scaled <= 0.0 {
            return Err(ExecutionError::BadRequest(format!(
                "V2 decimal rounded to zero, got {value}"
            )));
        }
        Ok(format!("{:.*}", decimal_places as usize, scaled / scale))
    }

    fn v2_market_buy_amount_usdc(
        limit_price: f64,
        quantity: f64,
    ) -> Result<String, ExecutionError> {
        let notional = limit_price * quantity;
        Self::v2_decimal_string(notional, 2, true)
    }

    fn sdk_side(side: TradeSide) -> SdkSide {
        match side {
            TradeSide::Buy => SdkSide::Buy,
            TradeSide::Sell => SdkSide::Sell,
        }
    }

    fn decimal_from_f64(
        value: f64,
        scale: usize,
        field: &str,
    ) -> Result<SdkDecimal, ExecutionError> {
        if !value.is_finite() || value <= 0.0 {
            return Err(ExecutionError::BadRequest(format!(
                "{field} must be positive and finite, got {value}"
            )));
        }
        format!("{value:.scale$}")
            .parse::<SdkDecimal>()
            .map_err(|error| ExecutionError::BadRequest(format!("invalid {field}: {error}")))
    }

    fn venue_gtd_expiration_ms(intended_expires_at_ms: EpochMillis) -> EpochMillis {
        // Polymarket requires the submitted GTD expiration to include a one
        // minute security threshold. The executor still cancels locally at the
        // intended max age; this only satisfies the venue's signing contract.
        intended_expires_at_ms.saturating_add(60_000)
    }

    fn usdc_balance_to_usd(raw: &str) -> f64 {
        let trimmed = raw.trim();
        let value = trimmed.parse::<f64>().unwrap_or(0.0);
        if trimmed.contains('.') {
            value
        } else {
            value / 1_000_000.0
        }
    }

    fn venue_position_from_data_position(
        position: &DataPosition,
        market_id_by_asset: &HashMap<String, String>,
    ) -> VenuePosition {
        let asset = position.asset.to_string();
        let condition_id = format!("{:#x}", position.condition_id);
        let quantity = position.size.to_string().parse::<f64>().unwrap_or(0.0);
        let average_cost_usd = {
            let avg = position.avg_price.to_string().parse::<f64>().unwrap_or(0.0);
            if avg.is_finite() && avg > 0.0 {
                avg
            } else {
                let total_bought = position
                    .total_bought
                    .to_string()
                    .parse::<f64>()
                    .unwrap_or(0.0);
                let initial_value = position
                    .initial_value
                    .to_string()
                    .parse::<f64>()
                    .unwrap_or(0.0);
                if quantity > 0.0 && total_bought.is_finite() && total_bought > 0.0 {
                    total_bought / quantity
                } else if quantity > 0.0 && initial_value.is_finite() && initial_value > 0.0 {
                    initial_value / quantity
                } else {
                    0.0
                }
            }
        };
        VenuePosition {
            market_id: MarketId::from(
                market_id_by_asset
                    .get(&asset)
                    .cloned()
                    .unwrap_or_else(|| condition_id.clone()),
            ),
            condition_id: Some(condition_id),
            instrument_id: InstrumentId::from(asset),
            quantity,
            average_cost_usd,
            redeemable: position.redeemable,
            mergeable: position.mergeable,
            current_value_usd: position
                .current_value
                .to_string()
                .parse::<f64>()
                .unwrap_or(0.0),
        }
    }

    async fn sync_positions_from_data_api(&self) -> Result<Vec<VenuePosition>, ExecutionError> {
        let Some(trade_address) = self.trade_address else {
            return Err(ExecutionError::AuthFailure(
                "cannot sync positions without signer or funder address".to_string(),
            ));
        };
        let request = DataPositionsRequest::builder()
            .user(trade_address)
            .size_threshold(SdkDecimal::ZERO)
            .limit(500)
            .map_err(|error| {
                ExecutionError::BadRequest(format!("invalid Data API positions request: {error}"))
            })?
            .build();
        let positions = self
            .data_client
            .positions(&request)
            .await
            .map_err(map_sdk_error)?;
        Ok(positions
            .iter()
            .map(|position| {
                Self::venue_position_from_data_position(position, &self._config.market_id_by_asset)
            })
            .filter(|position| position.quantity > 1e-9)
            .collect())
    }

    /// Fetch venue-authoritative metadata (minimum_order_size, minimum_tick_size,
    /// neg_risk, lifecycle flags) for a market by `condition_id`. Closes Q6 of the
    /// handoff audit: surfaces the truth that the strategy's hardcoded sizing
    /// multipliers should be reconciled against. Operator-visible only; does not
    /// modify strategy behavior.
    pub async fn fetch_market_metadata(
        &self,
        condition_id: &str,
    ) -> Result<MarketMetadata, ExecutionError> {
        let trimmed = condition_id.trim();
        if trimmed.is_empty() {
            return Err(ExecutionError::BadRequest(
                "condition_id required for fetch_market_metadata".to_string(),
            ));
        }
        let url = join_url(&self._config.api_url, &format!("markets/{trimmed}"));
        let response = self
            .raw_http
            .get(&url)
            .send()
            .await
            .map_err(|error| ExecutionError::TransientNetwork(error.to_string()))?;
        let status = response.status();
        let response_text = response
            .text()
            .await
            .map_err(|error| ExecutionError::TransientNetwork(error.to_string()))?;
        if !status.is_success() {
            return Err(ExecutionError::VenueRejection(format!(
                "GET {url} failed {status}: {response_text}"
            )));
        }
        parse_market_metadata(&response_text)
    }

    async fn sync_open_orders_from_client(&self) -> Result<Vec<VenueOpenOrder>, ExecutionError> {
        let page = self
            .client
            .orders(&OrdersRequest::default(), None)
            .await
            .map_err(map_sdk_error)?;
        let mut orders = Vec::new();
        for item in page.data {
            if !matches!(
                item.status,
                OrderStatusType::Live | OrderStatusType::Delayed
            ) {
                continue;
            }
            let original_qty = item.original_size.to_string().parse::<f64>().unwrap_or(0.0);
            let matched_qty = item.size_matched.to_string().parse::<f64>().unwrap_or(0.0);
            orders.push(VenueOpenOrder {
                venue_order_id: OrderId::from(item.id),
                client_order_id: None,
                market_id: MarketId::from(format!("{:#x}", item.market)),
                instrument_id: InstrumentId::from(item.asset_id.to_string()),
                side: match item.side {
                    SdkSide::Buy => TradeSide::Buy,
                    _ => TradeSide::Sell,
                },
                limit_price: item.price.to_string().parse::<f64>().unwrap_or(0.0),
                original_qty,
                remaining_qty: (original_qty - matched_qty).max(0.0),
                created_at_ms: item.created_at.timestamp_millis().max(0) as u64,
            });
        }

        Ok(orders)
    }

    async fn sync_balances_from_client(&self) -> Result<VenueBalances, ExecutionError> {
        let response = self
            .client
            .balance_allowance(BalanceAllowanceRequest::default())
            .await
            .map_err(map_sdk_error)?;
        let raw_balance = response.balance.to_string();
        let positions = self.sync_positions_from_data_api().await?;
        Ok(VenueBalances {
            cash_usd: Self::usdc_balance_to_usd(&raw_balance),
            positions,
            positions_authoritative: true,
            observed_at_ms: now_unix_ms(),
        })
    }

    async fn sync_recent_fills_from_client(
        &self,
        after_ms: EpochMillis,
    ) -> Result<Vec<VenueFill>, ExecutionError> {
        let Some(trade_address) = self.trade_address else {
            return Ok(Vec::new());
        };
        let mut fills = Vec::new();

        fills.extend(
            self.sync_raw_recent_fills_page(RawTradeFilter::Maker, trade_address, after_ms)
                .await?,
        );
        fills.extend(
            self.sync_raw_recent_fills_page(RawTradeFilter::Taker, trade_address, after_ms)
                .await?,
        );

        let mut seen = HashSet::new();
        fills.retain(|fill| {
            seen.insert(format!(
                "{}:{}:{}:{:?}:{}:{}",
                fill.venue_order_id,
                fill.market_id,
                fill.instrument_id,
                fill.side,
                fill.price,
                fill.quantity
            ))
        });
        Ok(fills)
    }

    async fn sync_raw_recent_fills_page(
        &self,
        filter: RawTradeFilter,
        trade_address: SdkAddress,
        after_ms: EpochMillis,
    ) -> Result<Vec<VenueFill>, ExecutionError> {
        let url = join_url(&self._config.api_url, "data/trades");
        let params = vec![
            (filter.query_key(), trade_address.to_string()),
            ("after", (after_ms / 1_000).to_string()),
        ];
        let timestamp_s = (now_unix_ms() / 1_000) as i64;
        let headers = self.v2_l2_headers(Method::GET, &url, "", timestamp_s)?;
        let response = self
            .raw_http
            .get(&url)
            .headers(headers)
            .query(&params)
            .send()
            .await
            .map_err(|error| ExecutionError::TransientNetwork(error.to_string()))?;
        let status = response.status();
        let response_text = response
            .text()
            .await
            .map_err(|error| ExecutionError::TransientNetwork(error.to_string()))?;
        if !status.is_success() {
            return Err(ExecutionError::VenueRejection(format!(
                "CLOB trades GET failed {status}: {response_text}"
            )));
        }
        parse_raw_trades_page(&response_text, filter, trade_address)
    }
}

fn relayer_proxy_wallet_address(
    config: &PolymarketConfig,
    funder_address: &Option<String>,
) -> Option<String> {
    config
        .proxy_wallet_address
        .clone()
        .or_else(|| funder_address.clone())
}

#[async_trait]
impl ExecutionAdapter for PolymarketExecutionAdapter {
    async fn submit(&self, req: SubmitOrderRequest) -> Result<SubmitOrderAck, ExecutionError> {
        if matches!(self._config.protocol, ClobProtocolVersion::V2) {
            return self.submit_v2(req).await;
        }

        let token_id = SdkU256::from_str(req.instrument_id.as_str()).map_err(|error| {
            ExecutionError::BadRequest(format!(
                "invalid Polymarket token id `{}`: {error}",
                req.instrument_id
            ))
        })?;
        let order_type = Self::map_order_type(&req)?;
        let price = Self::decimal_from_f64(req.limit_price, 2, "limit_price")?;
        let size = Self::decimal_from_f64(req.quantity, 2, "quantity")?;
        let mut builder = self
            .client
            .limit_order()
            .token_id(token_id)
            .order_type(order_type)
            .post_only(req.post_only)
            .price(price)
            .size(size)
            .side(Self::sdk_side(req.side));
        if matches!(req.time_in_force, TimeInForce::Gtd) {
            let expires_at_ms = req.expires_at_ms.ok_or_else(|| {
                ExecutionError::BadRequest(
                    "GTD live order requires expires_at_ms on submit request".to_string(),
                )
            })?;
            let venue_expires_at_ms = Self::venue_gtd_expiration_ms(expires_at_ms);
            let expires_at = SdkDateTime::<SdkUtc>::from_timestamp(
                (venue_expires_at_ms / 1_000) as i64,
                ((venue_expires_at_ms % 1_000) * 1_000_000) as u32,
            )
            .ok_or_else(|| {
                ExecutionError::BadRequest(format!(
                    "invalid GTD expiration timestamp {venue_expires_at_ms}"
                ))
            })?;
            builder = builder.expiration(expires_at);
        }
        let order = builder.build().await.map_err(map_sdk_error)?;
        let signed_order = self
            .client
            .sign(&self.signer, order)
            .await
            .map_err(map_sdk_error)?;
        let response = self
            .client
            .post_order(signed_order)
            .await
            .map_err(map_sdk_error)?;

        let accepted = response.success;
        let ack = SubmitOrderAck {
            client_order_id: req.client_order_id.clone(),
            venue_order_id: if response.order_id.is_empty() {
                None
            } else {
                Some(OrderId::from(response.order_id.clone()))
            },
            accepted,
            accepted_at_ms: now_unix_ms(),
            venue_message: response.error_msg,
        };

        if accepted {
            let mut state = self.state.write().await;
            if let Some(order_id) = ack.venue_order_id.clone() {
                state
                    .venue_order_map
                    .insert(req.client_order_id.clone(), order_id);
            }
        }

        Ok(ack)
    }

    async fn cancel(&self, req: CancelOrderRequest) -> Result<CancelOrderAck, ExecutionError> {
        let state = self.state.read().await;
        let venue_order_id = req
            .venue_order_id
            .clone()
            .or_else(|| state.venue_order_map.get(&req.client_order_id).cloned());
        drop(state);

        let Some(order_id) = venue_order_id.clone() else {
            return Err(ExecutionError::BadRequest(format!(
                "cannot cancel {} without venue_order_id",
                req.client_order_id
            )));
        };
        let response = self
            .client
            .cancel_order(order_id.as_str())
            .await
            .map_err(map_sdk_error)?;

        let accepted = response
            .canceled
            .iter()
            .any(|canceled| canceled == order_id.as_str());
        let ack = CancelOrderAck {
            client_order_id: req.client_order_id.clone(),
            venue_order_id: Some(order_id.clone()),
            accepted,
            accepted_at_ms: now_unix_ms(),
            venue_message: response
                .not_canceled
                .get(order_id.as_str())
                .cloned()
                .or_else(|| Some("cancel submitted to Polymarket CLOB".to_string())),
        };

        if accepted {
            let mut state = self.state.write().await;
            state.venue_order_map.remove(&req.client_order_id);
        }

        Ok(ack)
    }

    async fn merge_positions(
        &self,
        req: MergePositionsRequest,
    ) -> Result<MergePositionsAck, ExecutionError> {
        let condition_id = req.condition_id.clone().ok_or_else(|| {
            ExecutionError::BadRequest(format!(
                "cannot merge market={} without condition_id from venue position sync",
                req.market_id
            ))
        })?;
        let metadata = serde_json::json!({
            "source": "polymarket-exec",
            "command_id": req.command_id.as_str(),
            "market_id": req.market_id.as_str(),
            "yes_token_id": req.yes_instrument_id.as_str(),
            "no_token_id": req.no_instrument_id.as_str(),
            "quantity": req.quantity,
        })
        .to_string();
        let ack = self
            .relayer_client
            .merge_positions(CtfMergeRequest {
                signer: self.signer.clone(),
                condition_id,
                quantity: req.quantity,
                metadata,
            })
            .await?;
        Ok(MergePositionsAck {
            command_id: req.command_id,
            accepted: true,
            accepted_at_ms: now_unix_ms(),
            venue_message: Some(format!(
                "relayer merge submitted transaction_id={} state={} hash={}",
                ack.transaction_id.as_deref().unwrap_or("unknown"),
                ack.state.as_deref().unwrap_or("unknown"),
                ack.transaction_hash.as_deref().unwrap_or("unknown")
            )),
        })
    }

    async fn redeem_positions(
        &self,
        req: RedeemPositionsRequest,
    ) -> Result<RedeemPositionsAck, ExecutionError> {
        let metadata = serde_json::json!({
            "source": "polymarket-exec",
            "command_id": req.command_id.as_str(),
            "market_id": req.market_id.as_str(),
            "condition_id": req.condition_id,
            "collateral_token_address": req.collateral_token_address,
            "index_sets": req.index_sets,
        })
        .to_string();
        let ack = self
            .relayer_client
            .redeem_positions(CtfRedeemRequest {
                signer: self.signer.clone(),
                condition_id: req.condition_id.clone(),
                collateral_token_address: req.collateral_token_address.clone(),
                index_sets: req.index_sets.clone(),
                metadata,
            })
            .await?;
        Ok(RedeemPositionsAck {
            command_id: req.command_id,
            accepted: true,
            accepted_at_ms: now_unix_ms(),
            venue_message: Some(format!(
                "relayer redeem submitted transaction_id={} state={} hash={}",
                ack.transaction_id.as_deref().unwrap_or("unknown"),
                ack.state.as_deref().unwrap_or("unknown"),
                ack.transaction_hash.as_deref().unwrap_or("unknown")
            )),
        })
    }

    async fn ensure_pusd_collateral_from_usdce(
        &self,
        min_wrap_usd: f64,
    ) -> Result<Option<PusdWrapReport>, ExecutionError> {
        Self::ensure_pusd_collateral_from_usdce(self, min_wrap_usd).await
    }

    async fn sync_open_orders(&self) -> Result<Vec<VenueOpenOrder>, ExecutionError> {
        self.sync_open_orders_from_client().await
    }

    async fn sync_balances(&self) -> Result<VenueBalances, ExecutionError> {
        self.sync_balances_from_client().await
    }

    async fn sync_recent_fills(
        &self,
        after_ms: EpochMillis,
    ) -> Result<Vec<VenueFill>, ExecutionError> {
        self.sync_recent_fills_from_client(after_ms).await
    }

    async fn check_orders_scoring(
        &self,
        venue_order_ids: &[&str],
    ) -> Result<std::collections::HashMap<String, bool>, ExecutionError> {
        // Delegates to the inherent method (which uses the cached V2
        // SDK auth client to call /orders-scoring batch endpoint).
        Self::check_orders_scoring(self, venue_order_ids).await
    }

    async fn fetch_market_metadata(
        &self,
        condition_id: &str,
    ) -> Result<MarketMetadata, ExecutionError> {
        // Delegates to the inherent method (HTTP GET /markets/{condition_id}).
        Self::fetch_market_metadata(self, condition_id).await
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn v2_salt() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

fn join_url(base: &str, path: &str) -> String {
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

fn parse_market_metadata(body: &str) -> Result<MarketMetadata, ExecutionError> {
    let raw: serde_json::Value = serde_json::from_str(body).map_err(|error| {
        ExecutionError::VenueRejection(format!(
            "failed to decode market metadata response `{body}`: {error}"
        ))
    })?;
    let condition_id = raw
        .get("condition_id")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let minimum_order_size = raw
        .get("minimum_order_size")
        .and_then(|v| match v {
            serde_json::Value::String(s) => s.parse::<f64>().ok(),
            serde_json::Value::Number(n) => n.as_f64(),
            _ => None,
        })
        .unwrap_or(0.0);
    let minimum_tick_size = raw
        .get("minimum_tick_size")
        .and_then(|v| match v {
            serde_json::Value::String(s) => s.parse::<f64>().ok(),
            serde_json::Value::Number(n) => n.as_f64(),
            _ => None,
        })
        .unwrap_or(0.0);
    let neg_risk = raw
        .get("neg_risk")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let active = raw.get("active").and_then(|v| v.as_bool()).unwrap_or(true);
    let closed = raw.get("closed").and_then(|v| v.as_bool()).unwrap_or(false);
    Ok(MarketMetadata {
        condition_id,
        minimum_order_size,
        minimum_tick_size,
        neg_risk,
        active,
        closed,
    })
}

fn l2_hmac(secret: &str, message: &str) -> Result<String, ExecutionError> {
    let decoded_secret = base64::engine::general_purpose::URL_SAFE
        .decode(secret)
        .map_err(|error| {
            ExecutionError::AuthFailure(format!("invalid CLOB API secret: {error}"))
        })?;
    let mut mac = Hmac::<Sha256>::new_from_slice(&decoded_secret).map_err(|error| {
        ExecutionError::AuthFailure(format!("invalid CLOB API secret: {error}"))
    })?;
    mac.update(message.as_bytes());
    Ok(base64::engine::general_purpose::URL_SAFE.encode(mac.finalize().into_bytes()))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPostOrderResponse {
    pub error_msg: Option<String>,
    #[serde(rename = "orderID")]
    pub order_id: String,
    pub success: bool,
}

fn map_sdk_error(error: polymarket_client_sdk::error::Error) -> ExecutionError {
    let message = error.to_string();
    let lower = message.to_ascii_lowercase();
    if lower.contains("400")
        || lower.contains("422")
        || lower.contains("validation")
        || lower.contains("lower than the minimum")
        || lower.contains(" is invalid")
    {
        ExecutionError::BadRequest(message)
    } else if lower.contains("401")
        || lower.contains("403")
        || lower.contains("unauthorized")
        || lower.contains("forbidden")
    {
        ExecutionError::AuthFailure(message)
    } else if lower.contains("429") || lower.contains("rate limit") {
        ExecutionError::RateLimit {
            retry_after_ms: None,
        }
    } else if lower.contains("timeout")
        || lower.contains("connection")
        || lower.contains("network")
        || lower.contains("502")
        || lower.contains("503")
        || lower.contains("504")
    {
        ExecutionError::TransientNetwork(message)
    } else if lower.contains("409") || lower.contains("uncertain") {
        ExecutionError::UncertainOutcome(message)
    } else {
        ExecutionError::VenueRejection(message)
    }
}

#[cfg(test)]
#[path = "../../tests/unit/wire_execution_adapter.rs"]
mod execution_adapter_tests;
