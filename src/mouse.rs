//! SGR mouse translation with four-edge clipping into the agent pane.

use crate::raw_input::is_sgr_mouse;

/// 0-based point-in-rect (matches ratatui `Rect` cell space).
/// Empty rects (`w==0` or `h==0`) never contain a point.
pub fn point_in_rect(x: u16, y: u16, rx: u16, ry: u16, rw: u16, rh: u16) -> bool {
    if rw == 0 || rh == 0 {
        return false;
    }
    x >= rx && y >= ry && x < rx.saturating_add(rw) && y < ry.saturating_add(rh)
}

/// Row index within a list rect for a 0-based screen `y`, or `None` if outside
/// the rect vertically or past `len` (empty/chrome rows).
pub fn list_row_index(y: u16, list_y: u16, list_h: u16, len: usize) -> Option<usize> {
    if list_h == 0 || y < list_y || y >= list_y.saturating_add(list_h) {
        return None;
    }
    let i = (y - list_y) as usize;
    if i >= len {
        return None;
    }
    Some(i)
}

/// Translate SGR mouse coords from screen space into pane-local 1-based cells.
///
/// `pane_x`/`pane_y` are 0-based screen origin of the PTY surface.
/// `pane_w`/`pane_h` are pane size in cells.
///
/// Rejects clicks outside **all four** edges (unlike dux which only guards
/// top/left for fullscreen overlays).
pub fn translate_sgr_mouse_clipped(
    seq: &[u8],
    pane_x: u16,
    pane_y: u16,
    pane_w: u16,
    pane_h: u16,
) -> Option<Vec<u8>> {
    if !is_sgr_mouse(seq) || pane_w == 0 || pane_h == 0 {
        return None;
    }
    let final_byte = *seq.last()?;
    let params = std::str::from_utf8(&seq[3..seq.len() - 1]).ok()?;
    let mut parts = params.split(';');
    let cb: u16 = parts.next()?.parse().ok()?;
    let cx: u16 = parts.next()?.parse().ok()?; // 1-based screen
    let cy: u16 = parts.next()?.parse().ok()?;

    // wire is 1-based; origin is 0-based → local = wire - origin
    let tx = cx.checked_sub(pane_x)?;
    let ty = cy.checked_sub(pane_y)?;
    if tx == 0 || ty == 0 {
        return None; // on or before left/top edge of content
    }
    // Right/bottom: local must be within [1, pane_w] / [1, pane_h]
    if tx > pane_w || ty > pane_h {
        return None;
    }

    Some(format!("\x1b[<{cb};{tx};{ty}{}", final_byte as char).into_bytes())
}

/// Whether Shift is held in an SGR mouse report (bit 2 of Cb) — yield to outer
/// terminal selection; do not forward.
pub fn sgr_has_shift(seq: &[u8]) -> bool {
    if !is_sgr_mouse(seq) {
        return false;
    }
    let Some(params) = std::str::from_utf8(&seq[3..seq.len() - 1]).ok() else {
        return false;
    };
    let Some(cb_s) = params.split(';').next() else {
        return false;
    };
    let Ok(cb) = cb_s.parse::<u16>() else {
        return false;
    };
    cb & 4 != 0
}

/// SGR mouse report is a button release (final byte `m`).
pub fn sgr_is_release(seq: &[u8]) -> bool {
    is_sgr_mouse(seq) && seq.last() == Some(&b'm')
}

/// Wheel notch from an SGR mouse report: `-1` = scroll up (into history),
/// `+1` = scroll down (toward live). `None` if not a wheel event.
///
/// xterm encoding: bit 6 (64) marks wheel; bit 0 selects direction
/// (`64` up, `65` down). Modifier bits are ignored for direction.
pub fn sgr_wheel_delta(seq: &[u8]) -> Option<i32> {
    if !is_sgr_mouse(seq) {
        return None;
    }
    let params = std::str::from_utf8(&seq[3..seq.len() - 1]).ok()?;
    let cb: u16 = params.split(';').next()?.parse().ok()?;
    if cb & 64 == 0 {
        return None;
    }
    // Releases (`m`) for wheel are uncommon; ignore them.
    if seq.last() == Some(&b'm') {
        return None;
    }
    Some(if cb & 1 != 0 { 1 } else { -1 })
}

/// SGR mouse report is a button press (final `M`, motion bit clear).
/// Motion/drag reports also end in `M` but set bit 3 of Cb; a press has it
/// clear. Used to gate the leave-AgentMode button-release on an actual drag.
pub fn sgr_is_button_press(seq: &[u8]) -> bool {
    if !is_sgr_mouse(seq) {
        return false;
    }
    let Some(&last) = seq.last() else {
        return false;
    };
    if last != b'M' {
        return false;
    }
    let Some(params) = std::str::from_utf8(&seq[3..seq.len() - 1]).ok() else {
        return false;
    };
    let Some(cb_s) = params.split(';').next() else {
        return false;
    };
    let Ok(cb) = cb_s.parse::<u16>() else {
        return false;
    };
    // Wheel notches also end in `M` with motion bit clear — not a click.
    if cb & 64 != 0 {
        return false;
    }
    // xterm SGR: bit 5 (32) = motion/drag; bit 3 (8) = Alt/Meta modifier.
    // (Was wrongly using 0x08, which treated Alt+click as non-press.)
    cb & 32 == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_inside_pane() {
        // pane at (10, 5), size 40x20; click screen (15, 10) → local (5, 5)
        // wire 1-based: cx=15 means screen col 14 (0-based) if we think 0-based...
        // Actually: wire cx=15, pane_x=10 → tx = 15-10 = 5 ✓
        let seq = b"\x1b[<0;15;10M";
        let out = translate_sgr_mouse_clipped(seq, 10, 5, 40, 20).unwrap();
        assert_eq!(out, b"\x1b[<0;5;5M");
    }

    #[test]
    fn rejects_left_of_pane() {
        let seq = b"\x1b[<0;5;10M"; // cx=5, pane_x=10 → underflow
        assert!(translate_sgr_mouse_clipped(seq, 10, 5, 40, 20).is_none());
    }

    #[test]
    fn rejects_right_of_pane() {
        // pane_w=40, pane_x=10 → max wire cx = 10+40 = 50
        let seq = b"\x1b[<0;51;10M";
        assert!(translate_sgr_mouse_clipped(seq, 10, 5, 40, 20).is_none());
    }

    #[test]
    fn rejects_below_pane() {
        // pane_h=20, pane_y=5 → max wire cy = 25
        let seq = b"\x1b[<0;15;26M";
        assert!(translate_sgr_mouse_clipped(seq, 10, 5, 40, 20).is_none());
    }

    #[test]
    fn accepts_bottom_right_corner() {
        let seq = b"\x1b[<0;50;25M"; // tx=40, ty=20
        let out = translate_sgr_mouse_clipped(seq, 10, 5, 40, 20).unwrap();
        assert_eq!(out, b"\x1b[<0;40;20M");
    }

    #[test]
    fn detects_shift() {
        assert!(sgr_has_shift(b"\x1b[<4;10;10M"));
        assert!(!sgr_has_shift(b"\x1b[<0;10;10M"));
    }

    #[test]
    fn wheel_delta_up_down() {
        assert_eq!(sgr_wheel_delta(b"\x1b[<64;10;10M"), Some(-1));
        assert_eq!(sgr_wheel_delta(b"\x1b[<65;10;10M"), Some(1));
        assert_eq!(sgr_wheel_delta(b"\x1b[<0;10;10M"), None);
        assert_eq!(sgr_wheel_delta(b"\x1b[<64;10;10m"), None);
    }

    #[test]
    fn release_detected_by_final_m() {
        assert!(sgr_is_release(b"\x1b[<0;10;10m"));
        assert!(!sgr_is_release(b"\x1b[<0;10;10M")); // press, not release
        assert!(!sgr_is_release(b"not a mouse seq"));
    }

    #[test]
    fn press_detected_when_motion_bit_clear() {
        // cb=0, motion bit (32) clear → press
        assert!(sgr_is_button_press(b"\x1b[<0;10;10M"));
        // cb=32 = motion/drag → not a press
        assert!(!sgr_is_button_press(b"\x1b[<32;10;10M"));
        // cb=8 is Alt modifier, still a press
        assert!(sgr_is_button_press(b"\x1b[<8;10;10M"));
        // release final 'm' is not a press
        assert!(!sgr_is_button_press(b"\x1b[<0;10;10m"));
        assert!(!sgr_is_button_press(b"not a mouse seq"));
        // Wheel is not a click (must not attach / hit-test).
        assert!(!sgr_is_button_press(b"\x1b[<64;10;10M"));
        assert!(!sgr_is_button_press(b"\x1b[<65;10;10M"));
    }

    #[test]
    fn point_in_rect_basic() {
        assert!(point_in_rect(5, 5, 0, 0, 10, 10));
        assert!(!point_in_rect(10, 5, 0, 0, 10, 10)); // right edge exclusive
        assert!(!point_in_rect(5, 10, 0, 0, 10, 10));
        assert!(!point_in_rect(0, 0, 0, 0, 0, 10)); // empty width
        assert!(!point_in_rect(0, 0, 0, 0, 10, 0)); // empty height
    }

    #[test]
    fn list_row_index_from_fixture() {
        // ws_list at y=2, h=7; three workspaces
        assert_eq!(list_row_index(2, 2, 7, 3), Some(0));
        assert_eq!(list_row_index(4, 2, 7, 3), Some(2));
        assert_eq!(list_row_index(5, 2, 7, 3), None); // past len → chrome
        assert_eq!(list_row_index(1, 2, 7, 3), None); // above list
        // sess_list TOP-inner at y=10, h=20; two sessions
        assert_eq!(list_row_index(10, 10, 20, 2), Some(0));
        assert_eq!(list_row_index(11, 10, 20, 2), Some(1));
        assert_eq!(list_row_index(12, 10, 20, 2), None);
    }

    #[test]
    fn dispatcher_gates_on_press_only() {
        // Shell hit-test must ignore release/motion (B1).
        assert!(sgr_is_button_press(b"\x1b[<0;5;5M"));
        assert!(!sgr_is_button_press(b"\x1b[<0;5;5m"));
        assert!(!sgr_is_button_press(b"\x1b[<32;5;5M")); // motion bit
        assert!(!sgr_is_release(b"\x1b[<0;5;5M"));
        assert!(sgr_is_release(b"\x1b[<0;5;5m"));
    }
}
