//! Host terminal appearance (OSC 11 + COLORFGBG + Mode 2031).

use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::time::{Duration, Instant};

const PROBE_TIMEOUT: Duration = Duration::from_millis(400);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Appearance {
    Dark,
    Light,
}

/// Host FG/BG used for PTY OSC 10/11 replies and default-cell paint.
///
/// Keeping OSC answers and painted defaults on the **same** RGB avoids omp
/// seeing "terminal is black" while empty cells show a different host shade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostSurface {
    pub appearance: Appearance,
    pub fg: (u8, u8, u8),
    pub bg: (u8, u8, u8),
}

impl HostSurface {
    pub fn fallback(appearance: Appearance) -> Self {
        let (fg, bg) = default_fg_bg(appearance);
        Self {
            appearance,
            fg,
            bg,
        }
    }

    pub fn from_bg(r: u8, g: u8, b: u8) -> Self {
        let appearance = appearance_from_rgb(r, g, b);
        let (fg, _) = default_fg_bg(appearance);
        Self {
            appearance,
            fg,
            bg: (r, g, b),
        }
    }

    pub fn from_fg_bg(fg: (u8, u8, u8), bg: (u8, u8, u8)) -> Self {
        Self {
            appearance: appearance_from_rgb(bg.0, bg.1, bg.2),
            fg,
            bg,
        }
    }
}

/// Fallback FG/BG when OSC probe fails (dark=xterm black/white, light=omp page).
pub fn default_fg_bg(appearance: Appearance) -> ((u8, u8, u8), (u8, u8, u8)) {
    match appearance {
        Appearance::Dark => ((0xff, 0xff, 0xff), (0x00, 0x00, 0x00)),
        Appearance::Light => ((0x1e, 0x1e, 0x1e), (0xf8, 0xf8, 0xf8)),
    }
}

/// BT.601 luma (`Y ≈ 0.299R + 0.587G + 0.114B` on 0..255); `< 128` → Dark.
pub fn appearance_from_rgb(r: u8, g: u8, b: u8) -> Appearance {
    let y = (u32::from(r) * 299 + u32::from(g) * 587 + u32::from(b) * 114) / 1000;
    if y < 128 {
        Appearance::Dark
    } else {
        Appearance::Light
    }
}

/// `COLORFGBG=fg;bg` (common in xterm/tmux). Uses **bg** index: 0–6/8 → Dark, else Light.
pub fn appearance_from_colorfgbg(value: &str) -> Option<Appearance> {
    let bg = value.split(';').nth(1)?.trim().parse::<u8>().ok()?;
    if bg <= 6 || bg == 8 {
        Some(Appearance::Dark)
    } else {
        Some(Appearance::Light)
    }
}

pub fn appearance_from_colorfgbg_env() -> Option<Appearance> {
    std::env::var("COLORFGBG")
        .ok()
        .as_deref()
        .and_then(appearance_from_colorfgbg)
}

pub fn parse_osc11_rgb(reply: &str) -> Option<(u8, u8, u8)> {
    parse_osc_color_rgb(reply, 11)
}

pub fn parse_osc10_rgb(reply: &str) -> Option<(u8, u8, u8)> {
    parse_osc_color_rgb(reply, 10)
}

fn parse_osc_color_rgb(reply: &str, code: u16) -> Option<(u8, u8, u8)> {
    let marker = format!("]{code};rgb:");
    let start = reply.find(&marker)? + marker.len();
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

/// Mode 2031 DSR: `\x1b[?997;1n` = dark, `\x1b[?997;2n` = light.
pub fn parse_mode2031_dsr(seq: &[u8]) -> Option<Appearance> {
    let s = std::str::from_utf8(seq).ok()?;
    let rest = s.strip_prefix("\x1b[?997;")?.strip_suffix('n')?;
    match rest {
        "1" => Some(Appearance::Dark),
        "2" => Some(Appearance::Light),
        _ => None,
    }
}

/// Primary Device Attributes reply (`CSI ? … c`) — probe sentinel leftover.
pub fn is_da1_reply(seq: &[u8]) -> bool {
    seq.len() >= 4
        && seq[0] == 0x1b
        && seq[1] == b'['
        && seq[2] == b'?'
        && seq.last() == Some(&b'c')
}

/// OSC sequences that set dynamic colors inside the PTY emulator.
pub fn palette_set_osc(surface: HostSurface) -> Vec<u8> {
    let (fr, fg, fb) = surface.fg;
    let (br, bg, bb) = surface.bg;
    format!(
        "\x1b]10;rgb:{fr:02x}{fr:02x}/{fg:02x}{fg:02x}/{fb:02x}{fb:02x}\x07\
         \x1b]11;rgb:{br:02x}{br:02x}/{bg:02x}{bg:02x}/{bb:02x}{bb:02x}\x07\
         \x1b]12;rgb:{fr:02x}{fr:02x}/{fg:02x}{fg:02x}/{fb:02x}{fb:02x}\x07"
    )
    .into_bytes()
}

/// Injected to child so omp re-queries OSC 11 after host theme change.
pub fn mode2031_notify_bytes(appearance: Appearance) -> Vec<u8> {
    match appearance {
        Appearance::Dark => b"\x1b[?997;1n".to_vec(),
        Appearance::Light => b"\x1b[?997;2n".to_vec(),
    }
}

/// `COLORFGBG` for the child. Dark returns `None` (leave host / unset).
pub fn colorfgbg_env(appearance: Appearance) -> Option<&'static str> {
    match appearance {
        Appearance::Dark => None,
        Appearance::Light => Some("0;15"),
    }
}

/// Probe host FG/BG via OSC 10 + 11 + DA1 sentinel.
///
/// **Callers must disable ICANON/ECHO first** (e.g. `enable_raw_mode`).
pub fn probe_host_surface(
    out: &mut impl Write,
    input: &mut (impl Read + AsRawFd),
) -> HostSurface {
    let fd = input.as_raw_fd();
    probe_host_surface_inner(out, input, Some(fd))
}

/// Convenience: appearance-only (tests / callers that ignore RGB).
pub fn probe_appearance(out: &mut impl Write, input: &mut (impl Read + AsRawFd)) -> Appearance {
    probe_host_surface(out, input).appearance
}

/// Parse an OSC 11 reply sequence into a [`HostSurface`] (FG fallback by luma).
pub fn host_surface_from_osc11_seq(seq: &[u8]) -> Option<HostSurface> {
    if seq.len() < 6 || seq[0] != 0x1b || seq[1] != b']' {
        return None;
    }
    let s = std::str::from_utf8(seq).ok()?;
    if !s.as_bytes().get(2..5).is_some_and(|p| p == b"11;") {
        return None;
    }
    let (r, g, b) = parse_osc11_rgb(s)?;
    Some(HostSurface::from_bg(r, g, b))
}

pub fn appearance_from_osc11_seq(seq: &[u8]) -> Option<Appearance> {
    host_surface_from_osc11_seq(seq).map(|s| s.appearance)
}

fn probe_host_surface_inner(
    out: &mut impl Write,
    input: &mut impl Read,
    poll_fd: Option<RawFd>,
) -> HostSurface {
    // OSC 10 fg + OSC 11 bg, then DA1 sentinel.
    if out
        .write_all(b"\x1b]10;?\x07\x1b]11;?\x07\x1b[c")
        .is_err()
    {
        return finalize_surface(&[]);
    }
    let _ = out.flush();
    drain_probe_reply(input, poll_fd, PROBE_TIMEOUT)
}

fn drain_probe_reply(
    input: &mut impl Read,
    poll_fd: Option<RawFd>,
    timeout: Duration,
) -> HostSurface {
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
                Ok(false) => break,
                Err(_) => break,
            }
        }

        match input.read(&mut chunk) {
            Ok(0) => {
                if poll_fd.is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(n) => {
                collected.extend_from_slice(&chunk[..n]);
                if let Some(surface) = classify_surface(&collected) {
                    if da1_present(&collected) {
                        return surface;
                    }
                    continue;
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

    finalize_surface(&collected)
}

fn classify_surface(collected: &[u8]) -> Option<HostSurface> {
    let s = String::from_utf8_lossy(collected);
    let bg = parse_osc11_rgb(&s)?;
    let fg = parse_osc10_rgb(&s).unwrap_or_else(|| {
        let appearance = appearance_from_rgb(bg.0, bg.1, bg.2);
        default_fg_bg(appearance).0
    });
    Some(HostSurface::from_fg_bg(fg, bg))
}

fn da1_present(collected: &[u8]) -> bool {
    let s = String::from_utf8_lossy(collected);
    s.contains("\x1b[?") && s.contains('c')
}

fn finalize_surface(collected: &[u8]) -> HostSurface {
    if let Some(surface) = classify_surface(collected) {
        return surface;
    }
    if let Some(a) = appearance_from_colorfgbg_env() {
        return HostSurface::fallback(a);
    }
    HostSurface::fallback(Appearance::Dark)
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
    fn probe_osc11_reply_is_light_with_host_bg() {
        let mut out = Vec::new();
        let reply = b"\x1b]11;rgb:eeee/eeee/eeee\x07\x1b[?1;2c";
        let mut input = Cursor::new(reply.as_slice());
        let surface = probe_host_surface_inner(&mut out, &mut input, None);
        assert_eq!(surface.appearance, Appearance::Light);
        assert_eq!(surface.bg, (0xee, 0xee, 0xee));
        assert_eq!(out.as_slice(), b"\x1b]10;?\x07\x1b]11;?\x07\x1b[c");
    }

    #[test]
    fn probe_keeps_host_bg_rgb_for_dark() {
        let mut out = Vec::new();
        // Same shade the user terminal returned earlier.
        let reply = b"\x1b]10;rgb:dddd/dddd/dddd\x07\x1b]11;rgb:d5d5/dddd/e0e0\x07\x1b[?1;2c";
        let mut input = Cursor::new(reply.as_slice());
        let surface = probe_host_surface_inner(&mut out, &mut input, None);
        assert_eq!(surface.appearance, Appearance::Light);
        assert_eq!(surface.bg, (0xd5, 0xdd, 0xe0));
        assert_eq!(surface.fg, (0xdd, 0xdd, 0xdd));
    }

    #[test]
    fn probe_dark_host_bg_not_forced_black() {
        let mut out = Vec::new();
        let reply = b"\x1b]11;rgb:1e1e/1e2e/3e3e\x07\x1b[?1;2c";
        let mut input = Cursor::new(reply.as_slice());
        let surface = probe_host_surface_inner(&mut out, &mut input, None);
        assert_eq!(surface.appearance, Appearance::Dark);
        assert_eq!(surface.bg, (0x1e, 0x1e, 0x3e));
        assert_ne!(surface.bg, (0, 0, 0));
    }

    #[test]
    fn probe_da1_without_osc_falls_back_dark_without_colorfgbg() {
        let mut out = Vec::new();
        let reply = b"\x1b[?1;2c";
        let mut input = Cursor::new(reply.as_slice());
        let started = Instant::now();
        let surface = probe_host_surface_inner(&mut out, &mut input, None);
        assert_eq!(surface.appearance, Appearance::Dark);
        assert_eq!(surface.bg, (0, 0, 0));
        assert!(started.elapsed() >= Duration::from_millis(150));
    }

    #[test]
    fn probe_da1_first_then_osc_still_light() {
        let mut out = Vec::new();
        let reply = b"\x1b[?1;2c\x1b]11;rgb:eeee/eeee/eeee\x07";
        let mut input = Cursor::new(reply.as_slice());
        let surface = probe_host_surface_inner(&mut out, &mut input, None);
        assert_eq!(surface.appearance, Appearance::Light);
        assert_eq!(surface.bg, (0xee, 0xee, 0xee));
    }

    #[test]
    fn probe_empty_cursor_times_out_dark() {
        let mut out = Vec::new();
        let mut input = Cursor::new(&[][..]);
        let started = Instant::now();
        let surface = probe_host_surface_inner(&mut out, &mut input, None);
        let elapsed = started.elapsed();
        assert_eq!(surface.appearance, Appearance::Dark);
        assert!(elapsed >= Duration::from_millis(150));
        assert!(elapsed < Duration::from_millis(800));
    }

    #[test]
    fn probe_tty_poll_timeout_on_quiet_pipe() {
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let read_fd = fds[0];
        let write_fd = fds[1];
        let mut reader = unsafe { std::fs::File::from_raw_fd(read_fd) };
        let mut out = Vec::new();
        let started = Instant::now();
        let surface = probe_host_surface(&mut out, &mut reader);
        let elapsed = started.elapsed();
        drop(reader);
        unsafe {
            libc::close(write_fd);
        }
        assert_eq!(surface.appearance, Appearance::Dark);
        assert!(elapsed >= Duration::from_millis(150));
        assert!(elapsed < Duration::from_millis(800));
    }

    #[test]
    fn colorfgbg_bg_index() {
        assert_eq!(appearance_from_colorfgbg("15;0"), Some(Appearance::Dark));
        assert_eq!(appearance_from_colorfgbg("0;15"), Some(Appearance::Light));
        assert_eq!(appearance_from_colorfgbg("7;8"), Some(Appearance::Dark));
    }

    #[test]
    fn mode2031_dsr() {
        assert_eq!(parse_mode2031_dsr(b"\x1b[?997;1n"), Some(Appearance::Dark));
        assert_eq!(parse_mode2031_dsr(b"\x1b[?997;2n"), Some(Appearance::Light));
        assert_eq!(parse_mode2031_dsr(b"\x1b[?997;3n"), None);
        assert_eq!(parse_mode2031_dsr(b"\x1b[A"), None);
    }

    #[test]
    fn osc11_seq_st_terminator_light() {
        let seq = b"\x1b]11;rgb:d5d5/dddd/e0e0\x1b\\";
        let surface = host_surface_from_osc11_seq(seq).unwrap();
        assert_eq!(surface.appearance, Appearance::Light);
        assert_eq!(surface.bg, (0xd5, 0xdd, 0xe0));
    }
}
