use anyhow::{bail, ensure, Result};
use sha2::{Digest, Sha256};
use std::mem::{align_of, size_of};
use std::slice;

pub const TAPE_MAGIC: [u8; 8] = *b"PMTAPE01";
pub const TAPE_VERSION: u32 = 1;
pub const TAPE_HEADER_SIZE: u32 = 64;

pub const RECORD_KIND_BOOK: u32 = 1;
pub const RECORD_KIND_TRADE: u32 = 2;
pub const RECORD_KIND_BTC: u32 = 3;

pub const SIDE_BID: u8 = 0;
pub const SIDE_ASK: u8 = 1;

pub const LEG_YES: u8 = 0;
pub const LEG_NO: u8 = 1;

pub const EVENT_UPDATE: u8 = 0;
pub const EVENT_DELETE: u8 = 1;
pub const EVENT_SNAPSHOT_START: u8 = 2;
pub const EVENT_SNAPSHOT_END: u8 = 3;

pub const TAKER_BUY: u8 = 0;
pub const TAKER_SELL: u8 = 1;

pub const PRICE_SCALE: f64 = 10_000.0;
pub const SIZE_SCALE: f64 = 100.0;

#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct TapeHeaderV1 {
    pub magic: [u8; 8],
    pub version: u32,
    pub record_kind: u32,
    pub record_size: u32,
    pub header_size: u32,
    pub record_count: u64,
    pub ts_min_ns: u64,
    pub ts_max_ns: u64,
    pub market_id_hash: u64,
    pub flags: u32,
    pub _reserved: [u8; 4],
}

#[repr(C)]
#[derive(Debug, Copy, Clone, Default, PartialEq, Eq)]
pub struct BookEventV1 {
    pub ts_ns: u64,
    pub price_ticks: u32,
    pub size_lots: u32,
    pub leg: u8,
    pub side: u8,
    pub event_type: u8,
    pub _pad: [u8; 5],
}

#[repr(C)]
#[derive(Debug, Copy, Clone, Default, PartialEq, Eq)]
pub struct TradeEventV1 {
    pub ts_ns: u64,
    pub price_ticks: u32,
    pub size_lots: u32,
    pub leg: u8,
    pub taker_side: u8,
    pub _pad: [u8; 6],
}

#[repr(C)]
#[derive(Debug, Copy, Clone, Default, PartialEq, Eq)]
pub struct BtcTickV1 {
    pub ts_ns: u64,
    pub price_cents: u64,
    pub qty_lots: u32,
    pub _pad: [u8; 4],
}

/// Marker for fixed-layout tape records.
///
/// # Safety
///
/// Implementors must be plain old data: `Copy`, `repr(C)`, no padding that is
/// left uninitialized, no references, no drop glue, and stable for direct byte
/// casts on little-endian hosts.
pub unsafe trait TapeRecord: Copy + Sized + 'static {
    const RECORD_KIND: u32;

    fn ts_ns(&self) -> u64;
}

unsafe impl TapeRecord for BookEventV1 {
    const RECORD_KIND: u32 = RECORD_KIND_BOOK;

    #[inline]
    fn ts_ns(&self) -> u64 {
        self.ts_ns
    }
}

unsafe impl TapeRecord for TradeEventV1 {
    const RECORD_KIND: u32 = RECORD_KIND_TRADE;

    #[inline]
    fn ts_ns(&self) -> u64 {
        self.ts_ns
    }
}

unsafe impl TapeRecord for BtcTickV1 {
    const RECORD_KIND: u32 = RECORD_KIND_BTC;

    #[inline]
    fn ts_ns(&self) -> u64 {
        self.ts_ns
    }
}

pub fn validate_host_layout() -> Result<()> {
    ensure!(
        cfg!(target_endian = "little"),
        "packed replay tapes require little-endian hosts"
    );
    ensure!(size_of::<TapeHeaderV1>() == TAPE_HEADER_SIZE as usize);
    ensure!(align_of::<TapeHeaderV1>() == 8);
    ensure!(size_of::<BookEventV1>() == 24);
    ensure!(size_of::<TradeEventV1>() == 24);
    ensure!(size_of::<BtcTickV1>() == 24);
    Ok(())
}

pub fn market_id_hash(market_id: &str) -> u64 {
    let digest = Sha256::digest(market_id.as_bytes());
    u64::from_le_bytes(digest[0..8].try_into().expect("sha256 prefix length"))
}

pub fn price_to_ticks(price: f64) -> Result<u32> {
    finite_non_negative_to_scaled_u32(price, PRICE_SCALE, "price")
}

pub fn size_to_lots(size: f64) -> Result<u32> {
    rounded_non_negative_to_scaled_u32(size, SIZE_SCALE, "size")
}

pub fn btc_price_to_cents(price: f64) -> Result<u64> {
    if !price.is_finite() || price < 0.0 {
        bail!("btc price must be finite and non-negative");
    }
    let scaled = (price * 100.0).round();
    ensure!(scaled <= u64::MAX as f64, "btc price overflows u64 cents");
    Ok(scaled as u64)
}

#[inline]
pub fn ticks_to_price(price_ticks: u32) -> f64 {
    price_ticks as f64 / PRICE_SCALE
}

#[inline]
pub fn lots_to_size(size_lots: u32) -> f64 {
    size_lots as f64 / SIZE_SCALE
}

pub fn header_for<T: TapeRecord>(market_id: &str, records: &[T]) -> TapeHeaderV1 {
    let (ts_min_ns, ts_max_ns) = match (records.first(), records.last()) {
        (Some(first), Some(last)) => (first.ts_ns(), last.ts_ns()),
        _ => (0, 0),
    };

    TapeHeaderV1 {
        magic: TAPE_MAGIC,
        version: TAPE_VERSION,
        record_kind: T::RECORD_KIND,
        record_size: size_of::<T>() as u32,
        header_size: TAPE_HEADER_SIZE,
        record_count: records.len() as u64,
        ts_min_ns,
        ts_max_ns,
        market_id_hash: market_id_hash(market_id),
        flags: 0,
        _reserved: [0; 4],
    }
}

pub fn validate_header<T: TapeRecord>(header: &TapeHeaderV1) -> Result<()> {
    ensure!(header.magic == TAPE_MAGIC, "invalid tape magic");
    ensure!(header.version == TAPE_VERSION, "unsupported tape version");
    ensure!(header.header_size == TAPE_HEADER_SIZE, "invalid tape header size");
    ensure!(
        header.record_kind == T::RECORD_KIND,
        "unexpected tape record kind"
    );
    ensure!(
        header.record_size as usize == size_of::<T>(),
        "unexpected tape record size"
    );
    Ok(())
}

pub fn validate_monotonic<T: TapeRecord>(records: &[T]) -> Result<()> {
    let mut previous = None;
    for record in records {
        if let Some(previous_ts) = previous {
            ensure!(
                record.ts_ns() >= previous_ts,
                "out-of-order tape record timestamp"
            );
        }
        previous = Some(record.ts_ns());
    }
    Ok(())
}

pub fn as_bytes<T: TapeRecord>(records: &[T]) -> &[u8] {
    let byte_len = records.len() * size_of::<T>();
    unsafe { slice::from_raw_parts(records.as_ptr().cast::<u8>(), byte_len) }
}

pub fn one_as_bytes<T: Copy>(record: &T) -> &[u8] {
    unsafe { slice::from_raw_parts((record as *const T).cast::<u8>(), size_of::<T>()) }
}

/// Cast a validated byte slice to fixed tape records.
///
/// # Safety
///
/// The caller must ensure the bytes came from a tape file with a matching
/// `TapeHeaderV1`, the slice starts at an address aligned for `T`, and its
/// length is an exact multiple of `size_of::<T>()`.
pub unsafe fn records_from_bytes<T: TapeRecord>(bytes: &[u8]) -> &[T] {
    slice::from_raw_parts(bytes.as_ptr().cast::<T>(), bytes.len() / size_of::<T>())
}

fn finite_non_negative_to_scaled_u32(value: f64, scale: f64, label: &str) -> Result<u32> {
    if !value.is_finite() || value < 0.0 {
        bail!("{label} must be finite and non-negative");
    }
    let scaled = (value * scale).round();
    ensure!(scaled <= u32::MAX as f64, "{label} overflows u32");
    let roundtrip = scaled / scale;
    ensure!(
        (roundtrip - value).abs() <= 1e-9,
        "{label} cannot be represented at tape scale"
    );
    Ok(scaled as u32)
}

fn rounded_non_negative_to_scaled_u32(value: f64, scale: f64, label: &str) -> Result<u32> {
    if !value.is_finite() || value < 0.0 {
        bail!("{label} must be finite and non-negative");
    }
    let scaled = (value * scale).round();
    ensure!(scaled <= u32::MAX as f64, "{label} overflows u32");
    Ok(scaled as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_layouts_are_fixed() {
        validate_host_layout().unwrap();
    }

    #[test]
    fn price_scaling_rejects_unrepresentable_values() {
        assert_eq!(price_to_ticks(0.1234).unwrap(), 1234);
        assert!(price_to_ticks(0.12345).is_err());
    }
}
