//! Durable JSONL journal writer for runtime events, commands, and checkpoints.

use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::event_log::EventRecord;
use crate::runtime::RuntimeCheckpoint;
use crate::types::RuntimeCommand;

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum JournalLine<'a> {
    RuntimeEvent {
        record: &'a EventRecord,
    },
    RuntimeCommand {
        command: &'a RuntimeCommand,
    },
    RuntimeCheckpoint {
        observed_at_ms: u64,
        run_id: &'a str,
        name: &'a str,
        open_orders: usize,
        needs_reconcile_orders: usize,
        event_seq_checkpoint: u64,
    },
    RuntimeReplayCheckpoint {
        checkpoint: &'a RuntimeCheckpoint,
    },
}

pub struct JournalWriter {
    path: PathBuf,
    writer: BufWriter<File>,
    rotate_bytes: Option<u64>,
    current_size_bytes: u64,
}

impl JournalWriter {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        Self::open_with_rotation(path, None)
    }

    pub fn open_with_rotation(path: impl Into<PathBuf>, rotate_bytes: Option<u64>) -> Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create journal parent dir {}", parent.display())
            })?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("failed to open journal {}", path.display()))?;
        let current_size_bytes = file
            .metadata()
            .with_context(|| format!("failed to stat journal {}", path.display()))?
            .len();
        Ok(Self {
            path,
            writer: BufWriter::new(file),
            rotate_bytes,
            current_size_bytes,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append_event(&mut self, record: &EventRecord) -> Result<()> {
        self.append_line(&JournalLine::RuntimeEvent { record })
    }

    pub fn append_command(&mut self, command: &RuntimeCommand) -> Result<()> {
        self.append_line(&JournalLine::RuntimeCommand { command })
    }

    pub fn append_checkpoint(
        &mut self,
        observed_at_ms: u64,
        run_id: &str,
        name: &str,
        open_orders: usize,
        needs_reconcile_orders: usize,
        event_seq_checkpoint: u64,
    ) -> Result<()> {
        self.append_line(&JournalLine::RuntimeCheckpoint {
            observed_at_ms,
            run_id,
            name,
            open_orders,
            needs_reconcile_orders,
            event_seq_checkpoint,
        })
    }

    pub fn append_runtime_checkpoint(&mut self, checkpoint: &RuntimeCheckpoint) -> Result<()> {
        self.append_line(&JournalLine::RuntimeReplayCheckpoint { checkpoint })
    }

    pub fn flush(&mut self) -> Result<()> {
        self.writer
            .flush()
            .with_context(|| format!("failed to flush journal {}", self.path.display()))
    }

    fn append_line(&mut self, line: &JournalLine<'_>) -> Result<()> {
        self.rotate_if_needed()?;
        serde_json::to_writer(&mut self.writer, line).with_context(|| {
            format!(
                "failed to serialize journal line to {}",
                self.path.display()
            )
        })?;
        self.writer
            .write_all(b"\n")
            .with_context(|| format!("failed to append newline to {}", self.path.display()))?;
        self.current_size_bytes = self.current_size_bytes.saturating_add(1);
        Ok(())
    }

    fn rotate_if_needed(&mut self) -> Result<()> {
        let Some(limit_bytes) = self.rotate_bytes else {
            return Ok(());
        };
        if self.current_size_bytes < limit_bytes {
            return Ok(());
        }

        self.writer
            .flush()
            .with_context(|| format!("failed to flush journal {}", self.path.display()))?;

        let rotated_path = rotated_journal_path(&self.path);
        fs::rename(&self.path, &rotated_path).with_context(|| {
            format!(
                "failed to rotate journal {} -> {}",
                self.path.display(),
                rotated_path.display()
            )
        })?;

        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.path)
            .with_context(|| format!("failed to reopen rotated journal {}", self.path.display()))?;
        self.writer = BufWriter::new(file);
        self.current_size_bytes = 0;
        Ok(())
    }
}

fn rotated_journal_path(path: &Path) -> PathBuf {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("journal");
    let ext = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("jsonl");
    parent.join(format!("{stem}.{ts}.{ext}"))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::event_log::{EventCategory, EventRecord};
    use crate::types::{
        ClientOrderId, InstrumentId, MarketId, OrderIntent, RuntimeCommand, TradeSide,
    };

    use super::JournalWriter;

    #[test]
    fn writes_event_and_command_jsonl() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("whale-pair-journal-{unique}.jsonl"));

        let mut journal = JournalWriter::open(&path).unwrap();
        journal
            .append_event(&EventRecord::new(EventCategory::Runtime, 1, "started"))
            .unwrap();
        journal
            .append_command(&RuntimeCommand::Submit(OrderIntent {
                client_order_id: ClientOrderId::from("coid-1"),
                market_id: MarketId::from("market-1"),
                instrument_id: InstrumentId::from("asset-1"),
                side: TradeSide::Buy,
                limit_price: 0.49,
                quantity: 10.0,
                reduce_only: false,
                reason: "test".to_string(),
                quote_level_tag: None,
                created_at_ms: 2,
            }))
            .unwrap();
        journal.flush().unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        let _ = fs::remove_file(&path);
        let lines = contents.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"kind\":\"runtime_event\""));
        assert!(lines[1].contains("\"kind\":\"runtime_command\""));
        assert!(lines[1].contains("\"client_order_id\":\"coid-1\""));
    }

    #[test]
    fn writes_runtime_checkpoint_jsonl() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("whale-pair-journal-checkpoint-{unique}.jsonl"));

        let mut journal = JournalWriter::open(&path).unwrap();
        journal
            .append_checkpoint(1, "run-1", "startup", 3, 1, 17)
            .unwrap();
        journal.flush().unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        let _ = fs::remove_file(&path);
        let lines = contents.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("\"kind\":\"runtime_checkpoint\""));
        assert!(lines[0].contains("\"run_id\":\"run-1\""));
    }

    #[test]
    fn rotates_journal_when_size_limit_hit() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("whale-pair-journal-rotate-{unique}"));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("journal.jsonl");

        let mut journal = JournalWriter::open_with_rotation(&path, Some(1)).unwrap();
        journal
            .append_event(&EventRecord::new(EventCategory::Runtime, 1, "started"))
            .unwrap();
        journal
            .append_event(&EventRecord::new(EventCategory::Runtime, 2, "second"))
            .unwrap();
        journal.flush().unwrap();

        let rotated = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|entry| {
                entry
                    .file_name()
                    .and_then(|v| v.to_str())
                    .unwrap_or("")
                    .starts_with("journal.")
            })
            .collect::<Vec<_>>();
        assert!(!rotated.is_empty());

        let active_contents = fs::read_to_string(&path).unwrap();
        assert!(active_contents.contains("\"second\""));

        let _ = fs::remove_dir_all(&dir);
    }
}
