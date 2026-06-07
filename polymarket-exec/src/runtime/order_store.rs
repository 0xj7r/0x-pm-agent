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
        store.migrate()?;
        Ok(store)
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
}

#[cfg(test)]
mod tests {
    use super::{AccountingLane, OrderStore, OrderStoreError, SqliteOrderStore};
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
}
