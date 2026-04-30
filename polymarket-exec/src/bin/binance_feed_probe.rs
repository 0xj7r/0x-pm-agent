use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures_util::StreamExt;
use serde_json::Value;
use tokio::time::timeout;
use tokio_tungstenite::{connect_async, tungstenite::Message};

#[tokio::main]
async fn main() -> Result<()> {
    let urls = [
        "wss://stream.binance.com:9443/ws/btcusdt@aggTrade",
        "wss://data-stream.binance.vision/ws/btcusdt@aggTrade",
        "wss://stream.binance.com:9443/stream?streams=btcusdt@aggTrade",
        "wss://data-stream.binance.vision/stream?streams=btcusdt@aggTrade",
    ];

    for url in urls {
        match probe_url(url).await {
            Ok(report) => {
                println!(
                    "OK url={} frames={} agg_trades={} first_trade_ms={} elapsed_ms={}",
                    url,
                    report.frames,
                    report.agg_trades,
                    report.first_trade_ms.unwrap_or_default(),
                    report.elapsed.as_millis()
                );
            }
            Err(error) => {
                println!("ERR url={} error={:#}", url, error);
            }
        }
    }

    Ok(())
}

struct ProbeReport {
    frames: usize,
    agg_trades: usize,
    first_trade_ms: Option<u64>,
    elapsed: Duration,
}

async fn probe_url(url: &str) -> Result<ProbeReport> {
    let started = Instant::now();
    let (stream, _) = timeout(Duration::from_secs(10), connect_async(url))
        .await
        .context("connect timed out")?
        .with_context(|| format!("connect failed for {url}"))?;
    let (_write, mut read) = stream.split();

    let mut frames = 0usize;
    let mut agg_trades = 0usize;
    let mut first_trade_ms = None;
    while started.elapsed() < Duration::from_secs(10) && agg_trades < 3 {
        let Some(frame) = timeout(Duration::from_secs(5), read.next())
            .await
            .context("read timed out")?
        else {
            anyhow::bail!("stream ended");
        };
        frames += 1;
        match frame.context("frame error")? {
            Message::Text(text) => {
                let payload: Value = serde_json::from_str(&text).context("invalid json")?;
                let data = payload
                    .get("data")
                    .filter(|value| value.is_object())
                    .unwrap_or(&payload);
                if data.get("e").and_then(Value::as_str) == Some("aggTrade") {
                    agg_trades += 1;
                    first_trade_ms = first_trade_ms
                        .or_else(|| data.get("T").and_then(Value::as_u64))
                        .or_else(|| data.get("E").and_then(Value::as_u64));
                }
            }
            Message::Close(close) => anyhow::bail!("closed by remote: {close:?}"),
            _ => {}
        }
    }

    if agg_trades == 0 {
        anyhow::bail!("no aggTrade frames received");
    }

    Ok(ProbeReport {
        frames,
        agg_trades,
        first_trade_ms,
        elapsed: started.elapsed(),
    })
}
