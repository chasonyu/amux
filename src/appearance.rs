//! Host terminal appearance (OSC 11).

use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::time::{Duration, Instant};

const PROBE_TIMEOUT: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Appearance {
    Dark,
    Light,
}

/// BT.601 luma (`Y ≈ 0.299R + 0.587G + 0.114B` on 0..255); `< 128` → Dark.
/// Threshold mirrors omp mid-gray (`bg < 8` on 0..15 ≈ half scale).
pub fn appearance_from_rgb(r: u8, g: u8, b: u8) -> Appearance {
    let y = (u32::from(r) * 299 + u32::from(g) * 587 + u32::from(b) * 114) / 1000;
    if y < 128 {
        Appearance::Dark
    } else {
        Appearance::Light
    }
}

pub fn parse_osc11_rgb(reply: &str) -> Option<(u8, u8, u8)> {
    // Accept rgb:RRRR/GGGG/BBBB (16-bit per channel) or rgb:RR/GG/BB
    let start = reply.find("rgb:")? + 4;
    let body = reply[start..]
        .split(|c| c == '\x07' || c == '\x1b')
        .next()?
        .trim();
    let mut parts = body.split('/');
    let r = parse_osc_channel(parts.next()?)?;
    let g = parse_osc_channel(parts.next()?)?;
    let b = parse_osc_channel(parts.next()?)?;
    Some((r, g, b))
}

fn parse_osc_channel(s: &str) -> Option<u8> {
    let v = u32::from_str_radix(s, 16).ok()?;
    Some(match s.len() {
        2 => v as u8,
        4 => (v >> 8) as u8,
        _ => return None,
    })
}

/// Probe host terminal background via OSC 11 + DA1 sentinel (~200ms).
///
/// Callers should pass a **TTY** fd (typically stdin). Readiness is waited with
/// `poll` so a quiet terminal cannot hang past the deadline. We intentionally
/// do **not** set `O_NONBLOCK` on the TTY: fds 0/1/2 often share one open-file
/// description, and flipping non-blocking would also affect stdout (see shell
/// event loop). If a caller does change blocking mode, they must restore it.
///
/// On timeout / parse failure returns [`Appearance::Dark`].
pub fn probe_appearance(out: &mut impl Write, input: &mut (impl Read + AsRawFd)) -> Appearance {
    let fd = input.as_raw_fd();
    probe_appearance_inner(out, input, Some(fd))
}

/// Read-side probe used by unit tests with `Cursor` / `Vec` (no fd / no poll).
fn probe_appearance_inner(
    out: &mut impl Write,
    input: &mut impl Read,
    poll_fd: Option<RawFd>,
) -> Appearance {
    if out.write_all(b"\x1b]11;?\x07\x1b[c").is_err() {
        return Appearance::Dark;
    }
    let _ = out.flush();
    drain_probe_reply(input, poll_fd, PROBE_TIMEOUT)
}

fn drain_probe_reply(
    input: &mut impl Read,
    poll_fd: Option<RawFd>,
    timeout: Duration,
) -> Appearance {
    let deadline = Instant::now() + timeout;
    let mut collected = Vec::new();
    let mut chunk = [0u8; 256];

    while Instant::now() < deadline {
        if let Some(fd) = poll_fd {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match poll_readable(fd, remaining) {
                Ok(true) => {}
                Ok(false) => break, // timed out waiting for data
                Err(_) => break,
            }
        }

        match input.read(&mut chunk) {
            Ok(0) => {
                // EOF (Cursor empty) or no data yet. Without an fd we cannot
                // poll; sleep briefly and retry until the deadline.
                if poll_fd.is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(n) => {
                collected.extend_from_slice(&chunk[..n]);
                if let Some(appearance) = classify_probe_bytes(&collected) {
                    return appearance;
                }
            }
            Err(ref e)
                if e.kind() == io::ErrorKind::WouldBlock
                    || e.kind() == io::ErrorKind::Interrupted =>
            {
                if poll_fd.is_none() {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            Err(_) => break,
        }
    }

    classify_probe_bytes(&collected).unwrap_or(Appearance::Dark)
}

/// Returns `Some` once the reply is conclusive (OSC 11 RGB or DA1-without-OSC).
fn classify_probe_bytes(collected: &[u8]) -> Option<Appearance> {
    let s = String::from_utf8_lossy(collected);
    if let Some((r, g, b)) = parse_osc11_rgb(&s) {
        return Some(appearance_from_rgb(r, g, b));
    }
    // DA1 reply without OSC 11 ⇒ unsupported.
    if s.contains("\x1b[?") && s.contains('c') {
        return Some(Appearance::Dark);
    }
    None
}

fn poll_readable(fd: RawFd, timeout: Duration) -> io::Result<bool> {
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
    use std::os::fd::BorrowedFd;

    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    let mut pfd = [PollFd::new(borrowed, PollFlags::POLLIN)];
    let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
    let to = PollTimeout::try_from(ms).unwrap_or(PollTimeout::ZERO);
    match poll(&mut pfd, to) {
        Ok(n) => Ok(n > 0),
        Err(nix::errno::Errno::EINTR) | Err(nix::errno::Errno::EAGAIN) => Ok(false),
        Err(e) => Err(io::Error::from_raw_os_error(e as i32)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::os::unix::io::FromRawFd;

    #[test]
    fn dark_when_luma_low() {
        assert_eq!(appearance_from_rgb(0x22, 0x1d, 0x1a), Appearance::Dark);
        assert_eq!(appearance_from_rgb(0, 0, 0), Appearance::Dark);
    }

    #[test]
    fn light_when_luma_high() {
        assert_eq!(appearance_from_rgb(0xf5, 0xf5, 0xf5), Appearance::Light);
        assert_eq!(appearance_from_rgb(255, 255, 255), Appearance::Light);
    }

    #[test]
    fn parse_osc11_rgb_channels() {
        let s = "\x1b]11;rgb:221d/1d1d/1a1a\x07";
        let (r, g, b) = parse_osc11_rgb(s).unwrap();
        assert!(r < 0x40 && g < 0x40 && b < 0x40);
    }

    #[test]
    fn probe_osc11_reply_is_light() {
        let mut out = Vec::new();
        let reply = b"\x1b]11;rgb:eeee/eeee/eeee\x07\x1b[?1;2c";
        let mut input = Cursor::new(reply.as_slice());
        assert_eq!(
            probe_appearance_inner(&mut out, &mut input, None),
            Appearance::Light
        );
        assert_eq!(out.as_slice(), b"\x1b]11;?\x07\x1b[c");
    }

    #[test]
    fn probe_da1_without_osc_is_dark() {
        let mut out = Vec::new();
        let reply = b"\x1b[?1;2c";
        let mut input = Cursor::new(reply.as_slice());
        assert_eq!(
            probe_appearance_inner(&mut out, &mut input, None),
            Appearance::Dark
        );
    }

    #[test]
    fn probe_empty_cursor_times_out_dark() {
        let mut out = Vec::new();
        let mut input = Cursor::new(&[][..]);
        let started = Instant::now();
        let appearance = probe_appearance_inner(&mut out, &mut input, None);
        let elapsed = started.elapsed();
        assert_eq!(appearance, Appearance::Dark);
        // Soft timeout path (no fd): ~200ms, allow slack for scheduling.
        assert!(
            elapsed >= Duration::from_millis(150),
            "elapsed {elapsed:?} too short for probe timeout"
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "elapsed {elapsed:?} too long for probe timeout"
        );
    }

    #[test]
    fn probe_tty_poll_timeout_on_quiet_pipe() {
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let read_fd = fds[0];
        let write_fd = fds[1];
        // Keep write end open so read does not see EOF; no data written → poll waits.
        let mut reader = unsafe { std::fs::File::from_raw_fd(read_fd) };
        let mut out = Vec::new();
        let started = Instant::now();
        let appearance = probe_appearance(&mut out, &mut reader);
        let elapsed = started.elapsed();
        drop(reader);
        unsafe {
            libc::close(write_fd);
        }
        assert_eq!(appearance, Appearance::Dark);
        assert!(
            elapsed >= Duration::from_millis(150),
            "elapsed {elapsed:?} too short for poll timeout"
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "elapsed {elapsed:?} too long for poll timeout"
        );
    }
}
