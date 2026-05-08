use crate::replay::tape::format::{
    records_from_bytes, validate_header, validate_host_layout, TapeHeaderV1, TapeRecord,
    TAPE_HEADER_SIZE,
};
use anyhow::{ensure, Context, Result};
use std::fs::File;
use std::marker::PhantomData;
use std::mem::{align_of, size_of};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::ptr::NonNull;
use std::slice;

pub struct MappedTape<T: TapeRecord> {
    mmap: MmapRegion,
    header: TapeHeaderV1,
    _record: PhantomData<T>,
}

impl<T: TapeRecord> MappedTape<T> {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        validate_host_layout()?;

        let path = path.as_ref();
        let file = File::open(path).with_context(|| format!("opening tape {}", path.display()))?;
        let len = file
            .metadata()
            .with_context(|| format!("stat tape {}", path.display()))?
            .len() as usize;
        ensure!(
            len >= TAPE_HEADER_SIZE as usize,
            "tape file is smaller than header"
        );

        let mmap = MmapRegion::map_read_only(&file, len)
            .with_context(|| format!("mmap tape {}", path.display()))?;
        let header = read_header(mmap.as_slice())?;
        validate_header::<T>(&header)?;

        let expected_len =
            TAPE_HEADER_SIZE as usize + header.record_count as usize * size_of::<T>();
        ensure!(
            len == expected_len,
            "tape file length does not match header record count"
        );

        let records_start = mmap.as_slice()[TAPE_HEADER_SIZE as usize..].as_ptr() as usize;
        ensure!(
            records_start % align_of::<T>() == 0,
            "tape records are not aligned for requested type"
        );

        Ok(Self {
            mmap,
            header,
            _record: PhantomData,
        })
    }

    pub fn header(&self) -> &TapeHeaderV1 {
        &self.header
    }

    pub fn records(&self) -> &[T] {
        let bytes = &self.mmap.as_slice()[TAPE_HEADER_SIZE as usize..];
        unsafe { records_from_bytes::<T>(bytes) }
    }

    pub fn seek_ts(&self, ts_ns: u64) -> usize {
        self.records().partition_point(|record| record.ts_ns() < ts_ns)
    }
}

struct MmapRegion {
    ptr: NonNull<u8>,
    len: usize,
}

impl MmapRegion {
    fn map_read_only(file: &File, len: usize) -> Result<Self> {
        ensure!(len > 0, "cannot mmap empty file");
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                file.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error()).context("mmap failed");
        }

        #[cfg(any(target_os = "linux", target_os = "macos"))]
        unsafe {
            let _ = libc::madvise(ptr, len, libc::MADV_SEQUENTIAL);
        }

        let ptr = NonNull::new(ptr.cast::<u8>()).context("mmap returned null")?;
        Ok(Self { ptr, len })
    }

    fn as_slice(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for MmapRegion {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr.as_ptr().cast(), self.len);
        }
    }
}

fn read_header(bytes: &[u8]) -> Result<TapeHeaderV1> {
    let header_bytes = &bytes[..TAPE_HEADER_SIZE as usize];
    let header_ptr = header_bytes.as_ptr();
    ensure!(
        (header_ptr as usize) % align_of::<TapeHeaderV1>() == 0,
        "tape header is not aligned"
    );
    Ok(unsafe { *(header_ptr.cast::<TapeHeaderV1>()) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replay::tape::format::{BookEventV1, EVENT_UPDATE, LEG_YES, SIDE_BID};
    use crate::replay::tape::writer::write_tape_file;

    #[test]
    fn reads_records_with_seek() {
        let path = std::env::temp_dir().join("mapped-tape-reader-test.bin");
        let records = [
            BookEventV1 {
                ts_ns: 10,
                price_ticks: 5_000,
                size_lots: 100,
                leg: LEG_YES,
                side: SIDE_BID,
                event_type: EVENT_UPDATE,
                _pad: [0; 5],
            },
            BookEventV1 {
                ts_ns: 20,
                price_ticks: 5_100,
                size_lots: 200,
                leg: LEG_YES,
                side: SIDE_BID,
                event_type: EVENT_UPDATE,
                _pad: [0; 5],
            },
        ];
        write_tape_file(&path, "market", &records).unwrap();

        let tape = MappedTape::<BookEventV1>::open(&path).unwrap();
        assert_eq!(tape.header().record_count, 2);
        assert_eq!(tape.records(), records);
        assert_eq!(tape.seek_ts(15), 1);

        let _ = std::fs::remove_file(path);
    }
}

