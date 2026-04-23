use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    whale_pair_exec::runtime::runner::run().await
}
