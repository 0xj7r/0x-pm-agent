//! Asynchronous batched Firehose writer.
//!
//! ## Buffering policy
//!
//! Records are appended to an in-memory buffer and flushed when ANY
//! threshold trips:
//! - 60 seconds since the last flush;
//! - 1 MiB of pre-encoded JSON+newline payload;
//! - 500 records (Firehose's `PutRecordBatch` upper limit).
//!
//! ## Failure model
//!
//! Firehose IS our durable buffer. We do not spool to local disk: a
//! Fargate Spot task can be terminated at any time and disk would be
//! lost anyway. On `PutRecordBatch` errors we retry up to 5 times with
//! exponential backoff capped at 30s, then drop the batch and increment
//! a metric. The collector's downstream consumers must therefore be
//! tolerant of small windows of missing data when Firehose itself is
//! impaired (typically <5 minutes per AWS regional incident).

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use aws_sdk_firehose::{primitives::Blob, types::Record, Client as FirehoseClient};
use tokio::sync::Mutex;
use tracing::{debug, warn};

use super::schema::Event;

/// Hard limits per Firehose's `PutRecordBatch` API.
pub const MAX_RECORDS_PER_BATCH: usize = 500;
pub const MAX_BYTES_PER_BATCH: usize = 1024 * 1024;
/// Wall-clock flush interval.
pub const MAX_FLUSH_INTERVAL: Duration = Duration::from_secs(60);

const MAX_RETRY_ATTEMPTS: u32 = 5;
const INITIAL_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Abstraction over the AWS SDK so unit tests can swap in a fake.
#[async_trait]
pub trait FirehosePutRecordBatch: Send + Sync {
    async fn put_record_batch(&self, records: Vec<Record>) -> Result<(), String>;
}

/// Production adapter that delegates to the real AWS SDK client.
pub struct AwsFirehose {
    client: FirehoseClient,
    delivery_stream: String,
}

impl AwsFirehose {
    pub fn new(client: FirehoseClient, delivery_stream: String) -> Self {
        Self {
            client,
            delivery_stream,
        }
    }
}

#[async_trait]
impl FirehosePutRecordBatch for AwsFirehose {
    async fn put_record_batch(&self, records: Vec<Record>) -> Result<(), String> {
        self.client
            .put_record_batch()
            .delivery_stream_name(&self.delivery_stream)
            .set_records(Some(records))
            .send()
            .await
            .map(|_| ())
            .map_err(|e| format!("{e}"))
    }
}

/// Snapshot of sink-internal state used for health checks and metrics.
#[derive(Debug, Clone)]
pub struct SinkSnapshot {
    pub events_buffered: usize,
    pub events_processed_total: u64,
    pub last_flush_age: Duration,
    pub last_flush_latency: Option<Duration>,
    pub firehose_throttle_count: u64,
    pub records_dropped_total: u64,
}

#[derive(Debug)]
struct SinkState {
    buffer: Vec<Vec<u8>>,
    buffer_bytes: usize,
    last_flush_at: Instant,
    last_flush_latency: Option<Duration>,
    events_processed_total: u64,
    firehose_throttle_count: u64,
    records_dropped_total: u64,
}

impl SinkState {
    fn new() -> Self {
        Self {
            buffer: Vec::with_capacity(MAX_RECORDS_PER_BATCH),
            buffer_bytes: 0,
            last_flush_at: Instant::now(),
            last_flush_latency: None,
            events_processed_total: 0,
            firehose_throttle_count: 0,
            records_dropped_total: 0,
        }
    }
}

/// Public sink handle. `Arc<FirehoseSink>` is cheap to clone; share it
/// across raw_tap consumers, the periodic flush task, and health checks.
pub struct FirehoseSink {
    backend: Arc<dyn FirehosePutRecordBatch>,
    state: Mutex<SinkState>,
}

impl FirehoseSink {
    pub fn new(backend: Arc<dyn FirehosePutRecordBatch>) -> Self {
        Self {
            backend,
            state: Mutex::new(SinkState::new()),
        }
    }

    /// Append one event to the buffer. Returns `true` if a flush should
    /// be triggered as a result. Caller is responsible for invoking
    /// `flush_now` when this returns `true`; we keep them separate so
    /// the lock isn't held across an `await` to AWS.
    pub async fn enqueue(&self, event: &Event) -> Result<bool, serde_json::Error> {
        let mut payload = serde_json::to_vec(event)?;
        payload.push(b'\n');
        let mut state = self.state.lock().await;
        state.buffer_bytes += payload.len();
        state.buffer.push(payload);
        let trigger = state.buffer.len() >= MAX_RECORDS_PER_BATCH
            || state.buffer_bytes >= MAX_BYTES_PER_BATCH;
        Ok(trigger)
    }

    /// Force a flush regardless of thresholds. Drains the buffer first;
    /// if a transient AWS error occurs after retries are exhausted, the
    /// drained batch is dropped and `records_dropped_total` is incremented.
    pub async fn flush_now(&self) {
        let drained = {
            let mut state = self.state.lock().await;
            if state.buffer.is_empty() {
                state.last_flush_at = Instant::now();
                return;
            }
            std::mem::take(&mut state.buffer)
        };
        let record_count = drained.len();
        let records: Vec<Record> = drained
            .into_iter()
            .filter_map(|bytes| {
                Record::builder()
                    .data(Blob::new(bytes))
                    .build()
                    .map_err(|e| {
                        warn!(error = %e, "failed to build firehose record; dropping");
                        e
                    })
                    .ok()
            })
            .collect();

        let started = Instant::now();
        let result = self.attempt_with_backoff(records).await;
        let elapsed = started.elapsed();

        let mut state = self.state.lock().await;
        state.buffer_bytes = 0;
        state.last_flush_at = Instant::now();
        state.last_flush_latency = Some(elapsed);
        match result {
            Ok(()) => {
                state.events_processed_total =
                    state.events_processed_total.saturating_add(record_count as u64);
                debug!(records = record_count, elapsed_ms = %elapsed.as_millis(), "firehose flush ok");
            }
            Err(err) => {
                state.records_dropped_total =
                    state.records_dropped_total.saturating_add(record_count as u64);
                state.firehose_throttle_count =
                    state.firehose_throttle_count.saturating_add(1);
                warn!(
                    error = %err,
                    records_dropped = record_count,
                    "firehose flush exhausted retries; dropping batch"
                );
            }
        }
    }

    /// Returns `Some(())` if the wall-clock interval has elapsed since
    /// the last flush. Cheap; safe to call from a 1Hz timer.
    pub async fn should_periodic_flush(&self) -> bool {
        let state = self.state.lock().await;
        state.last_flush_at.elapsed() >= MAX_FLUSH_INTERVAL
    }

    pub async fn snapshot(&self) -> SinkSnapshot {
        let state = self.state.lock().await;
        SinkSnapshot {
            events_buffered: state.buffer.len(),
            events_processed_total: state.events_processed_total,
            last_flush_age: state.last_flush_at.elapsed(),
            last_flush_latency: state.last_flush_latency,
            firehose_throttle_count: state.firehose_throttle_count,
            records_dropped_total: state.records_dropped_total,
        }
    }

    async fn attempt_with_backoff(&self, records: Vec<Record>) -> Result<(), String> {
        if records.is_empty() {
            return Ok(());
        }
        let mut backoff = INITIAL_BACKOFF;
        let mut last_err = String::from("no attempts");
        for attempt in 0..MAX_RETRY_ATTEMPTS {
            match self.backend.put_record_batch(records.clone()).await {
                Ok(()) => return Ok(()),
                Err(err) => {
                    last_err = err;
                    if attempt + 1 == MAX_RETRY_ATTEMPTS {
                        break;
                    }
                    debug!(attempt, error = %last_err, "firehose retry");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }
        }
        Err(last_err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collector::schema::{EventType, Source};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    fn make_event(idx: u64) -> Event {
        Event {
            v: 1,
            ts_ns: idx as i64,
            received_ns: idx as i64,
            event_type: EventType::Trade,
            market_type: "btc_5m".into(),
            market_slug: Some("slug".into()),
            asset_id: Some(format!("asset-{idx}")),
            side: Some("BUY".into()),
            price: Some("0.50".into()),
            size: Some("1.0".into()),
            sequence: Some(idx as i64),
            source: Source::PolymarketMarketWs,
            raw: json!({"i": idx}),
        }
    }

    struct FakeBackend {
        calls: AtomicUsize,
        fail_first: usize,
        recorded: Mutex<Vec<Vec<Record>>>,
    }

    impl FakeBackend {
        fn ok() -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                fail_first: 0,
                recorded: Mutex::new(Vec::new()),
            })
        }
        fn flaky(fail_first: usize) -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                fail_first,
                recorded: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl FirehosePutRecordBatch for FakeBackend {
        async fn put_record_batch(&self, records: Vec<Record>) -> Result<(), String> {
            let n = self.calls.fetch_add(1, AtomicOrdering::SeqCst);
            if n < self.fail_first {
                return Err(format!("induced failure {n}"));
            }
            self.recorded.lock().await.push(records);
            Ok(())
        }
    }

    #[tokio::test]
    async fn flush_now_sends_all_buffered_records() {
        let backend = FakeBackend::ok();
        let sink = FirehoseSink::new(backend.clone());
        for i in 0..3 {
            let trigger = sink.enqueue(&make_event(i)).await.unwrap();
            assert!(!trigger);
        }
        sink.flush_now().await;
        let recorded = backend.recorded.lock().await;
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].len(), 3);
        let snap = sink.snapshot().await;
        assert_eq!(snap.events_processed_total, 3);
        assert_eq!(snap.records_dropped_total, 0);
    }

    #[tokio::test]
    async fn enqueue_signals_flush_at_500_records() {
        let backend = FakeBackend::ok();
        let sink = FirehoseSink::new(backend.clone());
        let mut triggered = false;
        for i in 0..MAX_RECORDS_PER_BATCH as u64 {
            let trigger = sink.enqueue(&make_event(i)).await.unwrap();
            triggered |= trigger;
        }
        assert!(triggered);
    }

    #[tokio::test]
    async fn retries_then_succeeds() {
        let backend = FakeBackend::flaky(2);
        // Tighten retries by reusing a sink with the real constants;
        // 2 failures fit in MAX_RETRY_ATTEMPTS (=5).
        let sink = FirehoseSink::new(backend.clone());
        sink.enqueue(&make_event(1)).await.unwrap();
        sink.flush_now().await;
        assert_eq!(backend.calls.load(AtomicOrdering::SeqCst), 3);
        let snap = sink.snapshot().await;
        assert_eq!(snap.events_processed_total, 1);
        assert_eq!(snap.records_dropped_total, 0);
    }

    #[tokio::test]
    async fn drops_batch_after_exhausting_retries() {
        let backend = FakeBackend::flaky(MAX_RETRY_ATTEMPTS as usize);
        let sink = FirehoseSink::new(backend.clone());
        sink.enqueue(&make_event(1)).await.unwrap();
        sink.enqueue(&make_event(2)).await.unwrap();
        sink.flush_now().await;
        let snap = sink.snapshot().await;
        assert_eq!(snap.events_processed_total, 0);
        assert_eq!(snap.records_dropped_total, 2);
        assert_eq!(snap.firehose_throttle_count, 1);
    }

    #[tokio::test]
    async fn empty_flush_is_a_noop() {
        let backend = FakeBackend::ok();
        let sink = FirehoseSink::new(backend.clone());
        sink.flush_now().await;
        let snap = sink.snapshot().await;
        assert_eq!(snap.events_buffered, 0);
        assert_eq!(snap.events_processed_total, 0);
        assert_eq!(backend.calls.load(AtomicOrdering::SeqCst), 0);
    }
}
