//! Runtime preflight checks for live execution.

use std::path::PathBuf;

use crate::infra::singleton_lock::SingletonLock;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LivePreflightConfig {
    pub lock_path: PathBuf,
    pub label: String,
    pub require_clean_market_context: bool,
}

impl LivePreflightConfig {
    pub fn new(lock_path: impl Into<PathBuf>, label: impl Into<String>) -> Self {
        Self {
            lock_path: lock_path.into(),
            label: label.into(),
            require_clean_market_context: true,
        }
    }
}

#[derive(Debug)]
pub struct LivePreflightGuard {
    pub singleton: SingletonLock,
}

pub fn run_live_preflight(config: &LivePreflightConfig) -> std::io::Result<LivePreflightGuard> {
    let singleton = SingletonLock::acquire(&config.lock_path, &config.label)?;
    Ok(LivePreflightGuard { singleton })
}
