//! AgentMode escape hatch: Ctrl+\ (`\x1c`) with tmux-style double-tap.

use std::time::{Duration, Instant};

/// Default escape byte: Ctrl+\ (FS).
pub const DEFAULT_ESCAPE_BYTE: u8 = 0x1c;

/// Double-tap window for literal forward (spec: 500ms).
pub const DOUBLE_TAP_WINDOW: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscapeAction {
    /// First tap — arm pending; wait for window or other input.
    Armed,
    /// Window elapsed without second tap → toggle to Nav.
    ToggleNav,
    /// Second tap within window → forward one literal escape byte.
    ForwardLiteral,
}

/// Tracks pending first `Ctrl+\` for double-tap detection.
#[derive(Debug, Default)]
pub struct EscapeToggle {
    escape_byte: u8,
    pending_at: Option<Instant>,
    window: Duration,
}

impl EscapeToggle {
    pub fn new(escape_byte: u8) -> Self {
        Self {
            escape_byte,
            pending_at: None,
            window: DOUBLE_TAP_WINDOW,
        }
    }

    pub fn escape_byte(&self) -> u8 {
        self.escape_byte
    }

    pub fn is_escape_seq(&self, seq: &[u8]) -> bool {
        seq == [self.escape_byte]
    }

    /// Handle a complete sequence that matched the escape byte.
    pub fn on_escape(&mut self, now: Instant) -> EscapeAction {
        match self.pending_at {
            Some(t) if now.duration_since(t) <= self.window => {
                self.pending_at = None;
                EscapeAction::ForwardLiteral
            }
            _ => {
                self.pending_at = Some(now);
                EscapeAction::Armed
            }
        }
    }

    /// Call when any non-escape sequence arrives while armed → cancel pending
    /// and treat the armed tap as ToggleNav (consumed already) — actually
    /// per tmux: first tap arms; if other keys arrive before window, first
    /// tap still toggles. Spec: double within 500ms forwards; otherwise toggle.
    ///
    /// So on other input while armed: fire ToggleNav then process other input.
    pub fn on_other_input(&mut self) -> Option<EscapeAction> {
        if self.pending_at.take().is_some() {
            Some(EscapeAction::ToggleNav)
        } else {
            None
        }
    }

    /// Poll deadline: if armed and window elapsed → ToggleNav.
    pub fn poll(&mut self, now: Instant) -> Option<EscapeAction> {
        if let Some(t) = self.pending_at {
            if now.duration_since(t) > self.window {
                self.pending_at = None;
                return Some(EscapeAction::ToggleNav);
            }
        }
        None
    }

    pub fn is_armed(&self) -> bool {
        self.pending_at.is_some()
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.pending_at.map(|t| t + self.window)
    }

    pub fn clear(&mut self) {
        self.pending_at = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_tap_then_timeout_toggles() {
        let mut esc = EscapeToggle::new(DEFAULT_ESCAPE_BYTE);
        let t0 = Instant::now();
        assert_eq!(esc.on_escape(t0), EscapeAction::Armed);
        assert_eq!(
            esc.poll(t0 + DOUBLE_TAP_WINDOW + Duration::from_millis(1)),
            Some(EscapeAction::ToggleNav)
        );
    }

    #[test]
    fn double_tap_forwards_literal() {
        let mut esc = EscapeToggle::new(DEFAULT_ESCAPE_BYTE);
        let t0 = Instant::now();
        assert_eq!(esc.on_escape(t0), EscapeAction::Armed);
        assert_eq!(
            esc.on_escape(t0 + Duration::from_millis(100)),
            EscapeAction::ForwardLiteral
        );
        assert!(!esc.is_armed());
    }

    #[test]
    fn other_input_while_armed_toggles() {
        let mut esc = EscapeToggle::new(DEFAULT_ESCAPE_BYTE);
        let t0 = Instant::now();
        assert_eq!(esc.on_escape(t0), EscapeAction::Armed);
        assert_eq!(esc.on_other_input(), Some(EscapeAction::ToggleNav));
    }

    #[test]
    fn late_second_tap_rearms() {
        let mut esc = EscapeToggle::new(DEFAULT_ESCAPE_BYTE);
        let t0 = Instant::now();
        assert_eq!(esc.on_escape(t0), EscapeAction::Armed);
        // timeout
        assert_eq!(
            esc.poll(t0 + DOUBLE_TAP_WINDOW + Duration::from_millis(1)),
            Some(EscapeAction::ToggleNav)
        );
        // new first tap
        assert_eq!(
            esc.on_escape(t0 + DOUBLE_TAP_WINDOW + Duration::from_millis(10)),
            EscapeAction::Armed
        );
    }
}
