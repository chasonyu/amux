//! Session occupation: flock under `~/.amux/locks/` + best-effort pgrep.
//! Process-level single-instance lock with replace-on-start.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
#[allow(deprecated)]
use nix::fcntl::{flock, FlockArg};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;

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
        let resume_hit =
            lower.contains("--resume") || lower.contains(" -r ") || lower.contains(" -r=");
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

/// Held for process lifetime — exclusive amux instance lock.
pub struct InstanceLock {
    _file: File,
}

fn instance_lock_path() -> PathBuf {
    AmuxConfig::amux_dir().join("amux.instance.lock")
}

/// Acquire the single-instance lock. If another amux holds it, ask that
/// process to exit (SIGTERM → wait → SIGKILL) so this process becomes the
/// sole controller. Live PTYs die with the old process; disk sessions remain
/// resumable.
pub fn acquire_instance_lock_replacing() -> Result<InstanceLock> {
    AmuxConfig::ensure_dirs()?;
    let path = instance_lock_path();
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("open instance lock {}", path.display()))?;

    if try_flock_exclusive(&file).is_ok() {
        write_pid(&mut file)?;
        return Ok(InstanceLock { _file: file });
    }

    let old_pid = read_lock_pid(&mut file).or_else(find_other_amux_pid);
    if let Some(pid) = old_pid {
        eprintln!("amux: replacing previous instance (pid {pid})…");
        replace_old_instance(pid)?;
    } else {
        eprintln!("amux: instance lock busy; waiting for previous holder to exit…");
    }

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if try_flock_exclusive(&file).is_ok() {
            write_pid(&mut file)?;
            return Ok(InstanceLock { _file: file });
        }
        if Instant::now() >= deadline {
            // Last resort: kill whatever amux we can find, then one more try.
            if let Some(pid) = find_other_amux_pid() {
                let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
                thread::sleep(Duration::from_millis(200));
            }
            if try_flock_exclusive(&file).is_ok() {
                write_pid(&mut file)?;
                return Ok(InstanceLock { _file: file });
            }
            bail!(
                "could not acquire amux instance lock at {}\n\
                 Another amux may be stuck; remove the lock only if sure.",
                path.display()
            );
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn try_flock_exclusive(file: &File) -> std::result::Result<(), ()> {
    #[allow(deprecated)]
    match flock(file.as_raw_fd(), FlockArg::LockExclusiveNonblock) {
        Ok(()) => Ok(()),
        Err(_) => Err(()),
    }
}

fn write_pid(file: &mut File) -> Result<()> {
    file.set_len(0).context("truncate instance lock")?;
    file.seek(SeekFrom::Start(0)).context("seek instance lock")?;
    let pid = std::process::id();
    write!(file, "{pid}\n").context("write instance pid")?;
    file.flush().context("flush instance lock")?;
    Ok(())
}

fn read_lock_pid(file: &mut File) -> Option<i32> {
    let mut buf = String::new();
    file.seek(SeekFrom::Start(0)).ok()?;
    file.read_to_string(&mut buf).ok()?;
    let pid: i32 = buf.trim().lines().next()?.parse().ok()?;
    if pid <= 1 || pid == std::process::id() as i32 {
        return None;
    }
    if !pid_looks_like_amux(pid) {
        return None;
    }
    Some(pid)
}

fn replace_old_instance(pid: i32) -> Result<()> {
    let p = Pid::from_raw(pid);
    // Graceful first — amux SIGTERM handler runs teardown (restore TTY, kill children).
    match kill(p, Signal::SIGTERM) {
        Ok(()) => {}
        Err(nix::errno::Errno::ESRCH) => return Ok(()),
        Err(e) => bail!("SIGTERM pid {pid}: {e}"),
    }
    let soft = Instant::now() + Duration::from_secs(2);
    while Instant::now() < soft {
        if !pid_alive(pid) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }
    if pid_alive(pid) {
        let _ = kill(p, Signal::SIGKILL);
        let hard = Instant::now() + Duration::from_secs(1);
        while Instant::now() < hard {
            if !pid_alive(pid) {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
    Ok(())
}

fn pid_alive(pid: i32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

fn pid_looks_like_amux(pid: i32) -> bool {
    let path = format!("/proc/{pid}/cmdline");
    let Ok(bytes) = std::fs::read(&path) else {
        return false;
    };
    let cmd = String::from_utf8_lossy(&bytes).replace('\0', " ");
    let base = cmd.split_whitespace().next().unwrap_or("");
    base.ends_with("amux") || cmd.contains("/amux") || cmd.contains("amux ")
}

/// Fallback when lock file has no usable pid (stale / old soft-lock format).
fn find_other_amux_pid() -> Option<i32> {
    let me = std::process::id();
    let output = Command::new("pgrep").args(["-f", "amux"]).output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        let Ok(pid) = line.trim().parse::<i32>() else {
            continue;
        };
        if pid as u32 == me || pid <= 1 {
            continue;
        }
        if pid_looks_like_amux(pid) {
            return Some(pid);
        }
    }
    None
}

/// Soft try (no replace) — kept for tests / diagnostics.
pub fn try_instance_lock() -> io::Result<File> {
    let _ = AmuxConfig::ensure_dirs();
    let path = instance_lock_path();
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(path)?;
    #[allow(deprecated)]
    match flock(file.as_raw_fd(), FlockArg::LockExclusiveNonblock) {
        Ok(()) => {
            let _ = write_pid(&mut file);
            Ok(file)
        }
        Err(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "another amux instance may be running",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_keeps_uuidish() {
        assert_eq!(sanitize_id("019f-abc_1"), "019f-abc_1");
        assert_eq!(sanitize_id("a/b"), "a_b");
    }
}
