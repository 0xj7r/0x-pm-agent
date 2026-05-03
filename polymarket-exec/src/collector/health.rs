//! HTTP health endpoint.
//!
//! Returns 200 from `GET /healthz` while a successful Firehose flush
//! has occurred within the last `STALE_THRESHOLD`. After that, the
//! container is considered impaired and Fargate will recycle it.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use axum::{routing::get, Router};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use super::firehose_sink::FirehoseSink;

/// If no Firehose flush has succeeded for this long, healthz returns 503.
pub const STALE_THRESHOLD: Duration = Duration::from_secs(120);

#[derive(Clone)]
pub struct HealthState {
    pub sink: Arc<FirehoseSink>,
}

pub fn router(state: HealthState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .with_state(state)
}

/// Bind and serve `/healthz` until `shutdown` fires.
pub async fn serve(
    addr: std::net::SocketAddr,
    state: HealthState,
    shutdown: CancellationToken,
) -> Result<()> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind health server at {addr}"))?;
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown.cancelled_owned())
        .await
        .context("health server terminated unexpectedly")?;
    Ok(())
}

async fn healthz(State(state): State<HealthState>) -> impl IntoResponse {
    let snap = state.sink.snapshot().await;
    let body = serde_json::json!({
        "last_flush_age_seconds": snap.last_flush_age.as_secs(),
        "events_buffered": snap.events_buffered,
        "events_processed_total": snap.events_processed_total,
    });
    let status = if snap.last_flush_age <= STALE_THRESHOLD {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collector::firehose_sink::{FirehosePutRecordBatch, FirehoseSink};
    use async_trait::async_trait;
    use aws_sdk_firehose::types::Record;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    struct StubBackend;

    #[async_trait]
    impl FirehosePutRecordBatch for StubBackend {
        async fn put_record_batch(&self, _records: Vec<Record>) -> Result<(), String> {
            Ok(())
        }
    }

    fn build_state() -> (HealthState, Arc<FirehoseSink>) {
        let sink = Arc::new(FirehoseSink::new(Arc::new(StubBackend)));
        (
            HealthState {
                sink: sink.clone(),
            },
            sink,
        )
    }

    #[tokio::test]
    async fn fresh_sink_is_healthy() {
        let (state, _sink) = build_state();
        let app = router(state);
        let response = app
            .oneshot(Request::builder().uri("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(body.get("last_flush_age_seconds").is_some());
        assert!(body.get("events_buffered").is_some());
        assert!(body.get("events_processed_total").is_some());
    }

    #[tokio::test]
    async fn body_reflects_processed_total_after_flush() {
        use crate::collector::schema::{Event, EventType, Source};
        use serde_json::json;

        let (state, sink) = build_state();
        let event = Event {
            v: 1,
            ts_ns: 0,
            received_ns: 0,
            event_type: EventType::Heartbeat,
            market_type: "_global".into(),
            market_slug: None,
            asset_id: None,
            side: None,
            price: None,
            size: None,
            sequence: None,
            source: Source::Collector,
            raw: json!({}),
        };
        sink.enqueue(&event).await.unwrap();
        sink.flush_now().await;

        let app = router(state);
        let response = app
            .oneshot(Request::builder().uri("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["events_processed_total"], json!(1u64));
    }
}
