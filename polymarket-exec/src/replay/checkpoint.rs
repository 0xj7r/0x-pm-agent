//! Replay state checkpoint + resume facility.
//!
//! A `ReplayCheckpoint` is a serializable snapshot of the full replay system
//! state at a moment in event time. Combined with `CheckpointWriter` and
//! `CheckpointReader`, it allows reruns of a replay window starting from an
//! arbitrary mid-stream point rather than from the beginning of the input
//! corpus.
//!
//! Format on disk:
//!
//!   magic (4 bytes, b"PMCK")
//!   schema_version (u32 LE)
//!   payload_len (u64 LE)
//!   payload (bincode-encoded `ReplayCheckpoint`)
//!
//! The framing header lets future schema bumps surface as a typed
//! `CheckpointError::SchemaMismatch` rather than a deserialization panic.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const MAGIC: &[u8; 4] = b"PMCK";
const HEADER_LEN: usize = 4 + 4 + 8;

pub const CHECKPOINT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug)]
pub enum CheckpointError {
    Io(std::io::Error),
    Serialize(String),
    Deserialize(String),
    BadMagic,
    SchemaMismatch { expected: u32, found: u32 },
    InvalidFilename(String),
}

impl std::fmt::Display for CheckpointError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CheckpointError::Io(e) => write!(f, "checkpoint io error: {e}"),
            CheckpointError::Serialize(e) => write!(f, "checkpoint serialize error: {e}"),
            CheckpointError::Deserialize(e) => write!(f, "checkpoint deserialize error: {e}"),
            CheckpointError::BadMagic => write!(f, "checkpoint bad magic header"),
            CheckpointError::SchemaMismatch { expected, found } => write!(
                f,
                "checkpoint schema mismatch: expected v{expected}, found v{found}"
            ),
            CheckpointError::InvalidFilename(s) => write!(f, "checkpoint invalid filename: {s}"),
        }
    }
}

impl std::error::Error for CheckpointError {}

impl From<std::io::Error> for CheckpointError {
    fn from(e: std::io::Error) -> Self {
        CheckpointError::Io(e)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Position {
    pub market: String,
    pub asset_id: String,
    pub qty: f64,
    pub avg_cost: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenOrder {
    pub id: String,
    pub market: String,
    pub asset_id: String,
    pub side: String,
    pub price: f64,
    pub remaining_size: f64,
    pub reason_tag: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BookSnapshot {
    pub market: String,
    pub asset_id: String,
    pub side: String,
    pub levels: Vec<(f64, f64)>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AccountingState {
    pub realized_usd: f64,
    pub fees_usd: f64,
    pub gas_usd: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayCheckpoint {
    pub at_event_ts_ns: u64,
    pub schema_version: u32,
    pub positions: Vec<Position>,
    pub open_orders: Vec<OpenOrder>,
    pub book_state: Vec<BookSnapshot>,
    pub strategy_state: Vec<u8>,
    pub accounting: AccountingState,
}

impl ReplayCheckpoint {
    pub fn new(at_event_ts_ns: u64) -> Self {
        Self {
            at_event_ts_ns,
            schema_version: CHECKPOINT_SCHEMA_VERSION,
            positions: Vec::new(),
            open_orders: Vec::new(),
            book_state: Vec::new(),
            strategy_state: Vec::new(),
            accounting: AccountingState::default(),
        }
    }
}

/// Strategies/engines that want their internal state captured implement this.
/// `snapshot` returns an opaque blob; `restore` consumes one. The replay
/// engine treats the blob as a black box; only the producing component
/// understands its layout.
pub trait Checkpointable {
    fn snapshot(&self) -> Vec<u8>;
    fn restore(&mut self, blob: &[u8]) -> Result<(), CheckpointError>;
}

pub struct CheckpointWriter {
    out_dir: PathBuf,
}

static WRITE_NONCE: AtomicU64 = AtomicU64::new(0);

impl CheckpointWriter {
    pub fn new(out_dir: PathBuf) -> Self {
        Self { out_dir }
    }

    pub fn write(&self, ckpt: &ReplayCheckpoint) -> Result<PathBuf, CheckpointError> {
        fs::create_dir_all(&self.out_dir)?;

        let payload = bincode::serialize(ckpt)
            .map_err(|e| CheckpointError::Serialize(e.to_string()))?;

        let nonce_seq = WRITE_NONCE.fetch_add(1, Ordering::Relaxed);
        let wall_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let nonce = wall_ns ^ nonce_seq.wrapping_mul(0x9E37_79B9_7F4A_7C15);

        let filename = format!(
            "ckpt-{:020}-{:016x}.bin",
            ckpt.at_event_ts_ns, nonce,
        );
        let path = self.out_dir.join(filename);

        let tmp = path.with_extension("bin.tmp");
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(MAGIC)?;
            f.write_all(&ckpt.schema_version.to_le_bytes())?;
            f.write_all(&(payload.len() as u64).to_le_bytes())?;
            f.write_all(&payload)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &path)?;

        Ok(path)
    }
}

pub struct CheckpointReader;

impl CheckpointReader {
    pub fn list(dir: &Path) -> Result<Vec<PathBuf>, CheckpointError> {
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut entries: Vec<(u64, PathBuf)> = Vec::new();
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("bin") {
                continue;
            }
            let name = match path.file_name().and_then(|s| s.to_str()) {
                Some(n) => n,
                None => continue,
            };
            if !name.starts_with("ckpt-") {
                continue;
            }
            let ts = parse_ts_from_filename(name)?;
            entries.push((ts, path));
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        Ok(entries.into_iter().map(|(_, p)| p).collect())
    }

    pub fn load(path: &Path) -> Result<ReplayCheckpoint, CheckpointError> {
        let mut f = fs::File::open(path)?;
        let mut header = [0u8; HEADER_LEN];
        f.read_exact(&mut header)?;

        if &header[0..4] != MAGIC {
            return Err(CheckpointError::BadMagic);
        }
        let schema_version = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
        if schema_version != CHECKPOINT_SCHEMA_VERSION {
            return Err(CheckpointError::SchemaMismatch {
                expected: CHECKPOINT_SCHEMA_VERSION,
                found: schema_version,
            });
        }
        let payload_len = u64::from_le_bytes([
            header[8], header[9], header[10], header[11], header[12], header[13], header[14],
            header[15],
        ]) as usize;

        let mut payload = vec![0u8; payload_len];
        f.read_exact(&mut payload)?;

        let ckpt: ReplayCheckpoint = bincode::deserialize(&payload)
            .map_err(|e| CheckpointError::Deserialize(e.to_string()))?;

        Ok(ckpt)
    }

    pub fn nearest_before(
        dir: &Path,
        ts_ns: u64,
    ) -> Result<Option<ReplayCheckpoint>, CheckpointError> {
        let paths = Self::list(dir)?;
        let mut best: Option<PathBuf> = None;
        for p in paths {
            let name = match p.file_name().and_then(|s| s.to_str()) {
                Some(n) => n,
                None => continue,
            };
            let ts = parse_ts_from_filename(name)?;
            if ts <= ts_ns {
                best = Some(p);
            } else {
                break;
            }
        }
        match best {
            Some(p) => Ok(Some(Self::load(&p)?)),
            None => Ok(None),
        }
    }
}

fn parse_ts_from_filename(name: &str) -> Result<u64, CheckpointError> {
    // expected: ckpt-<20-digit ts>-<hex>.bin
    let stripped = name
        .strip_prefix("ckpt-")
        .ok_or_else(|| CheckpointError::InvalidFilename(name.to_string()))?;
    let ts_str = stripped
        .split('-')
        .next()
        .ok_or_else(|| CheckpointError::InvalidFilename(name.to_string()))?;
    ts_str
        .parse::<u64>()
        .map_err(|_| CheckpointError::InvalidFilename(name.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn sample_ckpt(ts: u64) -> ReplayCheckpoint {
        ReplayCheckpoint {
            at_event_ts_ns: ts,
            schema_version: CHECKPOINT_SCHEMA_VERSION,
            positions: vec![Position {
                market: "m1".into(),
                asset_id: "a1".into(),
                qty: 12.5,
                avg_cost: 0.42,
            }],
            open_orders: vec![OpenOrder {
                id: "o1".into(),
                market: "m1".into(),
                asset_id: "a1".into(),
                side: "BUY".into(),
                price: 0.41,
                remaining_size: 5.0,
                reason_tag: "maker_quote".into(),
            }],
            book_state: vec![BookSnapshot {
                market: "m1".into(),
                asset_id: "a1".into(),
                side: "BID".into(),
                levels: vec![(0.41, 100.0), (0.40, 250.0)],
            }],
            strategy_state: vec![1, 2, 3, 4, 5],
            accounting: AccountingState {
                realized_usd: 17.25,
                fees_usd: 0.13,
                gas_usd: 0.04,
            },
        }
    }

    #[test]
    fn round_trip_full_checkpoint() {
        let dir = tempdir().unwrap();
        let writer = CheckpointWriter::new(dir.path().to_path_buf());
        let original = sample_ckpt(1_700_000_000_000_000_000);
        let path = writer.write(&original).unwrap();
        let loaded = CheckpointReader::load(&path).unwrap();
        assert_eq!(original, loaded);
    }

    #[test]
    fn nearest_before_finds_most_recent_at_or_before() {
        let dir = tempdir().unwrap();
        let writer = CheckpointWriter::new(dir.path().to_path_buf());

        for ts in [1000u64, 2000, 3000, 5000] {
            writer.write(&sample_ckpt(ts)).unwrap();
        }

        let before_first = CheckpointReader::nearest_before(dir.path(), 500).unwrap();
        assert!(before_first.is_none());

        let exact = CheckpointReader::nearest_before(dir.path(), 3000)
            .unwrap()
            .unwrap();
        assert_eq!(exact.at_event_ts_ns, 3000);

        let between = CheckpointReader::nearest_before(dir.path(), 4500)
            .unwrap()
            .unwrap();
        assert_eq!(between.at_event_ts_ns, 3000);

        let after_last = CheckpointReader::nearest_before(dir.path(), 9999)
            .unwrap()
            .unwrap();
        assert_eq!(after_last.at_event_ts_ns, 5000);
    }

    #[test]
    fn schema_mismatch_returns_typed_error() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ckpt-00000000000000000001-deadbeefdeadbeef.bin");

        let mut f = fs::File::create(&path).unwrap();
        f.write_all(MAGIC).unwrap();
        f.write_all(&999u32.to_le_bytes()).unwrap();
        f.write_all(&0u64.to_le_bytes()).unwrap();
        drop(f);

        let err = CheckpointReader::load(&path).unwrap_err();
        match err {
            CheckpointError::SchemaMismatch { expected, found } => {
                assert_eq!(expected, CHECKPOINT_SCHEMA_VERSION);
                assert_eq!(found, 999);
            }
            other => panic!("expected SchemaMismatch, got {other:?}"),
        }
    }

    #[test]
    fn empty_strategy_state_round_trips() {
        let dir = tempdir().unwrap();
        let writer = CheckpointWriter::new(dir.path().to_path_buf());
        let mut ckpt = sample_ckpt(42);
        ckpt.strategy_state = Vec::new();
        let path = writer.write(&ckpt).unwrap();
        let loaded = CheckpointReader::load(&path).unwrap();
        assert!(loaded.strategy_state.is_empty());
        assert_eq!(loaded, ckpt);
    }

    #[test]
    fn duplicate_event_ts_yields_distinct_filenames() {
        let dir = tempdir().unwrap();
        let writer = CheckpointWriter::new(dir.path().to_path_buf());
        let ckpt = sample_ckpt(123_456);
        let p1 = writer.write(&ckpt).unwrap();
        let p2 = writer.write(&ckpt).unwrap();
        let p3 = writer.write(&ckpt).unwrap();
        assert_ne!(p1, p2);
        assert_ne!(p2, p3);
        assert_ne!(p1, p3);
        assert!(p1.exists() && p2.exists() && p3.exists());
    }

    #[test]
    fn list_returns_checkpoints_sorted_ascending_by_event_ts() {
        let dir = tempdir().unwrap();
        let writer = CheckpointWriter::new(dir.path().to_path_buf());
        for ts in [5000u64, 1000, 3000, 2000, 4000] {
            writer.write(&sample_ckpt(ts)).unwrap();
        }
        let listed = CheckpointReader::list(dir.path()).unwrap();
        let timestamps: Vec<u64> = listed
            .iter()
            .map(|p| parse_ts_from_filename(p.file_name().unwrap().to_str().unwrap()).unwrap())
            .collect();
        assert_eq!(timestamps, vec![1000, 2000, 3000, 4000, 5000]);
    }

    #[test]
    fn list_on_missing_dir_returns_empty() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        let listed = CheckpointReader::list(&missing).unwrap();
        assert!(listed.is_empty());
    }

    #[test]
    fn nearest_before_on_empty_dir_returns_none() {
        let dir = tempdir().unwrap();
        let result = CheckpointReader::nearest_before(dir.path(), 9999).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn bad_magic_returns_typed_error() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ckpt-00000000000000000001-cafef00dcafef00d.bin");
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(b"XXXX").unwrap();
        f.write_all(&CHECKPOINT_SCHEMA_VERSION.to_le_bytes()).unwrap();
        f.write_all(&0u64.to_le_bytes()).unwrap();
        drop(f);
        match CheckpointReader::load(&path).unwrap_err() {
            CheckpointError::BadMagic => {}
            other => panic!("expected BadMagic, got {other:?}"),
        }
    }

    struct DummyStrategy {
        cursor: u64,
    }

    impl Checkpointable for DummyStrategy {
        fn snapshot(&self) -> Vec<u8> {
            self.cursor.to_le_bytes().to_vec()
        }
        fn restore(&mut self, blob: &[u8]) -> Result<(), CheckpointError> {
            if blob.len() != 8 {
                return Err(CheckpointError::Deserialize(format!(
                    "expected 8 bytes, got {}",
                    blob.len()
                )));
            }
            let mut buf = [0u8; 8];
            buf.copy_from_slice(blob);
            self.cursor = u64::from_le_bytes(buf);
            Ok(())
        }
    }

    #[test]
    fn checkpointable_trait_round_trips_strategy_blob() {
        let mut s = DummyStrategy { cursor: 7777 };
        let blob = s.snapshot();
        s.cursor = 0;
        s.restore(&blob).unwrap();
        assert_eq!(s.cursor, 7777);
    }
}
