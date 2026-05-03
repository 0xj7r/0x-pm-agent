use polymarket_client_sdk::types::Address as SdkAddress;

use crate::types::{EpochMillis, FillLiquidity, InstrumentId, MarketId, OrderId, TradeSide};
use crate::wire::execution_adapter::{ExecutionError, VenueFill};

#[derive(Clone, Copy)]
pub(super) enum RawTradeFilter {
    Maker,
    Taker,
}

impl RawTradeFilter {
    pub(super) fn query_key(self) -> &'static str {
        match self {
            Self::Maker => "maker",
            Self::Taker => "taker",
        }
    }
}

pub(super) fn parse_raw_trades_page(
    body: &str,
    filter: RawTradeFilter,
    trade_address: SdkAddress,
) -> Result<Vec<VenueFill>, ExecutionError> {
    let raw: serde_json::Value = serde_json::from_str(body).map_err(|error| {
        ExecutionError::VenueRejection(format!(
            "failed to decode CLOB trades response `{body}`: {error}"
        ))
    })?;
    let trades = raw
        .get("data")
        .and_then(|value| value.as_array())
        .ok_or_else(|| {
            ExecutionError::VenueRejection("CLOB trades response missing data array".to_string())
        })?;

    let mut fills = Vec::new();
    for trade in trades {
        match filter {
            RawTradeFilter::Maker => {
                let Some(market_id) = json_string(trade.get("market")) else {
                    continue;
                };
                let observed_at_ms = json_epoch_seconds_ms(trade.get("match_time"));
                let maker_orders = trade
                    .get("maker_orders")
                    .and_then(|value| value.as_array())
                    .into_iter()
                    .flatten();
                for maker in maker_orders {
                    if !json_string(maker.get("maker_address")).is_some_and(|address| {
                        address.eq_ignore_ascii_case(&trade_address.to_string())
                    }) {
                        continue;
                    }
                    let (Some(order_id), Some(asset_id), Some(side), Some(price), Some(quantity)) = (
                        json_string(maker.get("order_id")),
                        json_string(maker.get("asset_id")),
                        json_trade_side(maker.get("side")),
                        json_decimal_f64(maker.get("price")),
                        json_decimal_f64(maker.get("matched_amount")),
                    ) else {
                        continue;
                    };
                    fills.push(VenueFill {
                        venue_order_id: OrderId::from(order_id),
                        client_order_id: None,
                        market_id: MarketId::from(market_id.clone()),
                        instrument_id: InstrumentId::from(asset_id),
                        side,
                        price,
                        quantity,
                        fee_usd: 0.0,
                        liquidity: FillLiquidity::Maker,
                        observed_at_ms,
                    });
                }
            }
            RawTradeFilter::Taker => {
                let (
                    Some(order_id),
                    Some(market_id),
                    Some(asset_id),
                    Some(side),
                    Some(price),
                    Some(quantity),
                ) = (
                    json_string(trade.get("taker_order_id")),
                    json_string(trade.get("market")),
                    json_string(trade.get("asset_id")),
                    json_trade_side(trade.get("side")),
                    json_decimal_f64(trade.get("price")),
                    json_decimal_f64(trade.get("size")),
                )
                else {
                    continue;
                };
                fills.push(VenueFill {
                    venue_order_id: OrderId::from(order_id),
                    client_order_id: None,
                    market_id: MarketId::from(market_id),
                    instrument_id: InstrumentId::from(asset_id),
                    side,
                    price,
                    quantity,
                    fee_usd: 0.0,
                    liquidity: json_fill_liquidity(trade.get("trader_side")),
                    observed_at_ms: json_epoch_seconds_ms(trade.get("match_time")),
                });
            }
        }
    }
    Ok(fills)
}

fn json_string(value: Option<&serde_json::Value>) -> Option<String> {
    match value? {
        serde_json::Value::String(raw) => {
            let trimmed = raw.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        }
        serde_json::Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

fn json_decimal_f64(value: Option<&serde_json::Value>) -> Option<f64> {
    match value? {
        serde_json::Value::Number(number) => number.as_f64(),
        serde_json::Value::String(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                None
            } else {
                trimmed.parse::<f64>().ok()
            }
        }
        _ => None,
    }
}

fn json_epoch_seconds_ms(value: Option<&serde_json::Value>) -> EpochMillis {
    json_decimal_f64(value)
        .map(|seconds| (seconds.max(0.0) * 1_000.0) as EpochMillis)
        .unwrap_or_default()
}

fn json_trade_side(value: Option<&serde_json::Value>) -> Option<TradeSide> {
    match json_string(value)?.to_ascii_uppercase().as_str() {
        "BUY" => Some(TradeSide::Buy),
        "SELL" => Some(TradeSide::Sell),
        _ => None,
    }
}

fn json_fill_liquidity(value: Option<&serde_json::Value>) -> FillLiquidity {
    match json_string(value)
        .unwrap_or_default()
        .to_ascii_uppercase()
        .as_str()
    {
        "MAKER" => FillLiquidity::Maker,
        "TAKER" => FillLiquidity::Taker,
        _ => FillLiquidity::Unknown,
    }
}
