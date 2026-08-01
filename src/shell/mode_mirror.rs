//! Mirror child TermMode onto the host terminal with raw escapes.

use std::io::Write;

use anyhow::Result;

use crate::pty::MirroredModes;

#[derive(Debug, Clone, Copy)]
pub struct KbNegotiated {
    pub kitty: bool,
    pub modify_other_keys: bool,
}

impl KbNegotiated {
    /// Best-effort probe: trust TERM / TERM_FEATURES; do not lie to child.
    pub fn probe() -> Self {
        let term = std::env::var("TERM").unwrap_or_default().to_ascii_lowercase();
        let features = std::env::var("TERM_FEATURES").unwrap_or_default();
        let program = std::env::var("TERM_PROGRAM").unwrap_or_default().to_ascii_lowercase();

        let kitty = term.contains("kitty")
            || program.contains("kitty")
            || features.contains("kitty")
            || program.contains("ghostty")
            || term.contains("ghostty");

        // modifyOtherKeys often available in xterm-like + VTE
        let modify_other_keys = kitty
            || term.contains("xterm")
            || term.contains("wezterm")
            || program.contains("wezterm")
            || program.contains("iterm")
            || term.contains("foot");

        Self {
            kitty,
            modify_other_keys,
        }
    }

    pub fn label(self) -> &'static str {
        if self.kitty {
            "kitty"
        } else if self.modify_other_keys {
            "mok"
        } else {
            "legacy"
        }
    }
}

/// Full mouse-off restore for process exit / panic teardown.
/// Do **not** use for Nav focus — see [`apply_nav_host_modes`].
pub fn baseline_host_modes(out: &mut impl Write) -> Result<()> {
    // Disable mouse, bracketed paste, focus, alt-scroll — exit restore.
    write!(
        out,
        "\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1004l\x1b[?2004l\x1b[?1007l"
    )?;
    out.flush()?;
    Ok(())
}

/// Nav / app-start host mouse modes — **same stack as crossterm
/// `EnableMouseCapture` / dux UI mode** (`1000/1002/1003/1015/1006`).
///
/// Do **not** turn `1002`/`1003` off after enabling: some clients (WebSSH /
/// browser terminals inside tmux) only deliver clicks when drag/motion
/// tracking is on. A/B in the same tmux window: dux (full stack) works,
/// amux with only `1000h+1006h` was silent; injected SGR still worked.
///
/// Re-arms mouse + bracketed paste for Nav (paste must stay on so accidental
/// clipboard dumps are wrapped in `\x1b[200~…\x1b[201~` and ignored as keys).
pub fn apply_nav_host_modes(out: &mut impl Write) -> Result<()> {
    write!(
        out,
        "\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1015h\x1b[?1006h\
         \x1b[?1004l\x1b[?1007l\x1b[?2004h"
    )?;
    out.flush()?;
    Ok(())
}

pub fn apply_host_modes(out: &mut impl Write, modes: &MirroredModes) -> Result<()> {
    // Apply absolute enable/disable from child state.
    if modes.bracketed_paste {
        write!(out, "\x1b[?2004h")?;
    } else {
        write!(out, "\x1b[?2004l")?;
    }

    if modes.focus {
        write!(out, "\x1b[?1004h")?;
    } else {
        write!(out, "\x1b[?1004l")?;
    }

    // Mouse: prefer child's most specific mode; if child has none, keep the
    // full EnableMouseCapture / dux floor so amux chrome hit-test still gets
    // events from WebSSH/tmux clients that require 1002/1003.
    write!(out, "\x1b[?1000l\x1b[?1002l\x1b[?1003l")?;
    if modes.mouse_1003 {
        write!(out, "\x1b[?1003h")?;
    } else if modes.mouse_1002 {
        write!(out, "\x1b[?1002h")?;
    } else if modes.mouse_1000 {
        write!(out, "\x1b[?1000h")?;
    } else {
        write!(out, "\x1b[?1000h\x1b[?1002h\x1b[?1003h")?;
    }
    write!(out, "\x1b[?1015h\x1b[?1006h")?;

    // ALTERNATE_SCROLL 1007: only meaningful in alt-screen without mouse
    // modes — translates wheel to CSI A/B. (§4.2.6)
    if modes.alt_screen && modes.alt_scroll && !modes.mouse_1000 && !modes.mouse_1002 && !modes.mouse_1003 {
        write!(out, "\x1b[?1007h")?;
    } else {
        write!(out, "\x1b[?1007l")?;
    }

    // modifyOtherKeys: mirror child level to host. alacritty_terminal 0.26
    // ignores CSI > 4; we tracked the level from raw output and now apply
    // it to the outer terminal. (§4.2.3a.4)
    write!(out, "\x1b[>4;{}m", modes.modify_other_keys)?;

    out.flush()?;
    Ok(())
}
