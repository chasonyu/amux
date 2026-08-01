//! Host terminal appearance (OSC 11).

use std::io::{self, Read, Write};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Appearance {
    Dark,
    Light,
}

/// BT.601 luma threshold: omp uses bg < 8 on 0..15 scale ≈ mid-gray.
/// We use 0..255 channel avg: `< 128` → Dark.
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

/// Probe host terminal background via OSC 11 + DA1 sentinel.
/// On timeout / parse failure returns [`Appearance::Dark`].
pub fn probe_appearance(out: &mut impl Write, input: &mut impl Read) -> Appearance {
    if out.write_all(b"\x1b]11;?\x07\x1b[c").is_err() {
        return Appearance::Dark;
    }
    let _ = out.flush();

    let deadline = Instant::now() + Duration::from_millis(200);
    let mut collected = Vec::new();
    let mut chunk = [0u8; 256];

    while Instant::now() < deadline {
        match input.read(&mut chunk) {
            Ok(0) => {
                // EOF or no data yet (non-blocking empty). Brief wait then retry.
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(n) => {
                collected.extend_from_slice(&chunk[..n]);
                let s = String::from_utf8_lossy(&collected);
                if let Some((r, g, b)) = parse_osc11_rgb(&s) {
                    return appearance_from_rgb(r, g, b);
                }
                // DA1 reply without OSC 11 ⇒ unsupported.
                if s.contains("\x1b[?") && s.contains('c') {
                    break;
                }
            }
            Err(ref e)
                if e.kind() == io::ErrorKind::WouldBlock
                    || e.kind() == io::ErrorKind::Interrupted =>
            {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => break,
        }
    }

    let s = String::from_utf8_lossy(&collected);
    if let Some((r, g, b)) = parse_osc11_rgb(&s) {
        appearance_from_rgb(r, g, b)
    } else {
        Appearance::Dark
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn parse_osc11_rgb() {
        let s = "\x1b]11;rgb:221d/1d1d/1a1a\x07";
        // `super::` — test name shadows the free function.
        let (r, g, b) = super::parse_osc11_rgb(s).unwrap();
        assert!(r < 0x40 && g < 0x40 && b < 0x40);
    }
}
