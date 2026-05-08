//! Packed binary replay tapes for fast local backtests.
//!
//! The hot replay path should not parse Parquet. It should read fixed-size,
//! timestamp-sorted binary records via mmap and feed the strategy engine from
//! cache-friendly slices. Parquet remains the interchange/cold-storage format;
//! tape files are the local execution format.

pub mod book_state;
pub mod convert;
pub mod events;
pub mod format;
pub mod reader;
pub mod smoke;
pub mod writer;
