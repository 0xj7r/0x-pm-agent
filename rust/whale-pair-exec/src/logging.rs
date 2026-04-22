use anyhow::{Context, Result};
use tracing_subscriber::EnvFilter;

use crate::config::{AppConfig, LogFormat};

pub fn init(config: &AppConfig) -> Result<()> {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(config.log_level.clone()));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_thread_names(true)
        .with_line_number(true);

    match config.log_format {
        LogFormat::Pretty => builder
            .compact()
            .try_init()
            .context("failed to initialize pretty tracing subscriber")?,
        LogFormat::Json => builder
            .json()
            .try_init()
            .context("failed to initialize json tracing subscriber")?,
    }

    Ok(())
}
