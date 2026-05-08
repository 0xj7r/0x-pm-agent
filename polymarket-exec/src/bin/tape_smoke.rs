use anyhow::Result;
use clap::Parser;
use polymarket_exec::replay::tape::smoke::run_tape_smoke;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "tape_smoke", about = "Smoke replay packed binary tapes")]
struct Cli {
    #[arg(long)]
    book: PathBuf,

    #[arg(long)]
    trades: PathBuf,

    #[arg(long)]
    btc: PathBuf,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let stats = run_tape_smoke(cli.book, cli.trades, cli.btc)?;
    println!("{}", serde_json::to_string_pretty(&stats)?);
    Ok(())
}

