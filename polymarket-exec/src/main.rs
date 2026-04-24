//! Binary entrypoint that loads env and runs the executor runtime loop.

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    whale_pair_exec::runtime::runner::run().await
}
