use std::collections::HashMap;
use std::env;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use polymarket_exec::paper::shadow_quote::ShadowQuoteRecord;
use polymarket_exec::types::TradeSide;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize)]
struct BookSnapshot {
    t: u64,
    asset: String,
    bids: Vec<[f64; 2]>,
    asks: Vec<[f64; 2]>,
}

#[derive(Clone, Debug, Default, Serialize)]
struct ScenarioStats {
    name: &'static str,
    quote_count: usize,
    plausible_fill_count: usize,
    plausible_fill_rate: f64,
    plausible_notional_usd: f64,
    avg_delay_ms: Option<f64>,
}

#[derive(Clone, Debug, Serialize)]
struct CalibrationSummary {
    quotes_path: String,
    books_path: String,
    quote_count: usize,
    book_snapshot_count: usize,
    total_quoted_notional_usd: f64,
    optimistic: ScenarioStats,
    base: ScenarioStats,
    conservative: ScenarioStats,
}

#[derive(Clone, Copy)]
struct Scenario {
    name: &'static str,
    horizon_ms: u64,
    min_age_ms: u64,
    queue_depth_fraction: f64,
    require_trade_through: bool,
}

fn main() -> Result<()> {
    let args = Args::parse()?;
    let quotes = read_jsonl::<ShadowQuoteRecord>(&args.quotes)
        .with_context(|| format!("failed to read quotes from {}", args.quotes.display()))?;
    let books = read_jsonl::<BookSnapshot>(&args.books)
        .with_context(|| format!("failed to read books from {}", args.books.display()))?;
    let mut books_by_asset: HashMap<String, Vec<BookSnapshot>> = HashMap::new();
    for book in books {
        books_by_asset
            .entry(book.asset.clone())
            .or_default()
            .push(book);
    }
    for snapshots in books_by_asset.values_mut() {
        snapshots.sort_by_key(|book| book.t);
    }

    let scenarios = [
        Scenario {
            name: "optimistic",
            horizon_ms: 60_000,
            min_age_ms: 0,
            queue_depth_fraction: 0.25,
            require_trade_through: false,
        },
        Scenario {
            name: "base",
            horizon_ms: 60_000,
            min_age_ms: 500,
            queue_depth_fraction: 0.75,
            require_trade_through: true,
        },
        Scenario {
            name: "conservative",
            horizon_ms: 30_000,
            min_age_ms: 1_500,
            queue_depth_fraction: 0.90,
            require_trade_through: true,
        },
    ];

    let summary = CalibrationSummary {
        quotes_path: args.quotes.display().to_string(),
        books_path: args.books.display().to_string(),
        quote_count: quotes.len(),
        book_snapshot_count: books_by_asset.values().map(Vec::len).sum(),
        total_quoted_notional_usd: quotes.iter().map(|quote| quote.notional_usd).sum(),
        optimistic: score_scenario(&quotes, &books_by_asset, scenarios[0]),
        base: score_scenario(&quotes, &books_by_asset, scenarios[1]),
        conservative: score_scenario(&quotes, &books_by_asset, scenarios[2]),
    };

    let json = serde_json::to_string_pretty(&summary)?;
    if let Some(output) = args.output {
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = File::create(&output)
            .with_context(|| format!("failed to create {}", output.display()))?;
        file.write_all(json.as_bytes())?;
        file.write_all(b"\n")?;
    } else {
        println!("{json}");
    }
    Ok(())
}

fn score_scenario(
    quotes: &[ShadowQuoteRecord],
    books_by_asset: &HashMap<String, Vec<BookSnapshot>>,
    scenario: Scenario,
) -> ScenarioStats {
    let mut stats = ScenarioStats {
        name: scenario.name,
        quote_count: quotes.len(),
        ..ScenarioStats::default()
    };
    let mut delay_sum = 0u128;
    for quote in quotes {
        let Some(snapshots) = books_by_asset.get(&quote.instrument_id) else {
            continue;
        };
        let Some((delay_ms, notional)) = plausible_fill(quote, snapshots, scenario) else {
            continue;
        };
        stats.plausible_fill_count += 1;
        stats.plausible_notional_usd += notional;
        delay_sum += delay_ms as u128;
    }
    if stats.quote_count > 0 {
        stats.plausible_fill_rate = stats.plausible_fill_count as f64 / stats.quote_count as f64;
    }
    if stats.plausible_fill_count > 0 {
        stats.avg_delay_ms = Some(delay_sum as f64 / stats.plausible_fill_count as f64);
    }
    stats
}

fn plausible_fill(
    quote: &ShadowQuoteRecord,
    snapshots: &[BookSnapshot],
    scenario: Scenario,
) -> Option<(u64, f64)> {
    let start = quote.observed_at_ms.saturating_add(scenario.min_age_ms);
    let end = quote.observed_at_ms.saturating_add(scenario.horizon_ms);
    for book in snapshots
        .iter()
        .filter(|book| book.t >= start && book.t <= end)
    {
        let levels = match quote.side {
            TradeSide::Buy => &book.asks,
            TradeSide::Sell => &book.bids,
        };
        let eligible: Vec<[f64; 2]> = levels
            .iter()
            .copied()
            .filter(|level| match quote.side {
                TradeSide::Buy => level[0] <= quote.limit_price,
                TradeSide::Sell => level[0] >= quote.limit_price,
            })
            .collect();
        if eligible.is_empty() {
            continue;
        }
        if scenario.require_trade_through && !touched_through(&eligible, quote) {
            continue;
        }
        let available_after_queue: f64 = eligible
            .iter()
            .enumerate()
            .map(|(idx, level)| {
                if idx == 0 {
                    level[1] * (1.0 - scenario.queue_depth_fraction).max(0.0)
                } else {
                    level[1]
                }
            })
            .sum();
        let fill_qty = quote.quantity.min(available_after_queue);
        if fill_qty <= 0.0 {
            continue;
        }
        let notional = fill_qty * quote.limit_price;
        return Some((book.t.saturating_sub(quote.observed_at_ms), notional));
    }
    None
}

fn touched_through(eligible: &[[f64; 2]], quote: &ShadowQuoteRecord) -> bool {
    let best = eligible[0][0];
    match quote.side {
        TradeSide::Buy => best < quote.limit_price,
        TradeSide::Sell => best > quote.limit_price,
    }
}

fn read_jsonl<T: for<'de> Deserialize<'de>>(path: &PathBuf) -> Result<Vec<T>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut rows = Vec::new();
    for (idx, line) in reader.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        rows.push(
            serde_json::from_str::<T>(&line)
                .with_context(|| format!("invalid JSONL at {}:{}", path.display(), idx + 1))?,
        );
    }
    Ok(rows)
}

struct Args {
    quotes: PathBuf,
    books: PathBuf,
    output: Option<PathBuf>,
}

impl Args {
    fn parse() -> Result<Self> {
        let mut quotes = PathBuf::from("data/execution/paper/shadow_quotes.jsonl");
        let mut books = PathBuf::from("data/execution/paper/books.jsonl");
        let mut output = None;
        let mut args = env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--quotes" => {
                    quotes = PathBuf::from(args.next().context("--quotes requires a path")?);
                }
                "--books" => {
                    books = PathBuf::from(args.next().context("--books requires a path")?);
                }
                "--output" => {
                    output = Some(PathBuf::from(
                        args.next().context("--output requires a path")?,
                    ));
                }
                "--help" | "-h" => {
                    println!(
                        "usage: paper_fill_calibration [--quotes path] [--books path] [--output path]"
                    );
                    std::process::exit(0);
                }
                other => anyhow::bail!("unknown arg: {other}"),
            }
        }
        Ok(Self {
            quotes,
            books,
            output,
        })
    }
}
