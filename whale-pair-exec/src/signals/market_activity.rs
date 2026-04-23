#[derive(Debug, Clone, Default)]
pub struct MarketActivitySignal {
    pub last_trade_event_count_10s: u32,
    pub last_trade_event_count_30s: u32,
    pub last_trade_event_count_60s: u32,
    pub last_trade_event_age_ms: Option<u64>,
}
