//! Process singleton lock.
//!
//! Live trading must not run multiple copies for the same profile. This module
//! implements a dependency-free PID lockfile based on `create_new(true)`.
//! Startup recovers stale locks left behind by SIGKILL/reboot, but still fails
//! closed when the recorded owner process is alive.

use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Write};
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

        let mut file = match open_new_lockfile(&path) {
            Ok(file) => file,
            Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                if !is_stale_lockfile(&path)? {
                    return Err(io::Error::new(
                        ErrorKind::AlreadyExists,
                        format!("live singleton lock is already held: {}", path.display()),
                    ));
                }
                fs::remove_file(&path)?;
                open_new_lockfile(&path)?
            }
            Err(err) => return Err(err),
        };

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

fn open_new_lockfile(path: &Path) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

fn is_stale_lockfile(path: &Path) -> io::Result<bool> {
    let content = fs::read_to_string(path)?;
    let Some(pid) = parse_lock_pid(&content) else {
        return Ok(false);
    };

    Ok(!process_is_alive(pid))
}

fn parse_lock_pid(content: &str) -> Option<u32> {
    content.lines().find_map(|line| {
        let value = line.strip_prefix("pid=")?;
        value.trim().parse::<u32>().ok()
    })
}

#[cfg(target_os = "linux")]
fn process_is_alive(pid: u32) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}

#[cfg(not(target_os = "linux"))]
fn process_is_alive(_pid: u32) -> bool {
    true
}
