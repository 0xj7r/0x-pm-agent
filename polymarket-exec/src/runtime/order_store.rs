//! Durable SQLite-backed order/signal persistence and recovery primitives.

use std::fmt;
use std::path::PathBuf;

use rusqlite::{params, types::Type, Connection, OptionalExtension, Row};

use crate::runtime::types::ManagedOrderStatus;
use crate::types::{
    ClientOrderId, EpochMillis, InstrumentId, MarketId, OrderId, OrderIntent, RuntimeStatus,
    TradeSide,
};

const DUST_REMAINING_QTY: f64 = 0.01;
const DUST_REMAINING_NOTIONAL_USD: f64 = 0.01;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AccountingLane {
    PairedCore,
    LateFavorite,
    CheapTail,
    ReversalHedge,
    BackToExplore,
    #[default]
    Other,
}

impl AccountingLane {
    pub fn from_quote_level_tag(tag: Option<&str>) -> Self {
        let Some(tag) = tag else {
            return Self::Other;
        };
        let tag = tag.to_ascii_lowercase();
        if tag.starts_with("paired-core:")
            || tag.contains("paired-mm")
            || tag.contains("mm-paired-bid")
        {
            Self::PairedCore
        } else if tag.contains("reversal-hedge") {
            Self::ReversalHedge
        } else if tag.starts_with("bte-taker") || tag.contains("back_to_explore") {
            Self::BackToExplore
        } else if tag.starts_with("cheap-tail") || tag.contains("convex") {
            Self::CheapTail
        } else if tag.starts_with("late-fav") {
            Self::LateFavorite
        } else {
            Self::Other
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::PairedCore => "paired_core",
            Self::LateFavorite => "late_favorite",
            Self::CheapTail => "cheap_tail",
            Self::ReversalHedge => "reversal_hedge",
            Self::BackToExplore => "back_to_explore",
            Self::Other => "other",
        }
    }

    fn from_db(raw: &str) -> std::result::Result<Self, OrderStoreError> {
        match raw {
            "paired_core" => Ok(Self::PairedCore),
            "late_favorite" => Ok(Self::LateFavorite),
            "cheap_tail" => Ok(Self::CheapTail),
            "reversal_hedge" => Ok(Self::ReversalHedge),
            "back_to_explore" => Ok(Self::BackToExplore),
            "other" => Ok(Self::Other),
            value => Err(OrderStoreError::Serialization(format!(
                "invalid accounting lane `{value}`"
            ))),
        }
    }

    pub fn is_directional(self) -> bool {
        matches!(
            self,
            Self::LateFavorite | Self::CheapTail | Self::ReversalHedge | Self::BackToExplore
        )
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct OrderRecord {
    pub run_id: String,
    pub client_order_id: ClientOrderId,
    pub venue_order_id: Option<OrderId>,
    pub market_id: MarketId,
    pub instrument_id: InstrumentId,
    pub side: TradeSide,
    pub limit_price: f64,
    pub reduce_only: bool,
    pub original_qty: f64,
    pub remaining_qty: f64,
    pub filled_qty: f64,
    pub status: ManagedOrderStatus,
    pub submitted_at_ms: EpochMillis,
    pub last_update_ms: EpochMillis,
    pub reason: Option<String>,
    pub strategy_tag: String,
    pub quote_level_tag: Option<String>,
    pub accounting_lane: AccountingLane,
}

impl OrderRecord {
    pub fn from_intent(
        run_id: impl Into<String>,
        intent: &OrderIntent,
        strategy_tag: impl Into<String>,
    ) -> Self {
        Self {
            run_id: run_id.into(),
            client_order_id: intent.client_order_id.clone(),
            venue_order_id: None,
            market_id: intent.market_id.clone(),
            instrument_id: intent.instrument_id.clone(),
            side: intent.side,
            limit_price: intent.limit_price,
            reduce_only: intent.reduce_only,
            original_qty: intent.quantity,
            remaining_qty: intent.quantity,
            filled_qty: 0.0,
            status: ManagedOrderStatus::PendingSubmit,
            submitted_at_ms: intent.created_at_ms,
            last_update_ms: intent.created_at_ms,
            reason: Some(intent.reason.clone()),
            strategy_tag: strategy_tag.into(),
            quote_level_tag: intent.quote_level_tag.clone(),
            accounting_lane: AccountingLane::from_quote_level_tag(
                intent.quote_level_tag.as_deref(),
            ),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RouterDecisionRecord {
    pub run_id: String,
    pub observed_at_ms: EpochMillis,
    pub market_id: MarketId,
    pub cluster: String,
    pub static_cluster_route: String,
    pub raw_effective_router_route: String,
    pub effective_router_route: String,
    pub latched_router_route: String,
    pub selected_router_route: String,
    pub action_router_route: String,
    pub locked_router_route: String,
    pub shadow_vote_route: String,
    pub router_enforce_enabled: bool,
    pub session_guard_active: bool,
    pub session_stress_fraction: f64,
    pub session_observation_count: u64,
    pub session_action_switch_count: u64,
    pub session_risk_off_until_ms: Option<EpochMillis>,
    pub br2_orders: u64,
    pub bte_orders: u64,
    pub br2_submit_intents: u64,
    pub bte_submit_intents: u64,
    pub yes_mid: f64,
    pub market_yes_range_so_far: f64,
    pub whipsaw_score: f64,
    pub path_efficiency: f64,
    pub sign_flip_rate: f64,
    pub reversal_pressure: f64,
    pub realized_vol_180s_bps: f64,
    pub whipsaw_sample_count: u64,
    pub btc_micro_regime: Option<String>,
    pub free_cash_usd: f64,
    pub gross_exposure_usd: f64,
    pub open_orders: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum OrderStoreError {
    Io(String),
    NotFound(ClientOrderId),
    Conflict(String),
    Serialization(String),
    Sqlite(String),
}

impl fmt::Display for OrderStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(message) => write!(f, "order store io error: {message}"),
            Self::NotFound(order_id) => write!(f, "order not found: {order_id}"),
            Self::Conflict(message) => write!(f, "order store conflict: {message}"),
            Self::Serialization(message) => {
                write!(f, "order store serialization error: {message}")
            }
            Self::Sqlite(message) => write!(f, "order store sqlite error: {message}"),
        }
    }
}

impl std::error::Error for OrderStoreError {}

pub trait OrderStore {
    fn insert(&mut self, record: OrderRecord) -> std::result::Result<(), OrderStoreError>;
    fn update_status(
        &mut self,
        client_order_id: &ClientOrderId,
        status: ManagedOrderStatus,
        updated_at_ms: EpochMillis,
    ) -> std::result::Result<(), OrderStoreError>;
    fn attach_venue_id(
        &mut self,
        client_order_id: &ClientOrderId,
        venue_order_id: OrderId,
        updated_at_ms: EpochMillis,
    ) -> std::result::Result<(), OrderStoreError>;
    fn apply_fill(
        &mut self,
        client_order_id: &ClientOrderId,
        fill_qty: f64,
        updated_at_ms: EpochMillis,
    ) -> std::result::Result<(), OrderStoreError>;
    fn expire_remaining_after_partial_fill(
        &mut self,
        client_order_id: &ClientOrderId,
        updated_at_ms: EpochMillis,
    ) -> std::result::Result<(), OrderStoreError>;
    fn get(
        &self,
        client_order_id: &ClientOrderId,
    ) -> std::result::Result<Option<OrderRecord>, OrderStoreError>;
    fn list_open(&self) -> std::result::Result<Vec<OrderRecord>, OrderStoreError>;
    fn list_by_market(
        &self,
        market_id: &MarketId,
    ) -> std::result::Result<Vec<OrderRecord>, OrderStoreError>;
    fn filled_buy_cost_basis(
        &self,
        market_id: &MarketId,
        instrument_id: &InstrumentId,
    ) -> std::result::Result<Option<f64>, OrderStoreError>;
    fn put_strategy_state(
        &mut self,
        run_id: &str,
        strategy_tag: &str,
        observed_at_ms: EpochMillis,
        payload: &serde_json::Value,
    ) -> std::result::Result<(), OrderStoreError>;
    fn latest_strategy_state(
        &self,
        strategy_tag: &str,
    ) -> std::result::Result<Option<serde_json::Value>, OrderStoreError>;
    fn put_runtime_status(
        &mut self,
        run_id: &str,
        observed_at_ms: EpochMillis,
        status: RuntimeStatus,
    ) -> std::result::Result<(), OrderStoreError>;
    fn latest_runtime_status(&self) -> std::result::Result<Option<RuntimeStatus>, OrderStoreError>;
    fn insert_router_decision(
        &mut self,
        record: RouterDecisionRecord,
    ) -> std::result::Result<(), OrderStoreError>;
    fn list_recent_router_decisions(
        &self,
        limit: usize,
    ) -> std::result::Result<Vec<RouterDecisionRecord>, OrderStoreError>;
}

#[derive(Debug)]
pub struct SqliteOrderStore {
    connection: Connection,
}

impl SqliteOrderStore {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, OrderStoreError> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                OrderStoreError::Io(format!(
                    "failed to create sqlite parent dir {}: {}",
                    parent.display(),
                    error
                ))
            })?;
        }

        let connection = Connection::open(&path)
            .map_err(|error| OrderStoreError::Sqlite(format!("failed to open sqlite: {error}")))?;
        let store = Self { connection };
        store.configure_connection()?;
        store.migrate()?;
        Ok(store)
    }

    fn configure_connection(&self) -> std::result::Result<(), OrderStoreError> {
        self.connection
            .execute_batch(
                "PRAGMA busy_timeout = 5000;
                 PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = NORMAL;",
            )
            .map_err(|error| {
                OrderStoreError::Sqlite(format!("failed to configure sqlite connection: {error}"))
            })?;
        Ok(())
    }

    fn migrate(&self) -> std::result::Result<(), OrderStoreError> {
        self.connection
            .execute(
                "CREATE TABLE IF NOT EXISTS orders (
                    client_order_id TEXT PRIMARY KEY,
                    run_id TEXT NOT NULL,
                    venue_order_id TEXT,
                    market_id TEXT NOT NULL,
                    instrument_id TEXT NOT NULL,
                    side TEXT NOT NULL,
                    limit_price REAL NOT NULL,
                    reduce_only INTEGER NOT NULL,
                    original_qty REAL NOT NULL,
                    remaining_qty REAL NOT NULL,
                    filled_qty REAL NOT NULL,
                    status TEXT NOT NULL,
                    submitted_at_ms INTEGER NOT NULL,
                    last_update_ms INTEGER NOT NULL,
                    reason TEXT,
                    strategy_tag TEXT NOT NULL,
                    quote_level_tag TEXT,
                    accounting_lane TEXT NOT NULL DEFAULT 'other'
                )",
                (),
            )
            .map_err(|error| {
                OrderStoreError::Sqlite(format!("failed to create orders table: {error}"))
            })?;

        self.add_column_if_missing("orders", "accounting_lane", "TEXT NOT NULL DEFAULT 'other'")?;

        self.connection
            .execute(
                "UPDATE orders
                 SET accounting_lane =
                    CASE
                        WHEN lower(coalesce(quote_level_tag, '')) LIKE 'paired-core:%'
                          OR lower(coalesce(quote_level_tag, '')) LIKE '%paired-mm%'
                          OR lower(coalesce(quote_level_tag, '')) LIKE '%mm-paired-bid%'
                            THEN 'paired_core'
                        WHEN lower(coalesce(quote_level_tag, '')) LIKE '%reversal-hedge%'
                            THEN 'reversal_hedge'
                        WHEN lower(coalesce(quote_level_tag, '')) LIKE 'bte-taker%'
                          OR lower(coalesce(quote_level_tag, '')) LIKE '%back_to_explore%'
                            THEN 'back_to_explore'
                        WHEN lower(coalesce(quote_level_tag, '')) LIKE 'cheap-tail%'
                          OR lower(coalesce(quote_level_tag, '')) LIKE '%convex%'
                            THEN 'cheap_tail'
                        WHEN lower(coalesce(quote_level_tag, '')) LIKE 'late-fav%'
                            THEN 'late_favorite'
                        ELSE 'other'
                    END
                 WHERE accounting_lane = 'other'",
                (),
            )
            .map_err(|error| {
                OrderStoreError::Sqlite(format!("failed to backfill accounting lane: {error}"))
            })?;

        self.connection
            .execute(
                "CREATE INDEX IF NOT EXISTS idx_orders_status_last_update ON orders (status, last_update_ms)",
                (),
            )
            .map_err(|error| {
                OrderStoreError::Sqlite(format!("failed to create status index: {error}"))
            })?;

        self.connection
            .execute(
                "CREATE INDEX IF NOT EXISTS idx_orders_market_status ON orders (market_id, status)",
                (),
            )
            .map_err(|error| {
                OrderStoreError::Sqlite(format!("failed to create market-status index: {error}"))
            })?;

        self.connection
            .execute(
                "CREATE TABLE IF NOT EXISTS strategy_state (
                    strategy_tag TEXT PRIMARY KEY,
                    run_id TEXT NOT NULL,
                    observed_at_ms INTEGER NOT NULL,
                    payload_json TEXT NOT NULL
                )",
                (),
            )
            .map_err(|error| {
                OrderStoreError::Sqlite(format!("failed to create strategy_state table: {error}"))
            })?;

        self.connection
            .execute(
                "CREATE TABLE IF NOT EXISTS runtime_state (
                    id INTEGER PRIMARY KEY CHECK (id = 1),
                    run_id TEXT NOT NULL,
                    observed_at_ms INTEGER NOT NULL,
                    runtime_status TEXT NOT NULL
                )",
                (),
            )
            .map_err(|error| {
                OrderStoreError::Sqlite(format!("failed to create runtime_state table: {error}"))
            })?;

        self.connection
            .execute(
                "CREATE TABLE IF NOT EXISTS router_decisions (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    run_id TEXT NOT NULL,
                    observed_at_ms INTEGER NOT NULL,
                    market_id TEXT NOT NULL,
                    cluster TEXT NOT NULL,
                    static_cluster_route TEXT NOT NULL,
                    raw_effective_router_route TEXT NOT NULL,
                    effective_router_route TEXT NOT NULL,
                    latched_router_route TEXT NOT NULL,
                    selected_router_route TEXT NOT NULL,
                    action_router_route TEXT NOT NULL,
                    locked_router_route TEXT NOT NULL,
                    shadow_vote_route TEXT NOT NULL,
                    router_enforce_enabled INTEGER NOT NULL,
                    session_guard_active INTEGER NOT NULL,
                    session_stress_fraction REAL NOT NULL,
                    session_observation_count INTEGER NOT NULL,
                    session_action_switch_count INTEGER NOT NULL,
                    session_risk_off_until_ms INTEGER,
                    br2_orders INTEGER NOT NULL,
                    bte_orders INTEGER NOT NULL,
                    br2_submit_intents INTEGER NOT NULL,
                    bte_submit_intents INTEGER NOT NULL,
                    yes_mid REAL NOT NULL,
                    market_yes_range_so_far REAL NOT NULL,
                    whipsaw_score REAL NOT NULL,
                    path_efficiency REAL NOT NULL,
                    sign_flip_rate REAL NOT NULL,
                    reversal_pressure REAL NOT NULL,
                    realized_vol_180s_bps REAL NOT NULL,
                    whipsaw_sample_count INTEGER NOT NULL,
                    btc_micro_regime TEXT,
                    free_cash_usd REAL NOT NULL,
                    gross_exposure_usd REAL NOT NULL,
                    open_orders INTEGER NOT NULL
                )",
                (),
            )
            .map_err(|error| {
                OrderStoreError::Sqlite(format!("failed to create router_decisions table: {error}"))
            })?;

        self.connection
            .execute(
                "CREATE INDEX IF NOT EXISTS idx_router_decisions_observed_at
                 ON router_decisions (observed_at_ms)",
                (),
            )
            .map_err(|error| {
                OrderStoreError::Sqlite(format!(
                    "failed to create router observed_at index: {error}"
                ))
            })?;

        self.connection
            .execute(
                "CREATE INDEX IF NOT EXISTS idx_router_decisions_market_observed_at
                 ON router_decisions (market_id, observed_at_ms)",
                (),
            )
            .map_err(|error| {
                OrderStoreError::Sqlite(format!(
                    "failed to create router market-observed_at index: {error}"
                ))
            })?;

        self.connection
            .execute(
                "CREATE INDEX IF NOT EXISTS idx_router_decisions_selected_route
                 ON router_decisions (selected_router_route, action_router_route, observed_at_ms)",
                (),
            )
            .map_err(|error| {
                OrderStoreError::Sqlite(format!(
                    "failed to create router selected-route index: {error}"
                ))
            })?;

        Ok(())
    }

    fn add_column_if_missing(
        &self,
        table: &str,
        column: &str,
        definition: &str,
    ) -> std::result::Result<(), OrderStoreError> {
        let mut statement = self
            .connection
            .prepare(&format!("PRAGMA table_info({table})"))
            .map_err(|error| {
                OrderStoreError::Sqlite(format!("failed to inspect {table} schema: {error}"))
            })?;
        let columns = statement
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(|error| {
                OrderStoreError::Sqlite(format!("failed to query {table} schema: {error}"))
            })?;
        for existing in columns {
            if existing.map_err(|error| {
                OrderStoreError::Sqlite(format!("failed to decode {table} schema: {error}"))
            })? == column
            {
                return Ok(());
            }
        }
        self.connection
            .execute(
                &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
                (),
            )
            .map_err(|error| {
                OrderStoreError::Sqlite(format!("failed to add {table}.{column} column: {error}"))
            })?;
        Ok(())
    }

    fn status_to_db(status: ManagedOrderStatus) -> &'static str {
        match status {
            ManagedOrderStatus::PendingSubmit => "PendingSubmit",
            ManagedOrderStatus::Submitted => "Submitted",
            ManagedOrderStatus::Working => "Working",
            ManagedOrderStatus::CancelRequested => "CancelRequested",
            ManagedOrderStatus::Filled => "Filled",
            ManagedOrderStatus::Cancelled => "Cancelled",
            ManagedOrderStatus::Rejected => "Rejected",
            ManagedOrderStatus::NeedsReconcile => "NeedsReconcile",
            ManagedOrderStatus::Quarantined => "Quarantined",
        }
    }

    fn status_from_db(raw: &str) -> std::result::Result<ManagedOrderStatus, OrderStoreError> {
        match raw {
            "PendingSubmit" => Ok(ManagedOrderStatus::PendingSubmit),
            "Submitted" => Ok(ManagedOrderStatus::Submitted),
            "Working" => Ok(ManagedOrderStatus::Working),
            "CancelRequested" => Ok(ManagedOrderStatus::CancelRequested),
            "Filled" => Ok(ManagedOrderStatus::Filled),
            "Cancelled" => Ok(ManagedOrderStatus::Cancelled),
            "Rejected" => Ok(ManagedOrderStatus::Rejected),
            "NeedsReconcile" => Ok(ManagedOrderStatus::NeedsReconcile),
            "Quarantined" => Ok(ManagedOrderStatus::Quarantined),
            value => Err(OrderStoreError::Serialization(format!(
                "invalid status `{value}`"
            ))),
        }
    }

    fn side_to_db(side: TradeSide) -> &'static str {
        match side {
            TradeSide::Buy => "Buy",
            TradeSide::Sell => "Sell",
        }
    }

    fn side_from_db(raw: &str) -> std::result::Result<TradeSide, OrderStoreError> {
        match raw {
            "Buy" => Ok(TradeSide::Buy),
            "Sell" => Ok(TradeSide::Sell),
            value => Err(OrderStoreError::Serialization(format!(
                "invalid side `{value}`"
            ))),
        }
    }

    fn sqlite_conversion_error(
        column_index: usize,
        column_type: Type,
        error: OrderStoreError,
    ) -> rusqlite::Error {
        rusqlite::Error::FromSqlConversionFailure(column_index, column_type, Box::new(error))
    }

    fn row_to_record(row: &Row<'_>) -> rusqlite::Result<OrderRecord> {
        let status_raw: String = row.get(11)?;
        let status = Self::status_from_db(&status_raw)
            .map_err(|error| Self::sqlite_conversion_error(11, Type::Text, error))?;
        let side_raw: String = row.get(5)?;
        let side = Self::side_from_db(&side_raw)
            .map_err(|error| Self::sqlite_conversion_error(5, Type::Text, error))?;
        let client_order_id = ClientOrderId::from(row.get::<_, String>(0)?.as_str());
        let venue_order_id = row
            .get::<_, Option<String>>(2)?
            .map(|value| OrderId::from(value.as_str()));
        let market_id = MarketId::from(row.get::<_, String>(3)?.as_str());
        let instrument_id = InstrumentId::from(row.get::<_, String>(4)?.as_str());
        let accounting_lane = row
            .get::<_, Option<String>>(17)?
            .as_deref()
            .map(AccountingLane::from_db)
            .transpose()
            .map_err(|error| Self::sqlite_conversion_error(17, Type::Text, error))?
            .unwrap_or_else(|| {
                AccountingLane::from_quote_level_tag(
                    row.get::<_, Option<String>>(16).ok().flatten().as_deref(),
                )
            });
        Ok(OrderRecord {
            run_id: row.get(1)?,
            client_order_id,
            venue_order_id,
            market_id,
            instrument_id,
            side,
            limit_price: row.get(6)?,
            reduce_only: row.get::<_, i64>(7)? != 0,
            original_qty: row.get(8)?,
            remaining_qty: row.get(9)?,
            filled_qty: row.get(10)?,
            status,
            submitted_at_ms: row.get::<_, i64>(12)? as u64,
            last_update_ms: row.get::<_, i64>(13)? as u64,
            reason: row.get(14)?,
            strategy_tag: row.get(15)?,
            quote_level_tag: row.get(16)?,
            accounting_lane,
        })
    }

    fn row_to_router_decision(row: &Row<'_>) -> rusqlite::Result<RouterDecisionRecord> {
        Ok(RouterDecisionRecord {
            run_id: row.get(0)?,
            observed_at_ms: row.get::<_, i64>(1)? as u64,
            market_id: MarketId::from(row.get::<_, String>(2)?.as_str()),
            cluster: row.get(3)?,
            static_cluster_route: row.get(4)?,
            raw_effective_router_route: row.get(5)?,
            effective_router_route: row.get(6)?,
            latched_router_route: row.get(7)?,
            selected_router_route: row.get(8)?,
            action_router_route: row.get(9)?,
            locked_router_route: row.get(10)?,
            shadow_vote_route: row.get(11)?,
            router_enforce_enabled: row.get::<_, i64>(12)? != 0,
            session_guard_active: row.get::<_, i64>(13)? != 0,
            session_stress_fraction: row.get(14)?,
            session_observation_count: row.get::<_, i64>(15)? as u64,
            session_action_switch_count: row.get::<_, i64>(16)? as u64,
            session_risk_off_until_ms: row.get::<_, Option<i64>>(17)?.map(|value| value as u64),
            br2_orders: row.get::<_, i64>(18)? as u64,
            bte_orders: row.get::<_, i64>(19)? as u64,
            br2_submit_intents: row.get::<_, i64>(20)? as u64,
            bte_submit_intents: row.get::<_, i64>(21)? as u64,
            yes_mid: row.get(22)?,
            market_yes_range_so_far: row.get(23)?,
            whipsaw_score: row.get(24)?,
            path_efficiency: row.get(25)?,
            sign_flip_rate: row.get(26)?,
            reversal_pressure: row.get(27)?,
            realized_vol_180s_bps: row.get(28)?,
            whipsaw_sample_count: row.get::<_, i64>(29)? as u64,
            btc_micro_regime: row.get(30)?,
            free_cash_usd: row.get(31)?,
            gross_exposure_usd: row.get(32)?,
            open_orders: row.get::<_, i64>(33)? as u64,
        })
    }

    fn current_status(
        &self,
        client_order_id: &ClientOrderId,
    ) -> std::result::Result<ManagedOrderStatus, OrderStoreError> {
        self.connection
            .query_row(
                "SELECT status FROM orders WHERE client_order_id = ?1",
                params![client_order_id.as_str()],
                |row| {
                    let raw: String = row.get(0)?;
                    Self::status_from_db(&raw)
                        .map_err(|error| Self::sqlite_conversion_error(0, Type::Text, error))
                },
            )
            .optional()
            .map_err(|error| {
                OrderStoreError::Sqlite(format!("failed to load order status: {error}"))
            })?
            .ok_or_else(|| OrderStoreError::NotFound(client_order_id.clone()))
    }

    fn list_open_query(include_terminal: bool) -> String {
        if include_terminal {
            "SELECT * FROM orders WHERE remaining_qty > 0.0 ORDER BY last_update_ms ASC".to_string()
        } else {
            "SELECT * FROM orders
             WHERE status IN ('PendingSubmit', 'Submitted', 'Working', 'CancelRequested', 'NeedsReconcile')
               AND remaining_qty > 0.0
             ORDER BY last_update_ms ASC"
                .to_string()
        }
    }
}

impl OrderStore for SqliteOrderStore {
    fn insert(&mut self, record: OrderRecord) -> std::result::Result<(), OrderStoreError> {
        let inserted = self
            .connection
            .execute(
                "INSERT INTO orders (
                    client_order_id,
                    run_id,
                    venue_order_id,
                    market_id,
                    instrument_id,
                    side,
                    limit_price,
                    reduce_only,
                    original_qty,
                    remaining_qty,
                    filled_qty,
                    status,
                    submitted_at_ms,
                    last_update_ms,
                    reason,
                    strategy_tag,
                    quote_level_tag,
                    accounting_lane
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
                params![
                    record.client_order_id.as_str(),
                    record.run_id,
                    record.venue_order_id.as_ref().map(ToString::to_string),
                    record.market_id.as_str(),
                    record.instrument_id.as_str(),
                    Self::side_to_db(record.side),
                    record.limit_price,
                    if record.reduce_only { 1 } else { 0 },
                    record.original_qty,
                    record.remaining_qty,
                    record.filled_qty,
                    Self::status_to_db(record.status),
                    record.submitted_at_ms,
                    record.last_update_ms,
                    record.reason,
                    record.strategy_tag,
                    record.quote_level_tag,
                    record.accounting_lane.as_str(),
                ],
            )
            .map_err(|error| {
                if error.sqlite_error_code()
                    .map(|code| code == rusqlite::ErrorCode::ConstraintViolation)
                    .unwrap_or(false)
                {
                    return OrderStoreError::Conflict(format!("order {} already exists", record.client_order_id));
                }
                OrderStoreError::Sqlite(format!("failed to insert order record: {error}"))
            })?;

        if inserted == 0 {
            return Err(OrderStoreError::Conflict(format!(
                "order {} already exists",
                record.client_order_id
            )));
        }
        Ok(())
    }

    fn update_status(
        &mut self,
        client_order_id: &ClientOrderId,
        status: ManagedOrderStatus,
        updated_at_ms: EpochMillis,
    ) -> std::result::Result<(), OrderStoreError> {
        let current_status = self.current_status(client_order_id)?;
        if !current_status.can_transition_to(status) {
            return Err(OrderStoreError::Conflict(format!(
                "invalid status transition for {}: {:?} -> {:?}",
                client_order_id, current_status, status
            )));
        }
        let updated = self
            .connection
            .execute(
                "UPDATE orders
                 SET status = ?1, last_update_ms = ?2
                 WHERE client_order_id = ?3",
                params![
                    Self::status_to_db(status),
                    updated_at_ms,
                    client_order_id.as_str()
                ],
            )
            .map_err(|error| {
                OrderStoreError::Sqlite(format!("failed to update status: {error}"))
            })?;

        if updated == 0 {
            return Err(OrderStoreError::NotFound(client_order_id.clone()));
        }

        Ok(())
    }

    fn attach_venue_id(
        &mut self,
        client_order_id: &ClientOrderId,
        venue_order_id: OrderId,
        updated_at_ms: EpochMillis,
    ) -> std::result::Result<(), OrderStoreError> {
        let current_status = self.current_status(client_order_id)?;
        let next_status = match current_status {
            ManagedOrderStatus::PendingSubmit | ManagedOrderStatus::Submitted => {
                ManagedOrderStatus::Submitted
            }
            ManagedOrderStatus::Working | ManagedOrderStatus::CancelRequested => current_status,
            ManagedOrderStatus::NeedsReconcile => ManagedOrderStatus::Working,
            ManagedOrderStatus::Filled
            | ManagedOrderStatus::Cancelled
            | ManagedOrderStatus::Rejected
            | ManagedOrderStatus::Quarantined => {
                return Err(OrderStoreError::Conflict(format!(
                    "invalid venue attachment for terminal order {}: {:?}",
                    client_order_id, current_status
                )));
            }
        };
        if !current_status.can_transition_to(next_status) {
            return Err(OrderStoreError::Conflict(format!(
                "invalid venue attachment transition for {}: {:?} -> {:?}",
                client_order_id, current_status, next_status
            )));
        }
        let updated = self
            .connection
            .execute(
                "UPDATE orders
                 SET venue_order_id = ?1, status = ?2, last_update_ms = ?3
                 WHERE client_order_id = ?4",
                params![
                    venue_order_id.as_str(),
                    Self::status_to_db(next_status),
                    updated_at_ms,
                    client_order_id.as_str(),
                ],
            )
            .map_err(|error| {
                OrderStoreError::Sqlite(format!("failed to attach venue order id: {error}"))
            })?;

        if updated == 0 {
            return Err(OrderStoreError::NotFound(client_order_id.clone()));
        }

        Ok(())
    }

    fn apply_fill(
        &mut self,
        client_order_id: &ClientOrderId,
        fill_qty: f64,
        updated_at_ms: EpochMillis,
    ) -> std::result::Result<(), OrderStoreError> {
        if fill_qty <= 0.0 {
            return Ok(());
        }

        let record = self
            .get(client_order_id)?
            .ok_or_else(|| OrderStoreError::NotFound(client_order_id.clone()))?;

        let mut remaining_qty = (record.remaining_qty - fill_qty).max(0.0);
        let mut filled_qty = record.filled_qty + fill_qty.min(record.remaining_qty);

        let remaining_notional = remaining_qty * record.limit_price;
        if remaining_qty <= 1e-9
            || remaining_qty <= DUST_REMAINING_QTY
            || remaining_notional <= DUST_REMAINING_NOTIONAL_USD
        {
            remaining_qty = 0.0;
            filled_qty = record.original_qty;
        }
        let status = if remaining_qty <= 1e-9 {
            ManagedOrderStatus::Filled
        } else if matches!(record.status, ManagedOrderStatus::PendingSubmit) {
            ManagedOrderStatus::Submitted
        } else if matches!(
            record.status,
            ManagedOrderStatus::Working | ManagedOrderStatus::NeedsReconcile
        ) {
            ManagedOrderStatus::Working
        } else if matches!(record.status, ManagedOrderStatus::Quarantined) {
            ManagedOrderStatus::Quarantined
        } else {
            record.status
        };

        let terminal_fill_correction = matches!(
            (record.status, status),
            (ManagedOrderStatus::Cancelled, ManagedOrderStatus::Filled)
                | (ManagedOrderStatus::Rejected, ManagedOrderStatus::Filled)
                | (ManagedOrderStatus::Quarantined, ManagedOrderStatus::Filled)
        );
        if !terminal_fill_correction && !record.status.can_transition_to(status) {
            return Err(OrderStoreError::Conflict(format!(
                "invalid fill transition for {}: {:?} -> {:?}",
                client_order_id, record.status, status
            )));
        }

        let updated = self
            .connection
            .execute(
                "UPDATE orders
                 SET filled_qty = ?1,
                     remaining_qty = ?2,
                     status = ?3,
                     last_update_ms = ?4
                 WHERE client_order_id = ?5",
                params![
                    filled_qty,
                    remaining_qty,
                    Self::status_to_db(status),
                    updated_at_ms,
                    client_order_id.as_str()
                ],
            )
            .map_err(|error| OrderStoreError::Sqlite(format!("failed to apply fill: {error}")))?;

        if updated == 0 {
            return Err(OrderStoreError::NotFound(client_order_id.clone()));
        }

        Ok(())
    }

    fn expire_remaining_after_partial_fill(
        &mut self,
        client_order_id: &ClientOrderId,
        updated_at_ms: EpochMillis,
    ) -> std::result::Result<(), OrderStoreError> {
        let record = self
            .get(client_order_id)?
            .ok_or_else(|| OrderStoreError::NotFound(client_order_id.clone()))?;
        if record.filled_qty <= 0.0 {
            return Err(OrderStoreError::Conflict(format!(
                "cannot expire unfilled order {} as partial fill",
                client_order_id
            )));
        }
        if record.status.is_terminal() {
            return Ok(());
        }
        if !record.status.can_transition_to(ManagedOrderStatus::Filled) {
            return Err(OrderStoreError::Conflict(format!(
                "invalid partial-fill expiry transition for {}: {:?} -> Filled",
                client_order_id, record.status
            )));
        }
        let updated = self
            .connection
            .execute(
                "UPDATE orders
                 SET remaining_qty = 0.0,
                     status = ?1,
                     last_update_ms = ?2
                 WHERE client_order_id = ?3",
                params![
                    Self::status_to_db(ManagedOrderStatus::Filled),
                    updated_at_ms,
                    client_order_id.as_str()
                ],
            )
            .map_err(|error| {
                OrderStoreError::Sqlite(format!("failed to expire partial-fill remainder: {error}"))
            })?;

        if updated == 0 {
            return Err(OrderStoreError::NotFound(client_order_id.clone()));
        }

        Ok(())
    }

    fn get(
        &self,
        client_order_id: &ClientOrderId,
    ) -> std::result::Result<Option<OrderRecord>, OrderStoreError> {
        self.connection
            .query_row(
                "SELECT * FROM orders WHERE client_order_id = ?1",
                params![client_order_id.as_str()],
                |row| Self::row_to_record(row),
            )
            .optional()
            .map_err(|error| OrderStoreError::Sqlite(format!("failed to load order: {error}")))
    }

    fn list_open(&self) -> std::result::Result<Vec<OrderRecord>, OrderStoreError> {
        let query = Self::list_open_query(false);
        let mut statement = self.connection.prepare(&query).map_err(|error| {
            OrderStoreError::Sqlite(format!("failed to list open orders: {error}"))
        })?;

        let rows = statement
            .query_map(params![], |row| Self::row_to_record(row))
            .map_err(|error| {
                OrderStoreError::Sqlite(format!("failed to execute list open query: {error}"))
            })?;

        let mut records = Vec::new();
        for row in rows {
            records.push(row.map_err(|error| {
                OrderStoreError::Sqlite(format!("failed to decode order row: {error}"))
            })?);
        }
        Ok(records)
    }

    fn list_by_market(
        &self,
        market_id: &MarketId,
    ) -> std::result::Result<Vec<OrderRecord>, OrderStoreError> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT * FROM orders
                 WHERE market_id = ?1
                 ORDER BY last_update_ms ASC",
            )
            .map_err(|error| {
                OrderStoreError::Sqlite(format!("failed to list by market query: {error}"))
            })?;

        let rows = statement
            .query_map(params![market_id.as_str()], |row| Self::row_to_record(row))
            .map_err(|error| {
                OrderStoreError::Sqlite(format!("failed to execute market list query: {error}"))
            })?;

        let mut records = Vec::new();
        for row in rows {
            records.push(row.map_err(|error| {
                OrderStoreError::Sqlite(format!("failed to decode order row: {error}"))
            })?);
        }
        Ok(records)
    }

    fn filled_buy_cost_basis(
        &self,
        market_id: &MarketId,
        instrument_id: &InstrumentId,
    ) -> std::result::Result<Option<f64>, OrderStoreError> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT SUM(filled_qty * limit_price), SUM(filled_qty)
                 FROM orders
                 WHERE market_id = ?1
                   AND instrument_id = ?2
                   AND side = 'Buy'
                   AND reduce_only = 0
                   AND filled_qty > 0.0",
            )
            .map_err(|error| {
                OrderStoreError::Sqlite(format!("failed to prepare cost-basis query: {error}"))
            })?;

        let (notional, quantity): (Option<f64>, Option<f64>) = statement
            .query_row(params![market_id.as_str(), instrument_id.as_str()], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .map_err(|error| {
                OrderStoreError::Sqlite(format!("failed to query filled buy cost basis: {error}"))
            })?;

        let (Some(notional), Some(quantity)) = (notional, quantity) else {
            return Ok(None);
        };
        if !notional.is_finite() || !quantity.is_finite() || quantity <= 0.0 {
            return Ok(None);
        }
        Ok(Some((notional / quantity).max(0.0)))
    }

    fn put_strategy_state(
        &mut self,
        run_id: &str,
        strategy_tag: &str,
        observed_at_ms: EpochMillis,
        payload: &serde_json::Value,
    ) -> std::result::Result<(), OrderStoreError> {
        let payload_json = serde_json::to_string(payload).map_err(|error| {
            OrderStoreError::Serialization(format!("failed to encode strategy state: {error}"))
        })?;
        self.connection
            .execute(
                "INSERT INTO strategy_state (
                    strategy_tag,
                    run_id,
                    observed_at_ms,
                    payload_json
                ) VALUES (?1, ?2, ?3, ?4)
                ON CONFLICT(strategy_tag) DO UPDATE SET
                    run_id = excluded.run_id,
                    observed_at_ms = excluded.observed_at_ms,
                    payload_json = excluded.payload_json",
                params![strategy_tag, run_id, observed_at_ms as i64, payload_json],
            )
            .map_err(|error| {
                OrderStoreError::Sqlite(format!("failed to upsert strategy state: {error}"))
            })?;
        Ok(())
    }

    fn latest_strategy_state(
        &self,
        strategy_tag: &str,
    ) -> std::result::Result<Option<serde_json::Value>, OrderStoreError> {
        let raw = self
            .connection
            .query_row(
                "SELECT payload_json FROM strategy_state WHERE strategy_tag = ?1",
                params![strategy_tag],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| {
                OrderStoreError::Sqlite(format!("failed to load strategy state: {error}"))
            })?;
        raw.map(|payload_json| {
            serde_json::from_str(&payload_json).map_err(|error| {
                OrderStoreError::Serialization(format!("failed to decode strategy state: {error}"))
            })
        })
        .transpose()
    }

    fn put_runtime_status(
        &mut self,
        run_id: &str,
        observed_at_ms: EpochMillis,
        status: RuntimeStatus,
    ) -> std::result::Result<(), OrderStoreError> {
        let status_json = serde_json::to_string(&status).map_err(|error| {
            OrderStoreError::Serialization(format!("failed to encode runtime status: {error}"))
        })?;
        self.connection
            .execute(
                "INSERT INTO runtime_state (
                    id,
                    run_id,
                    observed_at_ms,
                    runtime_status
                ) VALUES (1, ?1, ?2, ?3)
                ON CONFLICT(id) DO UPDATE SET
                    run_id = excluded.run_id,
                    observed_at_ms = excluded.observed_at_ms,
                    runtime_status = excluded.runtime_status",
                params![run_id, observed_at_ms as i64, status_json],
            )
            .map_err(|error| {
                OrderStoreError::Sqlite(format!("failed to upsert runtime status: {error}"))
            })?;
        Ok(())
    }

    fn latest_runtime_status(&self) -> std::result::Result<Option<RuntimeStatus>, OrderStoreError> {
        let raw = self
            .connection
            .query_row(
                "SELECT runtime_status FROM runtime_state WHERE id = 1",
                (),
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| {
                OrderStoreError::Sqlite(format!("failed to load runtime status: {error}"))
            })?;
        raw.map(|status_json| {
            serde_json::from_str(&status_json).map_err(|error| {
                OrderStoreError::Serialization(format!("failed to decode runtime status: {error}"))
            })
        })
        .transpose()
    }

    fn insert_router_decision(
        &mut self,
        record: RouterDecisionRecord,
    ) -> std::result::Result<(), OrderStoreError> {
        self.connection
            .execute(
                "INSERT INTO router_decisions (
                    run_id,
                    observed_at_ms,
                    market_id,
                    cluster,
                    static_cluster_route,
                    raw_effective_router_route,
                    effective_router_route,
                    latched_router_route,
                    selected_router_route,
                    action_router_route,
                    locked_router_route,
                    shadow_vote_route,
                    router_enforce_enabled,
                    session_guard_active,
                    session_stress_fraction,
                    session_observation_count,
                    session_action_switch_count,
                    session_risk_off_until_ms,
                    br2_orders,
                    bte_orders,
                    br2_submit_intents,
                    bte_submit_intents,
                    yes_mid,
                    market_yes_range_so_far,
                    whipsaw_score,
                    path_efficiency,
                    sign_flip_rate,
                    reversal_pressure,
                    realized_vol_180s_bps,
                    whipsaw_sample_count,
                    btc_micro_regime,
                    free_cash_usd,
                    gross_exposure_usd,
                    open_orders
                ) VALUES (
                    ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
                    ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18,
                    ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26,
                    ?27, ?28, ?29, ?30, ?31, ?32, ?33, ?34
                )",
                params![
                    record.run_id,
                    record.observed_at_ms as i64,
                    record.market_id.as_str(),
                    record.cluster,
                    record.static_cluster_route,
                    record.raw_effective_router_route,
                    record.effective_router_route,
                    record.latched_router_route,
                    record.selected_router_route,
                    record.action_router_route,
                    record.locked_router_route,
                    record.shadow_vote_route,
                    if record.router_enforce_enabled { 1 } else { 0 },
                    if record.session_guard_active { 1 } else { 0 },
                    record.session_stress_fraction,
                    record.session_observation_count as i64,
                    record.session_action_switch_count as i64,
                    record.session_risk_off_until_ms.map(|value| value as i64),
                    record.br2_orders as i64,
                    record.bte_orders as i64,
                    record.br2_submit_intents as i64,
                    record.bte_submit_intents as i64,
                    record.yes_mid,
                    record.market_yes_range_so_far,
                    record.whipsaw_score,
                    record.path_efficiency,
                    record.sign_flip_rate,
                    record.reversal_pressure,
                    record.realized_vol_180s_bps,
                    record.whipsaw_sample_count as i64,
                    record.btc_micro_regime,
                    record.free_cash_usd,
                    record.gross_exposure_usd,
                    record.open_orders as i64,
                ],
            )
            .map_err(|error| {
                OrderStoreError::Sqlite(format!("failed to insert router decision: {error}"))
            })?;
        Ok(())
    }

    fn list_recent_router_decisions(
        &self,
        limit: usize,
    ) -> std::result::Result<Vec<RouterDecisionRecord>, OrderStoreError> {
        let limit = limit.max(1).min(i64::MAX as usize) as i64;
        let mut statement = self
            .connection
            .prepare(
                "SELECT
                    run_id,
                    observed_at_ms,
                    market_id,
                    cluster,
                    static_cluster_route,
                    raw_effective_router_route,
                    effective_router_route,
                    latched_router_route,
                    selected_router_route,
                    action_router_route,
                    locked_router_route,
                    shadow_vote_route,
                    router_enforce_enabled,
                    session_guard_active,
                    session_stress_fraction,
                    session_observation_count,
                    session_action_switch_count,
                    session_risk_off_until_ms,
                    br2_orders,
                    bte_orders,
                    br2_submit_intents,
                    bte_submit_intents,
                    yes_mid,
                    market_yes_range_so_far,
                    whipsaw_score,
                    path_efficiency,
                    sign_flip_rate,
                    reversal_pressure,
                    realized_vol_180s_bps,
                    whipsaw_sample_count,
                    btc_micro_regime,
                    free_cash_usd,
                    gross_exposure_usd,
                    open_orders
                 FROM router_decisions
                 ORDER BY observed_at_ms DESC, id DESC
                 LIMIT ?1",
            )
            .map_err(|error| {
                OrderStoreError::Sqlite(format!(
                    "failed to prepare recent router decisions query: {error}"
                ))
            })?;
        let rows = statement
            .query_map(params![limit], Self::row_to_router_decision)
            .map_err(|error| {
                OrderStoreError::Sqlite(format!("failed to query recent router decisions: {error}"))
            })?;
        let mut records = Vec::new();
        for row in rows {
            records.push(row.map_err(|error| {
                OrderStoreError::Sqlite(format!("failed to decode router decision: {error}"))
            })?);
        }
        Ok(records)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AccountingLane, OrderStore, OrderStoreError, RouterDecisionRecord, SqliteOrderStore,
    };
    use crate::types::{
        ClientOrderId, EpochMillis, InstrumentId, MarketId, OrderId, OrderIntent, RuntimeStatus,
        TradeSide,
    };

    use std::env;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::runtime::types::ManagedOrderStatus;

    #[test]
    fn accounting_lane_classifies_bte_as_directional() {
        let lane =
            AccountingLane::from_quote_level_tag(Some("bte-taker:back_to_explore_range_repair"));
        assert_eq!(lane, AccountingLane::BackToExplore);
        assert!(lane.is_directional());
        assert_eq!(lane.as_str(), "back_to_explore");
    }

    #[test]
    fn store_insert_update_fill_cycle() -> anyhow::Result<()> {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = env::temp_dir().join(format!("polymarket-exec-order-store-{ts}.sqlite"));
        let mut store = SqliteOrderStore::open(path)?;
        let now: EpochMillis = 1;

        let record = crate::runtime::order_store::OrderRecord::from_intent(
            "run-1",
            &OrderIntent {
                client_order_id: ClientOrderId::from("coid-1"),
                market_id: MarketId::from("mkt-1"),
                instrument_id: InstrumentId::from("inst-1"),
                side: TradeSide::Buy,
                limit_price: 1.25,
                quantity: 4.0,
                reduce_only: false,
                reason: "test".to_string(),
                quote_level_tag: None,
                created_at_ms: now,
                pair_id: None,
                kind: crate::types::IntentKind::Entry,
            },
            "strat",
        );

        store.insert(record)?;
        store.update_status(
            &ClientOrderId::from("coid-1"),
            ManagedOrderStatus::Submitted,
            now + 1,
        )?;
        store.apply_fill(&ClientOrderId::from("coid-1"), 1.5, now + 2)?;
        let row = store.get(&ClientOrderId::from("coid-1"))?.expect("row");
        assert_eq!(row.filled_qty, 1.5);
        assert_eq!(row.status, ManagedOrderStatus::Submitted);

        let all = store.list_open()?;
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].client_order_id.as_str(), "coid-1");

        store.attach_venue_id(
            &ClientOrderId::from("coid-1"),
            OrderId::from("venue-1"),
            now + 3,
        )?;
        let with_venue = store.get(&ClientOrderId::from("coid-1"))?.expect("row");
        assert_eq!(with_venue.venue_order_id, Some(OrderId::from("venue-1")));
        Ok(())
    }

    #[test]
    fn attach_venue_id_preserves_working_state() -> anyhow::Result<()> {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = env::temp_dir().join(format!(
            "polymarket-exec-order-store-working-attach-{ts}.sqlite"
        ));
        let mut store = SqliteOrderStore::open(path)?;
        let now: EpochMillis = 1;
        let client_order_id = ClientOrderId::from("coid-working");

        store.insert(crate::runtime::order_store::OrderRecord::from_intent(
            "run-1",
            &OrderIntent {
                client_order_id: client_order_id.clone(),
                market_id: MarketId::from("mkt-1"),
                instrument_id: InstrumentId::from("inst-1"),
                side: TradeSide::Buy,
                limit_price: 0.72,
                quantity: 5.0,
                reduce_only: false,
                reason: "test".to_string(),
                quote_level_tag: None,
                created_at_ms: now,
                pair_id: None,
                kind: crate::types::IntentKind::Entry,
            },
            "strat",
        ))?;
        store.update_status(&client_order_id, ManagedOrderStatus::Working, now + 1)?;

        store.attach_venue_id(&client_order_id, OrderId::from("venue-working"), now + 2)?;

        let row = store.get(&client_order_id)?.expect("row");
        assert_eq!(row.venue_order_id, Some(OrderId::from("venue-working")));
        assert_eq!(row.status, ManagedOrderStatus::Working);
        Ok(())
    }

    #[test]
    fn attach_venue_id_recovers_needs_reconcile_to_working() -> anyhow::Result<()> {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = env::temp_dir().join(format!(
            "polymarket-exec-order-store-needs-reconcile-attach-{ts}.sqlite"
        ));
        let mut store = SqliteOrderStore::open(path)?;
        let now: EpochMillis = 1;
        let client_order_id = ClientOrderId::from("coid-reconcile");

        store.insert(crate::runtime::order_store::OrderRecord::from_intent(
            "run-1",
            &OrderIntent {
                client_order_id: client_order_id.clone(),
                market_id: MarketId::from("mkt-1"),
                instrument_id: InstrumentId::from("inst-1"),
                side: TradeSide::Buy,
                limit_price: 0.72,
                quantity: 5.0,
                reduce_only: false,
                reason: "test".to_string(),
                quote_level_tag: None,
                created_at_ms: now,
                pair_id: None,
                kind: crate::types::IntentKind::Entry,
            },
            "strat",
        ))?;
        store.update_status(
            &client_order_id,
            ManagedOrderStatus::NeedsReconcile,
            now + 1,
        )?;

        store.attach_venue_id(&client_order_id, OrderId::from("venue-recovered"), now + 2)?;

        let row = store.get(&client_order_id)?.expect("row");
        assert_eq!(row.venue_order_id, Some(OrderId::from("venue-recovered")));
        assert_eq!(row.status, ManagedOrderStatus::Working);
        Ok(())
    }

    #[test]
    fn apply_fill_treats_sub_cent_remainder_as_filled() -> anyhow::Result<()> {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path =
            env::temp_dir().join(format!("polymarket-exec-order-store-dust-fill-{ts}.sqlite"));
        let mut store = SqliteOrderStore::open(path)?;
        let now: EpochMillis = 1;
        let client_order_id = ClientOrderId::from("coid-dust");

        store.insert(crate::runtime::order_store::OrderRecord::from_intent(
            "run-1",
            &OrderIntent {
                client_order_id: client_order_id.clone(),
                market_id: MarketId::from("mkt-1"),
                instrument_id: InstrumentId::from("inst-1"),
                side: TradeSide::Buy,
                limit_price: 0.73,
                quantity: 5.0,
                reduce_only: false,
                reason: "test".to_string(),
                quote_level_tag: None,
                created_at_ms: now,
                pair_id: None,
                kind: crate::types::IntentKind::Entry,
            },
            "strat",
        ))?;
        store.update_status(&client_order_id, ManagedOrderStatus::Working, now + 1)?;

        store.apply_fill(&client_order_id, 4.990369, now + 2)?;

        let row = store.get(&client_order_id)?.expect("row");
        assert_eq!(row.status, ManagedOrderStatus::Filled);
        assert_eq!(row.remaining_qty, 0.0);
        assert_eq!(row.filled_qty, 5.0);
        Ok(())
    }

    #[test]
    fn apply_fill_corrects_terminal_cancel_to_filled() -> anyhow::Result<()> {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path =
            env::temp_dir().join(format!("polymarket-exec-order-store-late-fill-{ts}.sqlite"));
        let mut store = SqliteOrderStore::open(path)?;
        let now: EpochMillis = 1;
        let client_order_id = ClientOrderId::from("coid-late-fill");

        store.insert(crate::runtime::order_store::OrderRecord::from_intent(
            "run-1",
            &OrderIntent {
                client_order_id: client_order_id.clone(),
                market_id: MarketId::from("mkt-1"),
                instrument_id: InstrumentId::from("inst-1"),
                side: TradeSide::Buy,
                limit_price: 0.80,
                quantity: 6.5,
                reduce_only: false,
                reason: "test".to_string(),
                quote_level_tag: None,
                created_at_ms: now,
                pair_id: None,
                kind: crate::types::IntentKind::Entry,
            },
            "strat",
        ))?;
        store.update_status(&client_order_id, ManagedOrderStatus::Working, now + 1)?;
        store.update_status(&client_order_id, ManagedOrderStatus::Cancelled, now + 2)?;

        store.apply_fill(&client_order_id, 6.5, now + 3)?;

        let row = store.get(&client_order_id)?.expect("row");
        assert_eq!(row.status, ManagedOrderStatus::Filled);
        assert_eq!(row.remaining_qty, 0.0);
        assert_eq!(row.filled_qty, 6.5);
        Ok(())
    }

    #[test]
    fn apply_fill_corrects_terminal_quarantine_to_filled() -> anyhow::Result<()> {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = env::temp_dir().join(format!(
            "polymarket-exec-order-store-quarantine-fill-{ts}.sqlite"
        ));
        let mut store = SqliteOrderStore::open(path)?;
        let now: EpochMillis = 1;
        let client_order_id = ClientOrderId::from("coid-quarantine-fill");

        store.insert(crate::runtime::order_store::OrderRecord::from_intent(
            "run-1",
            &OrderIntent {
                client_order_id: client_order_id.clone(),
                market_id: MarketId::from("mkt-1"),
                instrument_id: InstrumentId::from("inst-1"),
                side: TradeSide::Buy,
                limit_price: 0.44,
                quantity: 5.0,
                reduce_only: false,
                reason: "test".to_string(),
                quote_level_tag: Some("paired-core:ladder:0".to_string()),
                created_at_ms: now,
                pair_id: None,
                kind: crate::types::IntentKind::Entry,
            },
            "strat",
        ))?;
        store.update_status(&client_order_id, ManagedOrderStatus::Working, now + 1)?;
        store.update_status(&client_order_id, ManagedOrderStatus::Quarantined, now + 2)?;

        store.apply_fill(&client_order_id, 5.0, now + 3)?;

        let row = store.get(&client_order_id)?.expect("row");
        assert_eq!(row.accounting_lane, AccountingLane::PairedCore);
        assert_eq!(row.status, ManagedOrderStatus::Filled);
        assert_eq!(row.remaining_qty, 0.0);
        assert_eq!(row.filled_qty, 5.0);
        Ok(())
    }

    #[test]
    fn duplicate_insert_is_conflict() -> anyhow::Result<()> {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path =
            env::temp_dir().join(format!("polymarket-exec-order-store-conflict-{ts}.sqlite"));
        let mut store = SqliteOrderStore::open(path)?;
        let now: EpochMillis = 10;
        let intent = OrderIntent {
            client_order_id: ClientOrderId::from("coid-2"),
            market_id: MarketId::from("mkt-1"),
            instrument_id: InstrumentId::from("inst-1"),
            side: TradeSide::Sell,
            limit_price: 2.0,
            quantity: 1.0,
            reduce_only: false,
            reason: "dup".to_string(),
            quote_level_tag: None,
            created_at_ms: now,
            pair_id: None,
            kind: crate::types::IntentKind::Entry,
        };
        let record = crate::runtime::order_store::OrderRecord::from_intent("run", &intent, "strat");
        store.insert(record.clone())?;
        let result = store.insert(record);
        assert!(matches!(result, Err(OrderStoreError::Conflict(_))));
        Ok(())
    }

    #[test]
    fn strategy_state_is_upserted_and_loaded() -> anyhow::Result<()> {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = env::temp_dir().join(format!("polymarket-exec-strategy-state-{ts}.sqlite"));
        let mut store = SqliteOrderStore::open(path)?;

        assert!(store.latest_strategy_state("paired_mm")?.is_none());

        let first = serde_json::json!({
            "version": 1,
            "market_states": [{"market_id": "market-a"}],
        });
        store.put_strategy_state("run-1", "paired_mm", 10, &first)?;
        assert_eq!(store.latest_strategy_state("paired_mm")?, Some(first));

        let second = serde_json::json!({
            "version": 1,
            "market_states": [{"market_id": "market-b"}],
        });
        store.put_strategy_state("run-2", "paired_mm", 20, &second)?;
        assert_eq!(store.latest_strategy_state("paired_mm")?, Some(second));
        assert!(store.latest_strategy_state("other")?.is_none());
        Ok(())
    }

    #[test]
    fn runtime_status_is_upserted_and_loaded() -> anyhow::Result<()> {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = env::temp_dir().join(format!("polymarket-exec-runtime-state-{ts}.sqlite"));
        let mut store = SqliteOrderStore::open(path)?;

        assert_eq!(store.latest_runtime_status()?, None);

        store.put_runtime_status("run-1", 10, RuntimeStatus::RiskOff)?;
        assert_eq!(store.latest_runtime_status()?, Some(RuntimeStatus::RiskOff));

        store.put_runtime_status("run-2", 20, RuntimeStatus::Running)?;
        assert_eq!(store.latest_runtime_status()?, Some(RuntimeStatus::Running));
        Ok(())
    }

    #[test]
    fn router_decision_records_are_inserted_and_loaded() -> anyhow::Result<()> {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = env::temp_dir().join(format!("polymarket-exec-router-decisions-{ts}.sqlite"));
        let mut store = SqliteOrderStore::open(path)?;

        store.insert_router_decision(RouterDecisionRecord {
            run_id: "run-1".to_string(),
            observed_at_ms: 123,
            market_id: MarketId::from("market-1"),
            cluster: "low_efficiency_nonreversal".to_string(),
            static_cluster_route: "bte".to_string(),
            raw_effective_router_route: "bte".to_string(),
            effective_router_route: "risk_off".to_string(),
            latched_router_route: "risk_off".to_string(),
            selected_router_route: "risk_off".to_string(),
            action_router_route: "risk_off".to_string(),
            locked_router_route: "none".to_string(),
            shadow_vote_route: "bte".to_string(),
            router_enforce_enabled: true,
            session_guard_active: true,
            session_stress_fraction: 0.71,
            session_observation_count: 1706,
            session_action_switch_count: 200,
            session_risk_off_until_ms: Some(456),
            br2_orders: 0,
            bte_orders: 1,
            br2_submit_intents: 0,
            bte_submit_intents: 1,
            yes_mid: 0.43,
            market_yes_range_so_far: 0.43,
            whipsaw_score: 0.61,
            path_efficiency: 0.03,
            sign_flip_rate: 0.37,
            reversal_pressure: 0.22,
            realized_vol_180s_bps: 10.33,
            whipsaw_sample_count: 37,
            btc_micro_regime: Some("whipsaw".to_string()),
            free_cash_usd: 2310.73,
            gross_exposure_usd: 0.0,
            open_orders: 0,
        })?;

        let records = store.list_recent_router_decisions(10)?;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].market_id, MarketId::from("market-1"));
        assert_eq!(records[0].selected_router_route, "risk_off");
        assert_eq!(records[0].btc_micro_regime.as_deref(), Some("whipsaw"));
        Ok(())
    }
}
