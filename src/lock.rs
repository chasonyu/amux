//! Session occupation: flock under `~/.amux/locks/` + best-effort pgrep.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
#[allow(deprecated)]
use nix::fcntl::{flock, FlockArg};

use crate::config::AmuxConfig;

pub struct SessionLock {
    _file: File,
    path: PathBuf,
}

impl SessionLock {
    /// Try to acquire exclusive non-blocking flock for `session_id`.
    pub fn try_acquire(session_id: &str) -> Result<Self> {
        AmuxConfig::ensure_dirs()?;
        let safe = sanitize_id(session_id);
        let path = AmuxConfig::locks_dir().join(format!("{safe}.lock"));
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("open lock {}", path.display()))?;
        #[allow(deprecated)]
        match flock(file.as_raw_fd(), FlockArg::LockExclusiveNonblock) {
            Ok(()) => Ok(Self { _file: file, path }),
            Err(nix::errno::Errno::EAGAIN) => {
                bail!(
                    "session occupied (amux flock held): {}\n\
                     Another amux instance may be attached. Close it or remove stale lock if sure.",
                    path.display()
                );
            }
            Err(e) => Err(e).context("flock"),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for SessionLock {
    fn drop(&mut self) {
        #[allow(deprecated)]
        let _ = flock(self._file.as_raw_fd(), FlockArg::Unlock);
        let _ = std::fs::remove_file(&self.path);
    }
}

fn sanitize_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Best-effort: refuse if an external `omp -r` / `--resume` for this id is running.
pub fn pgrep_external_omp_resume(session_id: &str) -> Option<String> {
    let prefix = if session_id.len() > 8 {
        &session_id[..8]
    } else {
        session_id
    };
    let output = Command::new("pgrep")
        .args(["-af", "omp"])
        .output()
        .ok()?;
    if !output.status.success() && output.stdout.is_empty() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        let lower = line.to_ascii_lowercase();
        let resume_hit = lower.contains("--resume") || lower.contains(" -r ") || lower.contains(" -r=");
        if resume_hit && line.contains(prefix) {
            // Ignore our own process line if it somehow matches
            if lower.contains("amux") {
                continue;
            }
            return Some(line.trim().to_string());
        }
    }
    None
}

pub fn check_occupiable(session_id: &str) -> Result<()> {
    if let Some(line) = pgrep_external_omp_resume(session_id) {
        bail!(
            "session appears occupied by external omp:\n  {line}\n\
             Attach refused (no force-hijack)."
        );
    }
    Ok(())
}

/// Single-instance advisory lock for amux itself (optional soft lock).
pub fn try_instance_lock() -> io::Result<File> {
    let _ = AmuxConfig::ensure_dirs();
    let path = AmuxConfig::amux_dir().join("amux.instance.lock");
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(path)?;
    #[allow(deprecated)]
    match flock(file.as_raw_fd(), FlockArg::LockExclusiveNonblock) {
        Ok(()) => Ok(file),
        Err(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "another amux instance may be running",
        )),
    }
}
