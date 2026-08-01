//! Streaming splitter for terminal stdin sequences (CSI/OSC/SS3/UTF-8/SGR).

pub const BRACKET_PASTE_START: &[u8] = b"\x1b[200~";
pub const BRACKET_PASTE_END: &[u8] = b"\x1b[201~";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SequenceStatus {
    Complete(usize),
    Incomplete,
}

#[derive(Debug, Clone, Default)]
pub struct RawInputParser {
    pending: Vec<u8>,
    in_bracket_paste: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSequence {
    pub bytes: Vec<u8>,
    /// True if this sequence arrived while already inside bracketed paste
    /// (markers themselves keep the previous paste state for intercept rules).
    pub in_bracket_paste: bool,
}

impl RawInputParser {
    pub fn feed_sequences(&mut self, bytes: &[u8]) -> Vec<ParsedSequence> {
        self.pending.extend_from_slice(bytes);
        let mut sequences = Vec::new();
        loop {
            if self.pending.is_empty() {
                break;
            }
            match scan_one_sequence(&self.pending) {
                SequenceStatus::Complete(len) => {
                    let bytes: Vec<u8> = self.pending.drain(..len).collect();
                    let in_bracket_paste = self.in_bracket_paste;
                    if bytes == BRACKET_PASTE_START {
                        self.in_bracket_paste = true;
                    } else if bytes == BRACKET_PASTE_END {
                        self.in_bracket_paste = false;
                    }
                    sequences.push(ParsedSequence {
                        bytes,
                        in_bracket_paste,
                    });
                }
                SequenceStatus::Incomplete => break,
            }
        }
        sequences
    }

    /// Non-blocking bare Esc resolution — call after a short deadline.
    pub fn resolve_pending_esc(&mut self) -> Option<Vec<u8>> {
        if self.pending == [0x1b] {
            self.pending.clear();
            Some(vec![0x1b])
        } else {
            None
        }
    }

    pub fn in_bracket_paste(&self) -> bool {
        self.in_bracket_paste
    }

    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    pub fn pending_is_bare_esc(&self) -> bool {
        self.pending == [0x1b]
    }
}

/// Split a buffer into complete sequences + incomplete remainder.
pub fn split_sequences(buf: &[u8]) -> (Vec<&[u8]>, &[u8]) {
    let mut sequences = Vec::new();
    let mut i = 0;
    while i < buf.len() {
        let start = i;
        match scan_one_sequence(&buf[start..]) {
            SequenceStatus::Complete(len) => {
                i += len;
                sequences.push(&buf[start..i]);
            }
            SequenceStatus::Incomplete => return (sequences, &buf[start..]),
        }
    }
    (sequences, &buf[buf.len()..])
}

pub fn is_sgr_mouse(seq: &[u8]) -> bool {
    seq.len() >= 6
        && seq[0] == 0x1b
        && seq[1] == b'['
        && seq[2] == b'<'
        && matches!(seq.last(), Some(b'M' | b'm'))
}

fn scan_one_sequence(buf: &[u8]) -> SequenceStatus {
    if buf.is_empty() {
        return SequenceStatus::Incomplete;
    }
    let b = buf[0];
    if b == 0x1b {
        if buf.len() < 2 {
            return SequenceStatus::Incomplete;
        }
        match buf[1] {
            0x1b => SequenceStatus::Complete(1),
            b'[' => {
                let mut i = 2;
                loop {
                    if i >= buf.len() {
                        return SequenceStatus::Incomplete;
                    }
                    let c = buf[i];
                    i += 1;
                    if (0x40..=0x7e).contains(&c) {
                        return SequenceStatus::Complete(i);
                    }
                }
            }
            b'O' => {
                if buf.len() < 3 {
                    SequenceStatus::Incomplete
                } else {
                    SequenceStatus::Complete(3)
                }
            }
            b']' => {
                let mut i = 2;
                loop {
                    if i >= buf.len() {
                        return SequenceStatus::Incomplete;
                    }
                    if buf[i] == 0x07 {
                        return SequenceStatus::Complete(i + 1);
                    }
                    if buf[i] == 0x1b {
                        if i + 1 >= buf.len() {
                            return SequenceStatus::Incomplete;
                        }
                        if buf[i + 1] == b'\\' {
                            return SequenceStatus::Complete(i + 2);
                        }
                        return SequenceStatus::Complete(i);
                    }
                    i += 1;
                }
            }
            _ => SequenceStatus::Complete(2),
        }
    } else if (0xc0..0xfe).contains(&b) {
        let expected = if b < 0xe0 {
            2
        } else if b < 0xf0 {
            3
        } else {
            4
        };
        if expected <= buf.len() {
            SequenceStatus::Complete(expected)
        } else {
            SequenceStatus::Incomplete
        }
    } else {
        SequenceStatus::Complete(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_ascii_csi_and_utf8() {
        let mut input = Vec::new();
        input.push(b'a');
        input.extend_from_slice(b"\x1b[A");
        input.extend_from_slice("中".as_bytes());
        let (seqs, rem) = split_sequences(&input);
        assert_eq!(seqs.len(), 3);
        assert_eq!(seqs[0], b"a");
        assert_eq!(seqs[1], b"\x1b[A");
        assert_eq!(seqs[2], "中".as_bytes());
        assert!(rem.is_empty());
    }

    #[test]
    fn keeps_incomplete_csi() {
        let (seqs, rem) = split_sequences(b"x\x1b[1;");
        assert_eq!(seqs, vec![b"x".as_slice()]);
        assert_eq!(rem, b"\x1b[1;");
    }

    #[test]
    fn bare_esc_incomplete() {
        let (seqs, rem) = split_sequences(b"\x1b");
        assert!(seqs.is_empty());
        assert_eq!(rem, b"\x1b");
    }

    #[test]
    fn sgr_mouse_complete() {
        let seq = b"\x1b[<0;10;20M";
        let (seqs, rem) = split_sequences(seq);
        assert_eq!(seqs, vec![seq.as_slice()]);
        assert!(rem.is_empty());
        assert!(is_sgr_mouse(seq));
    }

    #[test]
    fn parser_tracks_bracket_paste() {
        let mut p = RawInputParser::default();
        let seqs = p.feed_sequences(b"\x1b[200~hi\x1b[201~");
        assert_eq!(seqs.len(), 4); // start, h, i, end
        assert!(!seqs[0].in_bracket_paste);
        assert!(seqs[1].in_bracket_paste);
        assert!(seqs[2].in_bracket_paste);
        assert!(seqs[3].in_bracket_paste);
        assert!(!p.in_bracket_paste());
    }

    #[test]
    fn resolve_pending_esc() {
        let mut p = RawInputParser::default();
        let _ = p.feed_sequences(b"\x1b");
        assert!(p.pending_is_bare_esc());
        assert_eq!(p.resolve_pending_esc(), Some(vec![0x1b]));
        assert!(!p.has_pending());
    }
}
