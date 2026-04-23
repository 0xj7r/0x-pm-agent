use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    whale_pair_exec::runner::run().await
}
