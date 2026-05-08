use crate::replay::tape::format::{
    as_bytes, header_for, one_as_bytes, validate_host_layout, validate_monotonic, TapeRecord,
};
use anyhow::{Context, Result};
use std::fs::{rename, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

pub fn write_tape_file<T: TapeRecord>(
    output_path: impl AsRef<Path>,
    market_id: &str,
    records: &[T],
) -> Result<PathBuf> {
    validate_host_layout()?;
    validate_monotonic(records)?;

    let output_path = output_path.as_ref();
    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!("creating tape output directory {}", parent.display())
        })?;
    }

    let tmp_path = output_path.with_extension("bin.tmp");
    let file = File::create(&tmp_path)
        .with_context(|| format!("creating temp tape file {}", tmp_path.display()))?;
    let mut writer = BufWriter::with_capacity(8 * 1024 * 1024, file);

    let header = header_for(market_id, records);
    writer
        .write_all(one_as_bytes(&header))
        .with_context(|| format!("writing tape header {}", tmp_path.display()))?;
    writer
        .write_all(as_bytes(records))
        .with_context(|| format!("writing tape records {}", tmp_path.display()))?;
    writer
        .flush()
        .with_context(|| format!("flushing tape file {}", tmp_path.display()))?;
    drop(writer);

    OpenOptions::new()
        .read(true)
        .open(&tmp_path)
        .and_then(|file| file.sync_all())
        .with_context(|| format!("syncing tape file {}", tmp_path.display()))?;

    rename(&tmp_path, output_path).with_context(|| {
        format!(
            "renaming complete tape file {} to {}",
            tmp_path.display(),
            output_path.display()
        )
    })?;
    Ok(output_path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replay::tape::format::{BookEventV1, EVENT_UPDATE, LEG_YES, SIDE_BID};

    #[test]
    fn rejects_out_of_order_records() {
        let records = [
            BookEventV1 {
                ts_ns: 2,
                price_ticks: 5_000,
                size_lots: 100,
                leg: LEG_YES,
                side: SIDE_BID,
                event_type: EVENT_UPDATE,
                _pad: [0; 5],
            },
            BookEventV1 {
                ts_ns: 1,
                price_ticks: 5_000,
                size_lots: 100,
                leg: LEG_YES,
                side: SIDE_BID,
                event_type: EVENT_UPDATE,
                _pad: [0; 5],
            },
        ];
        assert!(write_tape_file("/tmp/ignored-tape-test.bin", "m", &records).is_err());
    }
}

