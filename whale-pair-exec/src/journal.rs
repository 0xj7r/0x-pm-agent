use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::event_log::EventRecord;
use crate::types::RuntimeCommand;

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum JournalLine<'a> {
    RuntimeEvent { record: &'a EventRecord },
    RuntimeCommand { command: &'a RuntimeCommand },
}

pub struct JournalWriter {
    path: PathBuf,
    writer: BufWriter<File>,
}

impl JournalWriter {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
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
        Ok(Self {
            path,
            writer: BufWriter::new(file),
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

    pub fn flush(&mut self) -> Result<()> {
        self.writer
            .flush()
            .with_context(|| format!("failed to flush journal {}", self.path.display()))
    }

    fn append_line(&mut self, line: &JournalLine<'_>) -> Result<()> {
        serde_json::to_writer(&mut self.writer, line).with_context(|| {
            format!("failed to serialize journal line to {}", self.path.display())
        })?;
        self.writer
            .write_all(b"\n")
            .with_context(|| format!("failed to append newline to {}", self.path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::event_log::{EventCategory, EventRecord};
    use crate::types::{ClientOrderId, InstrumentId, MarketId, OrderIntent, RuntimeCommand, TradeSide};

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
}
