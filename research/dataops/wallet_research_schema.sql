CREATE TABLE IF NOT EXISTS wallets (
    wallet_address TEXT PRIMARY KEY,
    wallet_alias TEXT NOT NULL,
    source TEXT NOT NULL DEFAULT 'polymarket',
    first_seen_ts INTEGER,
    last_seen_ts INTEGER,
    notes TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS collection_runs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    wallet_address TEXT NOT NULL,
    started_at TEXT NOT NULL,
    completed_at TEXT,
    status TEXT NOT NULL,
    config_json TEXT NOT NULL,
    summary_json TEXT,
    error_text TEXT
);

CREATE TABLE IF NOT EXISTS wallet_activity_raw (
    event_id TEXT PRIMARY KEY,
    wallet_address TEXT NOT NULL,
    event_ts INTEGER NOT NULL,
    event_iso TEXT NOT NULL,
    activity_type TEXT,
    side TEXT,
    outcome TEXT,
    slug TEXT,
    event_slug TEXT,
    market_slug TEXT,
    condition_id TEXT,
    transaction_hash TEXT,
    order_id TEXT,
    trade_id TEXT,
    size REAL,
    usdc_size REAL,
    price REAL,
    raw_json TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_wallet_activity_wallet_ts
    ON wallet_activity_raw(wallet_address, event_ts);
CREATE INDEX IF NOT EXISTS idx_wallet_activity_slug
    ON wallet_activity_raw(slug);

CREATE TABLE IF NOT EXISTS wallet_closed_positions_raw (
    position_id TEXT PRIMARY KEY,
    wallet_address TEXT NOT NULL,
    slug TEXT,
    outcome TEXT,
    realized_pnl REAL,
    total_bought REAL,
    avg_price REAL,
    shares REAL,
    end_date_iso TEXT,
    raw_json TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_wallet_closed_positions_wallet
    ON wallet_closed_positions_raw(wallet_address);

CREATE TABLE IF NOT EXISTS wallet_accounting_snapshots (
    snapshot_id TEXT PRIMARY KEY,
    wallet_address TEXT NOT NULL,
    captured_at TEXT NOT NULL,
    source_name TEXT NOT NULL,
    row_count INTEGER,
    payload_json TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS wallet_market_catalog (
    market_id TEXT PRIMARY KEY,
    slug TEXT UNIQUE,
    question TEXT,
    event_title TEXT,
    asset TEXT,
    market_family TEXT,
    start_time TEXT,
    end_time TEXT,
    event_start_time TEXT,
    closed_time TEXT,
    series_slug TEXT,
    resolution_source TEXT,
    price_to_beat REAL,
    final_price REAL,
    closed INTEGER,
    archived INTEGER,
    active INTEGER,
    outcome_names_json TEXT,
    token_ids_json TEXT,
    raw_json TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_wallet_market_catalog_slug
    ON wallet_market_catalog(slug);

CREATE TABLE IF NOT EXISTS wallet_orderbook_snapshots (
    snapshot_id TEXT PRIMARY KEY,
    wallet_address TEXT NOT NULL,
    market_id TEXT,
    slug TEXT,
    token_id TEXT NOT NULL,
    captured_at TEXT NOT NULL,
    best_bid REAL,
    best_ask REAL,
    bid_size REAL,
    ask_size REAL,
    bids_json TEXT,
    asks_json TEXT,
    raw_json TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_wallet_orderbook_snapshots_market_time
    ON wallet_orderbook_snapshots(market_id, captured_at);

CREATE TABLE IF NOT EXISTS wallet_market_stream_events (
    event_id TEXT PRIMARY KEY,
    wallet_address TEXT NOT NULL,
    market_id TEXT,
    slug TEXT,
    token_id TEXT NOT NULL,
    captured_at TEXT NOT NULL,
    source TEXT NOT NULL,
    event_type TEXT NOT NULL,
    best_bid REAL,
    best_ask REAL,
    bid_size REAL,
    ask_size REAL,
    spread REAL,
    last_trade_price REAL,
    trade_price REAL,
    trade_size REAL,
    trade_side TEXT,
    bids_json TEXT,
    asks_json TEXT,
    raw_json TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_wallet_market_stream_events_token_time
    ON wallet_market_stream_events(token_id, captured_at);

CREATE INDEX IF NOT EXISTS idx_wallet_market_stream_events_market_time
    ON wallet_market_stream_events(market_id, captured_at);

CREATE TABLE IF NOT EXISTS wallet_btc_price_series (
    series_key TEXT PRIMARY KEY,
    wallet_address TEXT NOT NULL,
    exchange TEXT NOT NULL,
    symbol TEXT NOT NULL,
    interval TEXT NOT NULL,
    open_time_ms INTEGER NOT NULL,
    close_time_ms INTEGER NOT NULL,
    open REAL,
    high REAL,
    low REAL,
    close REAL,
    volume REAL,
    trade_count INTEGER,
    raw_json TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_wallet_btc_price_series_open_time
    ON wallet_btc_price_series(open_time_ms);

CREATE TABLE IF NOT EXISTS wallet_window_reconstructions (
    window_key TEXT PRIMARY KEY,
    wallet_address TEXT NOT NULL,
    slug TEXT NOT NULL,
    market_id TEXT,
    start_ts INTEGER,
    end_ts INTEGER,
    first_trade_ts INTEGER,
    last_trade_ts INTEGER,
    buy_rows INTEGER NOT NULL,
    sell_rows INTEGER NOT NULL,
    merge_rows INTEGER NOT NULL,
    up_buy_cost REAL,
    down_buy_cost REAL,
    up_buy_shares REAL,
    down_buy_shares REAL,
    up_avg_buy_price REAL,
    down_avg_buy_price REAL,
    up_realized_pnl REAL,
    down_realized_pnl REAL,
    combined_realized_pnl REAL,
    paired_outcomes INTEGER NOT NULL,
    cheap_leg TEXT,
    cheap_leg_avg_price REAL,
    expensive_leg TEXT,
    expensive_leg_avg_price REAL,
    hedge_cost_ratio REAL,
    summary_json TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_wallet_window_reconstructions_wallet_start
    ON wallet_window_reconstructions(wallet_address, start_ts);
