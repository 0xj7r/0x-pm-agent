use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::RwLock;

const MAX_BOOK_LEVELS: usize = 20;
const TRADE_ACTIVITY_WINDOW_10S_MS: u64 = 10_000;
const TRADE_ACTIVITY_WINDOW_30S_MS: u64 = 30_000;
const TRADE_ACTIVITY_WINDOW_60S_MS: u64 = 60_000;
const MAX_TRADE_ACTIVITY_ENTRIES: usize = 4_000;

#[derive(Debug, Clone, Copy, Default)]
pub struct Level {
    pub price: f64,
    pub size: f64,
}

#[derive(Debug, Clone)]
pub struct BookState {
    pub asset_id: String,
    pub best_bid: f64,
    pub best_bid_size: f64,
    pub best_ask: f64,
    pub best_ask_size: f64,
    pub bids: Vec<Level>,
    pub asks: Vec<Level>,
    pub spread: f64,
    pub last_trade_price: f64,
    pub last_update_unix_ms: u64,
    last_update_mono: Option<Instant>,
    trade_events_unix_ms: VecDeque<u64>,
}

impl Default for BookState {
    fn default() -> Self {
        Self {
            asset_id: String::new(),
            best_bid: 0.0,
            best_bid_size: 0.0,
            best_ask: 0.0,
            best_ask_size: 0.0,
            bids: Vec::new(),
            asks: Vec::new(),
            spread: 0.0,
            last_trade_price: 0.0,
            last_update_unix_ms: 0,
            last_update_mono: None,
            trade_events_unix_ms: VecDeque::new(),
        }
    }
}

impl BookState {
    pub fn from_top_of_book(
        asset_id: impl Into<String>,
        best_bid: f64,
        best_bid_size: f64,
        best_ask: f64,
        best_ask_size: f64,
        last_trade_price: f64,
        last_update_unix_ms: u64,
    ) -> Self {
        let spread = if best_bid > 0.0 && best_ask > 0.0 {
            best_ask - best_bid
        } else {
            0.0
        };
        Self {
            asset_id: asset_id.into(),
            best_bid,
            best_bid_size,
            best_ask,
            best_ask_size,
            bids: vec![Level {
                price: best_bid,
                size: best_bid_size,
            }],
            asks: vec![Level {
                price: best_ask,
                size: best_ask_size,
            }],
            spread,
            last_trade_price,
            last_update_unix_ms,
            last_update_mono: None,
            trade_events_unix_ms: VecDeque::new(),
        }
    }

    pub fn age(&self) -> Option<Duration> {
        self.last_update_mono.map(|instant| instant.elapsed())
    }

    pub fn age_ms(&self) -> Option<u64> {
        self.age().map(|age| age.as_millis() as u64)
    }

    pub fn is_stale(&self, max_age: Duration) -> bool {
        match self.age() {
            Some(age) => age > max_age,
            None => true,
        }
    }

    fn touch(&mut self) {
        self.last_update_unix_ms = now_unix_ms();
        self.last_update_mono = Some(Instant::now());
        self.spread = if self.best_bid > 0.0 && self.best_ask > 0.0 {
            self.best_ask - self.best_bid
        } else {
            0.0
        };
    }

    fn prune_trade_events(&mut self, now_ms: u64) {
        while let Some(timestamp) = self.trade_events_unix_ms.front().copied() {
            if now_ms.saturating_sub(timestamp) <= TRADE_ACTIVITY_WINDOW_60S_MS {
                break;
            }
            self.trade_events_unix_ms.pop_front();
        }
        while self.trade_events_unix_ms.len() > MAX_TRADE_ACTIVITY_ENTRIES {
            self.trade_events_unix_ms.pop_front();
        }
    }

    fn record_trade_event(&mut self, observed_at_ms: u64) {
        self.prune_trade_events(observed_at_ms);
        self.trade_events_unix_ms.push_back(observed_at_ms);
    }

    pub fn trade_activity_counts(&mut self, now_ms: u64) -> (u32, u32, u32, Option<u64>) {
        self.prune_trade_events(now_ms);
        let mut c10 = 0u32;
        let mut c30 = 0u32;
        let mut c60 = 0u32;
        for &trade_ms in &self.trade_events_unix_ms {
            let age_ms = now_ms.saturating_sub(trade_ms);
            if age_ms <= TRADE_ACTIVITY_WINDOW_10S_MS {
                c10 += 1;
            }
            if age_ms <= TRADE_ACTIVITY_WINDOW_30S_MS {
                c30 += 1;
            }
            if age_ms <= TRADE_ACTIVITY_WINDOW_60S_MS {
                c60 += 1;
            }
        }
        let last_trade_event_age_ms = self
            .trade_events_unix_ms
            .back()
            .map(|last_ms| now_ms.saturating_sub(*last_ms));
        (c10, c30, c60, last_trade_event_age_ms)
    }

    pub fn bid_levels(&self) -> &[Level] {
        &self.bids
    }

    pub fn ask_levels(&self) -> &[Level] {
        &self.asks
    }

    fn update_levels(&mut self, bids: &[Level], asks: &[Level]) {
        if !bids.is_empty() {
            let mut sorted = normalize_levels(bids, true);
            sorted.sort_by(|left, right| right.price.total_cmp(&left.price));
            self.bids = sorted.into_iter().take(MAX_BOOK_LEVELS).collect();
            if let Some(level) = self.bids.first().copied() {
                self.best_bid = level.price;
                self.best_bid_size = level.size;
            }
        }
        if !asks.is_empty() {
            let mut sorted = normalize_levels(asks, false);
            sorted.sort_by(|left, right| left.price.total_cmp(&right.price));
            self.asks = sorted.into_iter().take(MAX_BOOK_LEVELS).collect();
            if let Some(level) = self.asks.first().copied() {
                self.best_ask = level.price;
                self.best_ask_size = level.size;
            }
        }
    }
}

#[derive(Debug, Default)]
pub struct BookStore {
    inner: RwLock<HashMap<String, BookState>>,
}

impl BookStore {
    pub fn new(seed_assets: &[String]) -> Self {
        let mut books = HashMap::new();
        for asset_id in seed_assets {
            books.insert(
                asset_id.clone(),
                BookState {
                    asset_id: asset_id.clone(),
                    ..BookState::default()
                },
            );
        }
        Self {
            inner: RwLock::new(books),
        }
    }

    pub async fn apply_snapshot(
        &self,
        asset_id: &str,
        bids: &[Level],
        asks: &[Level],
    ) -> BookState {
        let mut guard = self.inner.write().await;
        let book = guard.entry(asset_id.to_string()).or_insert_with(|| BookState {
            asset_id: asset_id.to_string(),
            ..BookState::default()
        });
        book.update_levels(bids, asks);
        book.touch();
        book.clone()
    }

    pub async fn apply_best_bid_ask(
        &self,
        asset_id: &str,
        best_bid: Option<f64>,
        best_ask: Option<f64>,
    ) -> BookState {
        let mut guard = self.inner.write().await;
        let book = guard.entry(asset_id.to_string()).or_insert_with(|| BookState {
            asset_id: asset_id.to_string(),
            ..BookState::default()
        });
        if let Some(best_bid) = best_bid {
            book.best_bid = best_bid;
            book.best_bid_size = book
                .bids
                .iter()
                .find(|level| (level.price - best_bid).abs() < f64::EPSILON)
                .map(|level| level.size)
                .unwrap_or_else(|| book.best_bid_size.max(0.0));
            if book.bids.is_empty() {
                book.bids.push(Level {
                    price: best_bid,
                    size: book.best_bid_size,
                });
                book.bids.sort_by(|left, right| right.price.total_cmp(&left.price));
                book.bids.truncate(MAX_BOOK_LEVELS);
            } else if let Some(first) = book.bids.first_mut() {
                first.price = best_bid;
                if first.size <= 0.0 {
                    first.size = book.best_bid_size;
                }
            }
            book.bids.sort_by(|left, right| right.price.total_cmp(&left.price));
            book.bids.truncate(MAX_BOOK_LEVELS);
        }
        if let Some(best_ask) = best_ask {
            book.best_ask = best_ask;
            book.best_ask_size = book
                .asks
                .iter()
                .find(|level| (level.price - best_ask).abs() < f64::EPSILON)
                .map(|level| level.size)
                .unwrap_or_else(|| book.best_ask_size.max(0.0));
            if book.asks.is_empty() {
                book.asks.push(Level {
                    price: best_ask,
                    size: book.best_ask_size,
                });
                book.asks.sort_by(|left, right| left.price.total_cmp(&right.price));
                book.asks.truncate(MAX_BOOK_LEVELS);
            } else if let Some(first) = book.asks.first_mut() {
                first.price = best_ask;
                if first.size <= 0.0 {
                    first.size = book.best_ask_size;
                }
            }
            book.asks.sort_by(|left, right| left.price.total_cmp(&right.price));
            book.asks.truncate(MAX_BOOK_LEVELS);
        }
        book.touch();
        book.clone()
    }

    pub async fn apply_last_trade(&self, asset_id: &str, price: f64) -> BookState {
        let mut guard = self.inner.write().await;
        let book = guard.entry(asset_id.to_string()).or_insert_with(|| BookState {
            asset_id: asset_id.to_string(),
            ..BookState::default()
        });
        book.last_trade_price = price;
        let observed_at_ms = now_unix_ms();
        book.record_trade_event(observed_at_ms);
        book.last_update_unix_ms = observed_at_ms;
        book.last_update_mono = Some(Instant::now());
        book.spread = if book.best_bid > 0.0 && book.best_ask > 0.0 {
            book.best_ask - book.best_bid
        } else {
            0.0
        };
        book.clone()
    }

    pub async fn record_trade_event(&self, asset_id: &str, observed_at_ms: u64) {
        let mut guard = self.inner.write().await;
        let book = guard.entry(asset_id.to_string()).or_insert_with(|| BookState {
            asset_id: asset_id.to_string(),
            ..BookState::default()
        });
        book.record_trade_event(observed_at_ms);
        book.last_update_unix_ms = observed_at_ms;
        book.last_update_mono = Some(Instant::now());
        book.spread = if book.best_bid > 0.0 && book.best_ask > 0.0 {
            book.best_ask - book.best_bid
        } else {
            0.0
        };
    }

    pub async fn trade_activity(
        &self,
        asset_id: &str,
        now_ms: u64,
    ) -> (u32, u32, u32, Option<u64>) {
        let mut guard = self.inner.write().await;
        let book = guard.entry(asset_id.to_string()).or_insert_with(|| BookState {
            asset_id: asset_id.to_string(),
            ..BookState::default()
        });
        book.trade_activity_counts(now_ms)
    }

    pub async fn snapshot(&self, asset_id: &str) -> Option<BookState> {
        self.inner.read().await.get(asset_id).cloned()
    }

    pub async fn snapshots(&self, asset_ids: &[String]) -> Vec<BookState> {
        let guard = self.inner.read().await;
        asset_ids
            .iter()
            .filter_map(|asset_id| guard.get(asset_id).cloned())
            .collect()
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn normalize_levels(levels: &[Level], highest_bid: bool) -> Vec<Level> {
    let mut normalized = levels
        .iter()
        .filter(|level| level.price.is_finite() && level.size.is_finite())
        .filter(|level| level.price > 0.0 && level.size > 0.0)
        .copied()
        .collect::<Vec<_>>();
    if highest_bid {
        normalized.sort_by(|left, right| right.price.total_cmp(&left.price));
    } else {
        normalized.sort_by(|left, right| left.price.total_cmp(&right.price));
    }
    normalized
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{BookStore, Level};

    #[tokio::test]
    async fn snapshot_updates_top_levels() {
        let store = BookStore::new(&["asset-1".to_string()]);
        let state = store
            .apply_snapshot(
                "asset-1",
                &[Level {
                    price: 0.41,
                    size: 120.0,
                }],
                &[Level {
                    price: 0.43,
                    size: 75.0,
                }],
            )
            .await;

        assert_eq!(state.best_bid, 0.41);
        assert_eq!(state.best_ask, 0.43);
        assert_eq!(state.best_bid_size, 120.0);
        assert_eq!(state.best_ask_size, 75.0);
        assert!(!state.is_stale(Duration::from_secs(1)));
    }

    #[tokio::test]
    async fn best_bid_ask_update_preserves_existing_sizes() {
        let seed_assets: Vec<String> = Vec::new();
        let store = BookStore::new(&seed_assets);
        store
            .apply_snapshot(
                "asset-2",
                &[Level {
                    price: 0.20,
                    size: 10.0,
                }],
                &[Level {
                    price: 0.24,
                    size: 8.0,
                }],
            )
            .await;
        let state = store
            .apply_best_bid_ask("asset-2", Some(0.21), Some(0.25))
            .await;

        assert_eq!(state.best_bid, 0.21);
        assert_eq!(state.best_ask, 0.25);
        assert_eq!(state.best_bid_size, 10.0);
        assert_eq!(state.best_ask_size, 8.0);
    }
}
