//! S3/Parquet object-key conventions.
//!
//! The first implementation slice keeps this module dependency-light: it owns
//! partitioning and object-key generation, while the concrete Arrow/Parquet
//! writer can be added behind this seam without touching strategy code.

use crate::data::operator_calibration::OperatorCalibrationRow;
use crate::types::MarketId;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct S3ParquetConfig {
    pub bucket: String,
    pub prefix: String,
}

impl S3ParquetConfig {
    pub fn new(bucket: impl Into<String>, prefix: impl Into<String>) -> Self {
        Self {
            bucket: bucket.into(),
            prefix: prefix.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DatasetKind {
    OperatorCalibration,
    Fills,
    Orders,
    Books,
    BookLevels,
    SpotTicks,
    Journal,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DatasetObjectKey {
    pub bucket: String,
    pub key: String,
}

impl DatasetKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OperatorCalibration => "operator_calibration",
            Self::Fills => "fills",
            Self::Orders => "orders",
            Self::Books => "books",
            Self::BookLevels => "book_levels",
            Self::SpotTicks => "spot_ticks",
            Self::Journal => "journal",
        }
    }
}

pub fn market_partition_key(
    config: &S3ParquetConfig,
    dataset: DatasetKind,
    market_id: &MarketId,
    date: &str,
    hour: &str,
    file_stem: &str,
) -> String {
    let prefix = config.prefix.trim_matches('/');
    format!(
        "{prefix}/{dataset}/market_id={market_id}/date={date}/hour={hour}/{file_stem}.parquet",
        dataset = dataset.as_str(),
        market_id = market_id,
    )
}

pub fn market_partition_object(
    config: &S3ParquetConfig,
    dataset: DatasetKind,
    market_id: &MarketId,
    date: &str,
    hour: &str,
    file_stem: &str,
) -> DatasetObjectKey {
    DatasetObjectKey {
        bucket: config.bucket.clone(),
        key: market_partition_key(config, dataset, market_id, date, hour, file_stem),
    }
}

pub fn operator_calibration_key(
    config: &S3ParquetConfig,
    row: &OperatorCalibrationRow,
    file_stem: &str,
) -> String {
    let (date, hour) = row.partition_date_hour();
    market_partition_key(
        config,
        DatasetKind::OperatorCalibration,
        &row.market_id,
        &date,
        &hour,
        file_stem,
    )
}
