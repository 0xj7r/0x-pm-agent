//! Process singleton lock.
//!
//! Live trading must not run multiple copies for the same profile. This module
//! implements a dependency-free lockfile based on `create_new(true)`. It is
//! intentionally small; runtime startup owns when to acquire it.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct SingletonLock {
    path: PathBuf,
    _file: File,
}

impl SingletonLock {
    pub fn acquire(path: impl AsRef<Path>, label: &str) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        writeln!(file, "pid={}", std::process::id())?;
        writeln!(file, "label={label}")?;

        Ok(Self { path, _file: file })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for SingletonLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}
