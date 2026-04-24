//! Audit JSONL writer with rotation for deterministic post-run forensic analysis.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use crate::runtime::{Runtime, RuntimeOutcome};
use crate::strategy::StrategyMode;

pub(super) struct AuditWriter {
    path: PathBuf,
    file: File,
    last_seq: u64,
    rotate_bytes: Option<u64>,
    current_size_bytes: u64,
}

impl AuditWriter {
    pub(super) fn open(path: &Path, rotate_bytes: Option<u64>) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("failed to create audit directory {}", parent.display())
            })?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("failed to open audit path {}", path.display()))?;
        let current_size_bytes = file
            .metadata()
            .with_context(|| format!("failed to stat audit path {}", path.display()))?
            .len();
        Ok(Self {
            path: path.to_path_buf(),
            file,
            last_seq: 0,
            rotate_bytes,
            current_size_bytes,
        })
    }

    pub(super) fn append_outcome(
        &mut self,
        source: &str,
        runtime: &Runtime<StrategyMode>,
        outcome: &RuntimeOutcome,
    ) -> Result<()> {
        let from_seq = outcome
            .event_seqs
            .first()
            .copied()
            .unwrap_or(self.last_seq.saturating_add(1));
        let after_seq = from_seq.saturating_sub(1).max(self.last_seq);
        for record in runtime.event_log().snapshot_since(after_seq) {
            let row = serde_json::json!({
                "kind": "event",
                "source": source,
                "run_id": runtime.run_id(),
                "record": record,
            });
            self.append_json_line(&row)?;
            self.last_seq = self.last_seq.max(record.seq);
        }
        for command in &outcome.commands {
            let row = serde_json::json!({
                "kind": "command",
                "source": source,
                "run_id": runtime.run_id(),
                "command": command,
            });
            self.append_json_line(&row)?;
        }
        self.file.flush()?;
        Ok(())
    }

    fn append_json_line(&mut self, value: &serde_json::Value) -> Result<()> {
        self.rotate_if_needed()?;
        let line = serde_json::to_string(value)?;
        self.file
            .write_all(line.as_bytes())
            .with_context(|| format!("failed to write audit line {}", self.path.display()))?;
        self.file
            .write_all(b"\n")
            .with_context(|| format!("failed to write audit newline {}", self.path.display()))?;
        self.current_size_bytes = self
            .current_size_bytes
            .saturating_add(line.len() as u64)
            .saturating_add(1);
        Ok(())
    }

    fn rotate_if_needed(&mut self) -> Result<()> {
        let Some(limit_bytes) = self.rotate_bytes else {
            return Ok(());
        };
        if self.current_size_bytes < limit_bytes {
            return Ok(());
        }
        self.file
            .flush()
            .with_context(|| format!("failed to flush audit path {}", self.path.display()))?;
        let rotated_path = rotated_audit_path(&self.path);
        std::fs::rename(&self.path, &rotated_path).with_context(|| {
            format!(
                "failed to rotate audit {} -> {}",
                self.path.display(),
                rotated_path.display()
            )
        })?;
        self.file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.path)
            .with_context(|| format!("failed to reopen audit path {}", self.path.display()))?;
        self.current_size_bytes = 0;
        Ok(())
    }
}

fn rotated_audit_path(path: &Path) -> PathBuf {
    let ts = now_unix_ms() / 1_000;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("audit");
    let ext = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("jsonl");
    parent.join(format!("{stem}.{ts}.{ext}"))
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}
