use anyhow::Result;
use clap::Parser;
use polymarket_exec::replay::tape::convert::{convert_raw_prefix, TapeConvertOptions};
use serde::Serialize;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "tape_convert",
    about = "Convert local raw Telonex/Binance Parquet into packed replay tapes"
)]
struct Cli {
    #[arg(long)]
    input_prefix: PathBuf,

    #[arg(long)]
    output_dir: PathBuf,

    #[arg(long)]
    market_slug: String,

    #[arg(long)]
    yes_asset_id: String,

    #[arg(long)]
    no_asset_id: String,

    #[arg(long)]
    window_start_ns: i64,

    #[arg(long)]
    window_end_ns: i64,

    #[arg(long, default_value_t = 25)]
    max_book_levels: usize,
}

#[derive(Serialize)]
struct JsonSummary {
    book_events: usize,
    trade_events: usize,
    btc_ticks: usize,
    book_path: String,
    trades_path: String,
    btc_path: String,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let summary = convert_raw_prefix(&TapeConvertOptions {
        input_prefix: cli.input_prefix,
        output_dir: cli.output_dir,
        market_slug: cli.market_slug,
        yes_asset_id: cli.yes_asset_id,
        no_asset_id: cli.no_asset_id,
        window_start_ns: cli.window_start_ns,
        window_end_ns: cli.window_end_ns,
        max_book_levels: cli.max_book_levels,
    })?;
    println!(
        "{}",
        serde_json::to_string_pretty(&JsonSummary {
            book_events: summary.book_events,
            trade_events: summary.trade_events,
            btc_ticks: summary.btc_ticks,
            book_path: summary.book_path.display().to_string(),
            trades_path: summary.trades_path.display().to_string(),
            btc_path: summary.btc_path.display().to_string(),
        })?
    );
    Ok(())
}

