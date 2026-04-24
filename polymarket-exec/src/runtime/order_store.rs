//! Durable SQLite-backed order/signal persistence and recovery primitives.

use std::fmt;
use std::path::PathBuf;

use rusqlite::{params, types::Type, Connection, OptionalExtension, Row};

use crate::runtime::types::ManagedOrderStatus;
use crate::types::{
    ClientOrderId, EpochMillis, InstrumentId, MarketId, OrderId, OrderIntent, TradeSide,
};

const DUST_REMAINING_QTY: f64 = 0.01;
const DUST_REMAINING_NOTIONAL_USD: f64 = 0.01;

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
            quote_level_tag: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SignalSnapshotRecord {
    pub run_id: String,
    pub market_id: MarketId,
    pub observed_at_ms: EpochMillis,
    pub session_bucket: String,
    pub mode: String,
    pub aggression_tier: Option<String>,
    pub cheap_instrument_id: InstrumentId,
    pub expensive_instrument_id: InstrumentId,
    pub cheap_bid: Option<f64>,
    pub cheap_ask: Option<f64>,
    pub expensive_bid: Option<f64>,
    pub expensive_ask: Option<f64>,
    pub price_gap: Option<f64>,
    pub books_fresh: bool,
    pub both_sides_present: bool,
    pub cheap_spread: Option<f64>,
    pub expensive_spread: Option<f64>,
    pub cheap_bid_depth_top3_qty: Option<f64>,
    pub cheap_ask_depth_top3_qty: Option<f64>,
    pub expensive_bid_depth_top3_qty: Option<f64>,
    pub expensive_ask_depth_top3_qty: Option<f64>,
    pub cheap_bid_notional_top3: Option<f64>,
    pub cheap_ask_notional_top3: Option<f64>,
    pub expensive_bid_notional_top3: Option<f64>,
    pub expensive_ask_notional_top3: Option<f64>,
    pub cheap_depth_imbalance_top3: Option<f64>,
    pub expensive_depth_imbalance_top3: Option<f64>,
    pub btc_last_price: Option<f64>,
    pub btc_realized_vol_5m_bps: Option<f64>,
    pub btc_realized_vol_15m_bps: Option<f64>,
    pub btc_trade_count_5m: u64,
    pub btc_trade_count_15m: u64,
    pub btc_return_30s_bps: Option<f64>,
    pub btc_return_60s_bps: Option<f64>,
    pub btc_observed_at_ms: EpochMillis,
    pub activity_10s: u32,
    pub activity_30s: u32,
    pub activity_60s: u32,
    pub activity_age_ms: Option<u64>,
    pub first_fill_ms: Option<u64>,
    pub first_merge_ms: Option<u64>,
    pub elapsed_s: Option<u64>,
    pub time_remaining_s: Option<u64>,
    pub clip_scale: f64,
    pub gate_reasons: String,
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
    fn get(
        &self,
        client_order_id: &ClientOrderId,
    ) -> std::result::Result<Option<OrderRecord>, OrderStoreError>;
    fn list_open(&self) -> std::result::Result<Vec<OrderRecord>, OrderStoreError>;
    fn list_by_market(
        &self,
        market_id: &MarketId,
    ) -> std::result::Result<Vec<OrderRecord>, OrderStoreError>;
    fn insert_signal_snapshot(
        &mut self,
        record: SignalSnapshotRecord,
    ) -> std::result::Result<(), OrderStoreError>;
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
                    quote_level_tag TEXT
                )",
                (),
            )
            .map_err(|error| {
                OrderStoreError::Sqlite(format!("failed to create orders table: {error}"))
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
                "CREATE TABLE IF NOT EXISTS signal_snapshots (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    run_id TEXT NOT NULL,
                    market_id TEXT NOT NULL,
                    observed_at_ms INTEGER NOT NULL,
                    session_bucket TEXT NOT NULL,
                    mode TEXT NOT NULL,
                    aggression_tier TEXT,
                    cheap_instrument_id TEXT NOT NULL,
                    expensive_instrument_id TEXT NOT NULL,
                    cheap_bid REAL,
                    cheap_ask REAL,
                    expensive_bid REAL,
                    expensive_ask REAL,
                    price_gap REAL,
                    books_fresh INTEGER NOT NULL,
                    both_sides_present INTEGER NOT NULL,
                    cheap_spread REAL,
                    expensive_spread REAL,
                    cheap_bid_depth_top3_qty REAL,
                    cheap_ask_depth_top3_qty REAL,
                    expensive_bid_depth_top3_qty REAL,
                    expensive_ask_depth_top3_qty REAL,
                    cheap_bid_notional_top3 REAL,
                    cheap_ask_notional_top3 REAL,
                    expensive_bid_notional_top3 REAL,
                    expensive_ask_notional_top3 REAL,
                    cheap_depth_imbalance_top3 REAL,
                    expensive_depth_imbalance_top3 REAL,
                    btc_last_price REAL,
                    btc_realized_vol_5m_bps REAL,
                    btc_realized_vol_15m_bps REAL,
                    btc_trade_count_5m INTEGER NOT NULL,
                    btc_trade_count_15m INTEGER NOT NULL,
                    btc_return_30s_bps REAL,
                    btc_return_60s_bps REAL,
                    btc_observed_at_ms INTEGER NOT NULL,
                    activity_10s INTEGER NOT NULL,
                    activity_30s INTEGER NOT NULL,
                    activity_60s INTEGER NOT NULL,
                    activity_age_ms INTEGER,
                    first_fill_ms INTEGER,
                    first_merge_ms INTEGER,
                    elapsed_s INTEGER,
                    time_remaining_s INTEGER,
                    clip_scale REAL NOT NULL,
                    gate_reasons TEXT NOT NULL
                )",
                (),
            )
            .map_err(|error| {
                OrderStoreError::Sqlite(format!("failed to create signal_snapshots table: {error}"))
            })?;

        self.connection
            .execute(
                "CREATE INDEX IF NOT EXISTS idx_signal_snapshots_market_time
                 ON signal_snapshots (market_id, observed_at_ms)",
                (),
            )
            .map_err(|error| {
                OrderStoreError::Sqlite(format!(
                    "failed to create signal_snapshots market-time index: {error}"
                ))
            })?;

        for (column, declaration) in [
            ("cheap_spread", "REAL"),
            ("expensive_spread", "REAL"),
            ("cheap_bid_depth_top3_qty", "REAL"),
            ("cheap_ask_depth_top3_qty", "REAL"),
            ("expensive_bid_depth_top3_qty", "REAL"),
            ("expensive_ask_depth_top3_qty", "REAL"),
            ("cheap_bid_notional_top3", "REAL"),
            ("cheap_ask_notional_top3", "REAL"),
            ("expensive_bid_notional_top3", "REAL"),
            ("expensive_ask_notional_top3", "REAL"),
            ("cheap_depth_imbalance_top3", "REAL"),
            ("expensive_depth_imbalance_top3", "REAL"),
        ] {
            self.ensure_column("signal_snapshots", column, declaration)?;
        }

        Ok(())
    }

    fn ensure_column(
        &self,
        table: &str,
        column: &str,
        declaration: &str,
    ) -> std::result::Result<(), OrderStoreError> {
        let pragma = format!("PRAGMA table_info({table})");
        let exists = self
            .connection
            .prepare(&pragma)
            .and_then(|mut stmt| {
                let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
                Ok(rows.filter_map(Result::ok).any(|name| name == column))
            })
            .map_err(|error| {
                OrderStoreError::Sqlite(format!(
                    "failed to inspect sqlite table info for {table}: {error}"
                ))
            })?;
        if exists {
            return Ok(());
        }
        let alter = format!("ALTER TABLE {table} ADD COLUMN {column} {declaration}");
        self.connection.execute(&alter, ()).map_err(|error| {
            OrderStoreError::Sqlite(format!(
                "failed to add sqlite column {table}.{column}: {error}"
            ))
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

    fn list_open_query_for_market(include_terminal: bool) -> String {
        if include_terminal {
            "SELECT * FROM orders
             WHERE market_id = ?1
               AND remaining_qty > 0.0
             ORDER BY last_update_ms ASC"
                .to_string()
        } else {
            "SELECT * FROM orders
             WHERE market_id = ?1
               AND status IN ('PendingSubmit', 'Submitted', 'Working', 'CancelRequested', 'NeedsReconcile')
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
                    quote_level_tag
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
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

        if !record.status.can_transition_to(status) {
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
        let query = Self::list_open_query_for_market(false);
        let mut statement = self.connection.prepare(&query).map_err(|error| {
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

    fn insert_signal_snapshot(
        &mut self,
        record: SignalSnapshotRecord,
    ) -> std::result::Result<(), OrderStoreError> {
        self.connection
            .execute(
                "INSERT INTO signal_snapshots (
                    run_id,
                    market_id,
                    observed_at_ms,
                    session_bucket,
                    mode,
                    aggression_tier,
                    cheap_instrument_id,
                    expensive_instrument_id,
                    cheap_bid,
                    cheap_ask,
                    expensive_bid,
                    expensive_ask,
                    price_gap,
                    books_fresh,
                    both_sides_present,
                    cheap_spread,
                    expensive_spread,
                    cheap_bid_depth_top3_qty,
                    cheap_ask_depth_top3_qty,
                    expensive_bid_depth_top3_qty,
                    expensive_ask_depth_top3_qty,
                    cheap_bid_notional_top3,
                    cheap_ask_notional_top3,
                    expensive_bid_notional_top3,
                    expensive_ask_notional_top3,
                    cheap_depth_imbalance_top3,
                    expensive_depth_imbalance_top3,
                    btc_last_price,
                    btc_realized_vol_5m_bps,
                    btc_realized_vol_15m_bps,
                    btc_trade_count_5m,
                    btc_trade_count_15m,
                    btc_return_30s_bps,
                    btc_return_60s_bps,
                    btc_observed_at_ms,
                    activity_10s,
                    activity_30s,
                    activity_60s,
                    activity_age_ms,
                    first_fill_ms,
                    first_merge_ms,
                    elapsed_s,
                    time_remaining_s,
                    clip_scale,
                    gate_reasons
                ) VALUES (
                    ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                    ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24,
                    ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33, ?34, ?35,
                    ?36, ?37, ?38, ?39, ?40, ?41, ?42, ?43, ?44, ?45
                )",
                params![
                    record.run_id,
                    record.market_id.as_str(),
                    record.observed_at_ms as i64,
                    record.session_bucket,
                    record.mode,
                    record.aggression_tier,
                    record.cheap_instrument_id.as_str(),
                    record.expensive_instrument_id.as_str(),
                    record.cheap_bid,
                    record.cheap_ask,
                    record.expensive_bid,
                    record.expensive_ask,
                    record.price_gap,
                    if record.books_fresh { 1 } else { 0 },
                    if record.both_sides_present { 1 } else { 0 },
                    record.cheap_spread,
                    record.expensive_spread,
                    record.cheap_bid_depth_top3_qty,
                    record.cheap_ask_depth_top3_qty,
                    record.expensive_bid_depth_top3_qty,
                    record.expensive_ask_depth_top3_qty,
                    record.cheap_bid_notional_top3,
                    record.cheap_ask_notional_top3,
                    record.expensive_bid_notional_top3,
                    record.expensive_ask_notional_top3,
                    record.cheap_depth_imbalance_top3,
                    record.expensive_depth_imbalance_top3,
                    record.btc_last_price,
                    record.btc_realized_vol_5m_bps,
                    record.btc_realized_vol_15m_bps,
                    record.btc_trade_count_5m as i64,
                    record.btc_trade_count_15m as i64,
                    record.btc_return_30s_bps,
                    record.btc_return_60s_bps,
                    record.btc_observed_at_ms as i64,
                    record.activity_10s as i64,
                    record.activity_30s as i64,
                    record.activity_60s as i64,
                    record.activity_age_ms.map(|value| value as i64),
                    record.first_fill_ms.map(|value| value as i64),
                    record.first_merge_ms.map(|value| value as i64),
                    record.elapsed_s.map(|value| value as i64),
                    record.time_remaining_s.map(|value| value as i64),
                    record.clip_scale,
                    record.gate_reasons,
                ],
            )
            .map_err(|error| {
                OrderStoreError::Sqlite(format!("failed to insert signal snapshot: {error}"))
            })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{OrderStore, OrderStoreError, SignalSnapshotRecord, SqliteOrderStore};
    use crate::types::{
        ClientOrderId, EpochMillis, InstrumentId, MarketId, OrderId, OrderIntent, TradeSide,
    };

    use std::env;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::runtime::types::ManagedOrderStatus;

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
        };
        let record = crate::runtime::order_store::OrderRecord::from_intent("run", &intent, "strat");
        store.insert(record.clone())?;
        let result = store.insert(record);
        assert!(matches!(result, Err(OrderStoreError::Conflict(_))));
        Ok(())
    }

    #[test]
    fn signal_snapshots_are_persisted() -> anyhow::Result<()> {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = env::temp_dir().join(format!("polymarket-exec-signal-store-{ts}.sqlite"));
        let mut store = SqliteOrderStore::open(path)?;

        store.insert_signal_snapshot(SignalSnapshotRecord {
            run_id: "run-1".to_string(),
            market_id: MarketId::from("mkt-1"),
            observed_at_ms: 1234,
            session_bucket: "Opportunistic".to_string(),
            mode: "Manage".to_string(),
            aggression_tier: Some("Light".to_string()),
            cheap_instrument_id: InstrumentId::from("cheap-1"),
            expensive_instrument_id: InstrumentId::from("expensive-1"),
            cheap_bid: Some(0.39),
            cheap_ask: Some(0.40),
            expensive_bid: Some(0.59),
            expensive_ask: Some(0.60),
            price_gap: Some(0.20),
            books_fresh: true,
            both_sides_present: true,
            cheap_spread: Some(0.01),
            expensive_spread: Some(0.01),
            cheap_bid_depth_top3_qty: Some(240.0),
            cheap_ask_depth_top3_qty: Some(180.0),
            expensive_bid_depth_top3_qty: Some(210.0),
            expensive_ask_depth_top3_qty: Some(190.0),
            cheap_bid_notional_top3: Some(93.0),
            cheap_ask_notional_top3: Some(72.0),
            expensive_bid_notional_top3: Some(123.9),
            expensive_ask_notional_top3: Some(114.0),
            cheap_depth_imbalance_top3: Some(0.142857),
            expensive_depth_imbalance_top3: Some(0.05),
            btc_last_price: Some(77700.0),
            btc_realized_vol_5m_bps: Some(3.0),
            btc_realized_vol_15m_bps: Some(5.0),
            btc_trade_count_5m: 100,
            btc_trade_count_15m: 150,
            btc_return_30s_bps: Some(1.2),
            btc_return_60s_bps: Some(2.4),
            btc_observed_at_ms: 1200,
            activity_10s: 50,
            activity_30s: 100,
            activity_60s: 150,
            activity_age_ms: Some(250),
            first_fill_ms: Some(1240),
            first_merge_ms: None,
            elapsed_s: Some(12),
            time_remaining_s: Some(288),
            clip_scale: 0.8,
            gate_reasons: "none".to_string(),
        })?;

        let count: i64 =
            store
                .connection
                .query_row("SELECT COUNT(*) FROM signal_snapshots", (), |row| {
                    row.get(0)
                })?;
        assert_eq!(count, 1);
        Ok(())
    }
}
