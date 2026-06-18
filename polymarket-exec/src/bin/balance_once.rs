//! One-shot CLOB tradable cash probe (venue balance_allowance).
//!
//! Usage:
//!   set -a && source ~/.config/polymarket-exec/wallet.env && set +a
//!   balance_once
//!   balance_once --json

use anyhow::Result;
use clap::Parser;
use polymarket_exec::shadow_exec::connect_live_adapter;
use polymarket_exec::wire::execution_adapter::ExecutionAdapter;

#[derive(Debug, Parser)]
#[command(name = "balance_once")]
struct Args {
    #[arg(long)]
    json: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    let args = Args::parse();

    let adapter = connect_live_adapter().await?;
    let balances = adapter.sync_balances().await?;

    if args.json {
        println!(
            "{{\"venue_cash_usd\":{:.4},\"positions\":{}}}",
            balances.cash_usd,
            balances.positions.len()
        );
    } else {
        println!("{:.2}", balances.cash_usd);
    }
    Ok(())
}