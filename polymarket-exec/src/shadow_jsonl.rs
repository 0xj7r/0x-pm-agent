//! Tail `shadow-final` JSONL and emit [`pm_shadow::ExecIntent`] for the execution loop.
//!
//! Decisions come ONLY from validated `would_enter` lines — no belief recompute.

use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use pm_shadow::ExecIntent;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, info, warn};

use crate::shadow_gamma::{close_ts_from_slug, GammaResolver};
use crate::shadow_parity::{now_unix_s, parse_ts_utc, SharedParityGate};

const POLL_MS: u64 = 200;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DedupKey {
    slug: String,
    side: String,
    clip: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct TailState {
    path: String,
    offset: u64,
    #[serde(default)]
    seen: Vec<String>,
}

impl TailState {
    fn dedup_key(key: &DedupKey) -> String {
        format!("{}|{}|{}", key.slug, key.side, key.clip)
    }

    fn insert(&mut self, key: &DedupKey) {
        let s = Self::dedup_key(key);
        if !self.seen.iter().any(|x| x == &s) {
            self.seen.push(s);
        }
    }
}

#[derive(Debug, Deserialize)]
struct WouldEnterRow {
    #[serde(rename = "type")]
    event_type: String,
    #[serde(default)]
    ts_utc: String,
    slug: String,
    side: String,
    p_exo: f64,
    #[serde(default)]
    p_side: Option<f64>,
    touch_price: f64,
    edge: f64,
    #[serde(default)]
    marketable_limit_price: Option<f64>,
    clip: u32,
    #[serde(default)]
    token_id: Option<String>,
    #[serde(default)]
    target_notional: Option<f64>,
    #[serde(default)]
    close_ts_s: Option<i64>,
    #[serde(default)]
    condition_id: Option<String>,
    #[serde(default)]
    up_index_set: Option<u64>,
    #[serde(default)]
    down_index_set: Option<u64>,
    #[serde(default)]
    sigma_bar_bps: Option<f64>,
    #[serde(default)]
    strike: Option<f64>,
}

fn would_enter_to_intent(row: WouldEnterRow) -> Option<ExecIntent> {
    if row.event_type != "would_enter" {
        return None;
    }
    let p_side = row.p_side.unwrap_or_else(|| {
        if row.side == "up" {
            row.p_exo
        } else {
            1.0 - row.p_exo
        }
    });
    let marketable_limit_price = row
        .marketable_limit_price
        .unwrap_or(p_side - row.edge);
    let token_id = row.token_id.unwrap_or_default();
    let close_ts_s = row
        .close_ts_s
        .or_else(|| close_ts_from_slug(&row.slug))
        .unwrap_or(0);
    Some(ExecIntent {
        slug: row.slug,
        side: row.side,
        token_id,
        p_exo: row.p_exo,
        p_side,
        touch_price: row.touch_price,
        marketable_limit_price,
        target_notional: row.target_notional.unwrap_or(50.0),
        hold_to_redemption: true,
        clip: row.clip,
        edge: row.edge,
        sigma_bar_bps: row.sigma_bar_bps.unwrap_or(0.0),
        strike: row.strike.unwrap_or(0.0),
        close_ts_s,
        condition_id: row.condition_id,
        up_index_set: row.up_index_set.unwrap_or(1),
        down_index_set: row.down_index_set.unwrap_or(2),
    })
}

pub fn parse_would_enter_line(line: &str) -> Option<ExecIntent> {
    let row: WouldEnterRow = serde_json::from_str(line).ok()?;
    would_enter_to_intent(row)
}

async fn enrich_intent(intent: &mut ExecIntent, gamma: &GammaResolver) -> Result<()> {
    if !intent.token_id.is_empty() && intent.close_ts_s > 0 {
        return Ok(());
    }
    let meta = gamma.resolve(&intent.slug).await?;
    if intent.token_id.is_empty() {
        intent.token_id = if intent.side == "up" {
            meta.up_token.clone()
        } else {
            meta.down_token.clone()
        };
        info!(slug = %intent.slug, side = %intent.side, "gamma resolved token_id");
    }
    if intent.close_ts_s <= 0 {
        intent.close_ts_s = meta.close_ts_s;
    }
    if intent.condition_id.is_none() {
        intent.condition_id = meta.condition_id.clone();
    }
    if intent.up_index_set == 1 && intent.down_index_set == 2 {
        intent.up_index_set = meta.up_index_set;
        intent.down_index_set = meta.down_index_set;
    }
    Ok(())
}

fn is_shadow_jsonl(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.starts_with("shadow-") && n.ends_with(".jsonl"))
        .unwrap_or(false)
}

pub fn latest_shadow_jsonl(dir: &Path) -> Result<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("read dir {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && is_shadow_jsonl(p))
        .collect();
    files.sort();
    files
        .pop()
        .with_context(|| format!("no shadow-*.jsonl under {}", dir.display()))
}

pub struct JsonlTailer {
    source: PathBuf,
    follow: bool,
    state_path: PathBuf,
    initial_offset: Option<u64>,
    from_start: bool,
}

impl JsonlTailer {
    pub fn from_env() -> Result<Self> {
        let source = std::env::var("PM_SHADOW_JSONL_PATH")
            .or_else(|_| std::env::var("PM_SHADOW_FINAL_DIR"))
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("shadow-final"));
        let state_path = std::env::var("PM_SHADOW_TAIL_STATE_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("shadow_tail_state.json"));
        let follow = !env_truthy(&["PM_SHADOW_TAIL_NO_FOLLOW"]);
        let from_start = env_truthy(&["PM_SHADOW_TAIL_FROM_START"]);
        Ok(Self {
            source,
            follow,
            state_path,
            initial_offset: None,
            from_start,
        })
    }

    pub fn with_path(mut self, path: PathBuf) -> Self {
        self.source = path;
        self
    }

    pub fn with_follow(mut self, follow: bool) -> Self {
        self.follow = follow;
        self
    }

    pub fn with_offset(mut self, offset: u64) -> Self {
        self.initial_offset = Some(offset);
        self
    }

    pub fn with_from_start(mut self, from_start: bool) -> Self {
        self.from_start = from_start;
        self
    }

    fn load_state(&self) -> TailState {
        std::fs::read_to_string(&self.state_path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    fn persist_state(&self, state: &TailState) -> Result<()> {
        let json = serde_json::to_string(state)?;
        std::fs::write(&self.state_path, json)?;
        Ok(())
    }

    fn resolve_active_path(&self) -> Result<PathBuf> {
        if self.source.is_file() {
            return Ok(self.source.clone());
        }
        if self.source.is_dir() {
            return latest_shadow_jsonl(&self.source);
        }
        anyhow::bail!(
            "PM_SHADOW_JSONL_PATH not found: {}",
            self.source.display()
        );
    }

    pub async fn run(
        self,
        intent_tx: UnboundedSender<ExecIntent>,
        parity: Option<SharedParityGate>,
    ) -> Result<()> {
        let gamma = GammaResolver::new();
        let mut state = self.load_state();
        let mut seen: HashSet<DedupKey> = state
            .seen
            .iter()
            .filter_map(|s| {
                let mut parts = s.splitn(3, '|');
                Some(DedupKey {
                    slug: parts.next()?.to_string(),
                    side: parts.next()?.to_string(),
                    clip: parts.next()?.parse().ok()?,
                })
            })
            .collect();

        let mut active = self.resolve_active_path()?;
        let mut offset = if self.from_start {
            0
        } else if let Some(off) = self.initial_offset {
            off
        } else if state.path == active.to_string_lossy() {
            state.offset
        } else {
            0
        };

        info!(
            path = %active.display(),
            offset,
            follow = self.follow,
            seen = seen.len(),
            "shadow_jsonl tail start"
        );

        loop {
            if self.source.is_dir() {
                if let Ok(latest) = latest_shadow_jsonl(&self.source) {
                    if latest != active {
                        info!(
                            old = %active.display(),
                            new = %latest.display(),
                            "shadow jsonl day roll"
                        );
                        active = latest;
                        offset = 0;
                    }
                }
            }

            match tail_once(&active, offset, &mut seen, &intent_tx, &gamma, parity.as_ref()).await {
                Ok(TailBatch {
                    new_offset,
                    emitted,
                    lines,
                }) => {
                    if lines > 0 {
                        offset = new_offset;
                        state.path = active.to_string_lossy().into_owned();
                        state.offset = offset;
                        for key in &seen {
                            state.insert(key);
                        }
                        if let Err(e) = self.persist_state(&state) {
                            warn!(error = %e, "tail state persist failed");
                        }
                        if emitted > 0 {
                            debug!(emitted, offset, "tail batch");
                        }
                    }
                }
                Err(e) => {
                    warn!(path = %active.display(), error = %e, "tail read error");
                }
            }

            if !self.follow {
                break;
            }
            tokio::time::sleep(Duration::from_millis(POLL_MS)).await;
        }

        Ok(())
    }
}

struct TailBatch {
    new_offset: u64,
    emitted: usize,
    lines: usize,
}

async fn tail_once(
    path: &Path,
    offset: u64,
    seen: &mut HashSet<DedupKey>,
    intent_tx: &UnboundedSender<ExecIntent>,
    gamma: &GammaResolver,
    parity: Option<&SharedParityGate>,
) -> Result<TailBatch> {
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("open {}", path.display()))?;
    let len = file.metadata()?.len();
    if offset > len {
        return Ok(TailBatch {
            new_offset: len,
            emitted: 0,
            lines: 0,
        });
    }
    file.seek(SeekFrom::Start(offset))?;

    let mut raw = Vec::new();
    file.read_to_end(&mut raw)?;
    let text = String::from_utf8_lossy(&raw);
    // Only commit through the last complete newline so a partial trailing line
    // is retried on the next poll (writer may still be flushing).
    let complete_end = text
        .rfind('\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let new_offset = offset + complete_end as u64;
    let chunk = &text[..complete_end];

    let mut emitted = 0usize;
    let mut lines = 0usize;
    for line in chunk.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        lines += 1;
        let row: WouldEnterRow = match serde_json::from_str(line) {
            Ok(row) => row,
            Err(_) => continue,
        };
        let ref_ts_s = parse_ts_utc(&row.ts_utc).unwrap_or_else(now_unix_s);
        let Some(mut intent) = would_enter_to_intent(row) else {
            continue;
        };
        let key = DedupKey {
            slug: intent.slug.clone(),
            side: intent.side.clone(),
            clip: intent.clip,
        };
        if seen.contains(&key) {
            continue;
        }
        if let Err(error) = enrich_intent(&mut intent, gamma).await {
            warn!(slug = %intent.slug, error = %error, "gamma enrich failed; skip");
            continue;
        }
        if intent.token_id.is_empty() {
            warn!(slug = %intent.slug, "would_enter still missing token_id; skip");
            continue;
        }
        seen.insert(key);
        if let Some(gate) = parity {
            gate.lock()
                .expect("parity gate poisoned")
                .record_would_enter(&intent.slug, &intent.side, intent.clip, ref_ts_s);
        }
        intent_tx
            .send(intent)
            .map_err(|_| anyhow::anyhow!("intent channel closed"))?;
        emitted += 1;
    }

    Ok(TailBatch {
        new_offset,
        emitted,
        lines,
    })
}

fn env_truthy(names: &[&str]) -> bool {
    names.iter().any(|name| {
        std::env::var(name)
            .ok()
            .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes"))
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_would_enter_maps_exec_fields() {
        let line = r#"{"type":"would_enter","ts_utc":"2026-06-16T10:20:01Z","slug":"btc-updown-5m-abc","side":"down","p_exo":0.319,"p_side":0.681,"touch_price":0.51,"touch_size":100.0,"edge":0.171,"marketable_limit_price":0.68,"strike":99000.0,"strike_source":"pm","sigma_bar_bps":4.2,"lane":"fade","clip":1,"token_id":"tok123","target_notional":50.0,"close_ts_s":1718535600,"condition_id":"cond1","up_index_set":1,"down_index_set":2}"#;
        let intent = parse_would_enter_line(line).expect("parse");
        assert_eq!(intent.slug, "btc-updown-5m-abc");
        assert_eq!(intent.side, "down");
        assert_eq!(intent.token_id, "tok123");
        assert!((intent.p_side - 0.681).abs() < 1e-9);
        assert!((intent.marketable_limit_price - 0.68).abs() < 1e-9);
        assert_eq!(intent.clip, 1);
        assert!(intent.hold_to_redemption);
    }

    #[tokio::test]
    async fn dedup_skips_replayed_clip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shadow-20260616.jsonl");
        let line = r#"{"type":"would_enter","slug":"s1","side":"up","p_exo":0.6,"touch_price":0.5,"edge":0.1,"clip":1,"token_id":"t1","close_ts_s":1}"#;
        std::fs::write(&path, format!("{line}\n{line}\n")).unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut seen = HashSet::new();
        let gamma = GammaResolver::new();
        let b1 = tail_once(&path, 0, &mut seen, &tx, &gamma, None).await.unwrap();
        assert_eq!(b1.emitted, 1);
        let _ = rx.try_recv().unwrap();
        let b2 = tail_once(&path, 0, &mut seen, &tx, &gamma, None).await.unwrap();
        assert_eq!(b2.emitted, 0);
    }
}