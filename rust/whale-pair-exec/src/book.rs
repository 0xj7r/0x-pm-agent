use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::RwLock;

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
    pub spread: f64,
    pub last_trade_price: f64,
    pub last_update_unix_ms: u64,
    last_update_mono: Option<Instant>,
}

impl Default for BookState {
    fn default() -> Self {
        Self {
            asset_id: String::new(),
            best_bid: 0.0,
            best_bid_size: 0.0,
            best_ask: 0.0,
            best_ask_size: 0.0,
            spread: 0.0,
            last_trade_price: 0.0,
            last_update_unix_ms: 0,
            last_update_mono: None,
        }
    }
}

impl BookState {
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
        if let Some(level) = bids.first().copied() {
            book.best_bid = level.price;
            book.best_bid_size = level.size;
        }
        if let Some(level) = asks.first().copied() {
            book.best_ask = level.price;
            book.best_ask_size = level.size;
        }
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
        if let Some(value) = best_bid {
            book.best_bid = value;
        }
        if let Some(value) = best_ask {
            book.best_ask = value;
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
        book.touch();
        book.clone()
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
