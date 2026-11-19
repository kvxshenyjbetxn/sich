//! Incremental scanner for the few terminal escape sequences the daemon cares about:
//! OSC 0 / OSC 2 (window title) and a standalone BEL. It is fed raw PTY chunks and keeps its
//! state between them, so sequences split across chunks are handled.

/// Titles longer than this many characters are truncated.
pub const MAX_TITLE_CHARS: usize = 256;
/// Raw bytes kept for a title body (4 bytes per char is the UTF-8 maximum).
const MAX_TITLE_BYTES: usize = MAX_TITLE_CHARS * 4;
/// Longest OSC command number we bother to parse.
const MAX_OSC_NUM: u32 = 9999;

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;
const CAN: u8 = 0x18;
const SUB: u8 = 0x1a;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OscEvent {
    /// OSC 0 / OSC 2: the terminal title was set (possibly to an empty string).
    Title(String),
    /// A BEL outside any OSC sequence.
    Bell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum St {
    Ground,
    /// After ESC in the ground state.
    Esc,
    /// After `ESC ]`, reading the command number.
    OscNum { num: u32, digits: u8 },
    /// Collecting the body of an OSC 0 / 2.
    OscTitle,
    /// Inside an OSC we do not care about; wait for its terminator.
    OscSkip,
    /// Saw ESC inside an OSC: `\` ends it (emitting the title if `title`), anything else aborts.
    OscEsc { title: bool },
}

#[derive(Debug)]
pub struct OscScanner {
    st: St,
    buf: Vec<u8>,
}

impl Default for OscScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl OscScanner {
    pub fn new() -> Self {
        Self { st: St::Ground, buf: Vec::new() }
    }

    /// Feeds a chunk; calls `emit` for every recognised event, in order.
    pub fn feed(&mut self, bytes: &[u8], mut emit: impl FnMut(OscEvent)) {
        for &b in bytes {
            self.step(b, &mut emit);
        }
    }

    fn finish_title(&mut self, emit: &mut impl FnMut(OscEvent)) {
        let text = String::from_utf8_lossy(&self.buf);
        let title: String = text.chars().take(MAX_TITLE_CHARS).collect();
        self.buf.clear();
        emit(OscEvent::Title(title));
    }

    fn step(&mut self, b: u8, emit: &mut impl FnMut(OscEvent)) {
        match self.st {
            St::Ground => match b {
                ESC => self.st = St::Esc,
                BEL => emit(OscEvent::Bell),
                _ => {}
            },
            St::Esc => match b {
                b']' => self.st = St::OscNum { num: 0, digits: 0 },
                ESC => {}
                _ => {
                    self.st = St::Ground;
                    if b == BEL {
                        emit(OscEvent::Bell);
                    }
                }
            },
            St::OscNum { num, digits } => match b {
                b'0'..=b'9' => {
                    let num = (num * 10 + u32::from(b - b'0')).min(MAX_OSC_NUM + 1);
                    self.st = St::OscNum { num, digits: digits.saturating_add(1) };
                }
                b';' => {
                    if digits > 0 && (num == 0 || num == 2) {
                        self.buf.clear();
                        self.st = St::OscTitle;
                    } else {
                        self.st = St::OscSkip;
                    }
                }
                BEL | CAN | SUB => self.st = St::Ground,
                ESC => self.st = St::OscEsc { title: false },
                _ => self.st = St::OscSkip,
            },
            St::OscTitle => match b {
                BEL => {
                    self.finish_title(emit);
                    self.st = St::Ground;
                }
                ESC => self.st = St::OscEsc { title: true },
                CAN | SUB => {
                    self.buf.clear();
                    self.st = St::Ground;
                }
                _ => {
                    if self.buf.len() < MAX_TITLE_BYTES {
                        self.buf.push(b);
                    }
                }
            },
            St::OscSkip => match b {
                BEL | CAN | SUB => self.st = St::Ground,
                ESC => self.st = St::OscEsc { title: false },
                _ => {}
            },
            St::OscEsc { title } => {
                if b == b'\\' {
                    if title {
                        self.finish_title(emit);
                    }
                    self.st = St::Ground;
                } else {
                    // Not an ST: the OSC was aborted and this byte follows an ESC.
                    self.buf.clear();
                    self.st = St::Esc;
                    self.step(b, emit);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan_chunks(chunks: &[&[u8]]) -> Vec<OscEvent> {
        let mut s = OscScanner::new();
        let mut out = Vec::new();
        for c in chunks {
            s.feed(c, |e| out.push(e));
        }
        out
    }

    fn title(s: &str) -> OscEvent {
        OscEvent::Title(s.to_string())
    }

    #[test]
    fn osc0_with_bel() {
        assert_eq!(scan_chunks(&[b"\x1b]0;hello\x07"]), vec![title("hello")]);
    }

    #[test]
    fn osc2_with_bel() {
        assert_eq!(scan_chunks(&[b"\x1b]2;world\x07"]), vec![title("world")]);
    }

    #[test]
    fn osc0_and_osc2_with_st() {
        assert_eq!(scan_chunks(&[b"\x1b]0;a\x1b\\"]), vec![title("a")]);
        assert_eq!(scan_chunks(&[b"\x1b]2;b\x1b\\"]), vec![title("b")]);
    }

    #[test]
    fn bel_inside_osc_is_not_a_bell() {
        let ev = scan_chunks(&[b"\x1b]0;t\x07"]);
        assert_eq!(ev, vec![title("t")]);
        assert!(!ev.contains(&OscEvent::Bell));
    }

    #[test]
    fn bel_terminating_other_osc_is_not_a_bell() {
        assert!(scan_chunks(&[b"\x1b]7;file:///c/x\x07"]).is_empty());
        assert!(scan_chunks(&[b"\x1b]133;A\x07"]).is_empty());
    }

    #[test]
    fn standalone_bel_counts() {
        assert_eq!(scan_chunks(&[b"abc\x07def\x07"]), vec![OscEvent::Bell, OscEvent::Bell]);
    }

    #[test]
    fn bell_after_title_is_counted() {
        assert_eq!(
            scan_chunks(&[b"\x1b]0;x\x07\x07"]),
            vec![title("x"), OscEvent::Bell]
        );
    }

    #[test]
    fn other_osc_numbers_are_ignored() {
        assert!(scan_chunks(&[b"\x1b]1;icon\x07", b"\x1b]10;?\x1b\\"]).is_empty());
        assert!(scan_chunks(&[b"\x1b]20;x\x07"]).is_empty());
        assert!(scan_chunks(&[b"\x1b]02;x\x07"]).len() == 1, "leading zero is still OSC 2");
    }

    #[test]
    fn split_at_every_byte_position() {
        let inputs: [&[u8]; 4] = [
            b"pre\x1b]0;my title\x07post",
            b"pre\x1b]2;my title\x1b\\post",
            "\x1b]0;\u{2733} Claude \u{1F600}\x07\x07".as_bytes(),
            b"\x1b]7;cwd\x07\x1b]0;t\x07 \x07",
        ];
        for input in inputs {
            let whole = scan_chunks(&[input]);
            assert!(!whole.is_empty());
            for cut in 0..=input.len() {
                let (a, b) = input.split_at(cut);
                assert_eq!(scan_chunks(&[a, b]), whole, "cut at {cut} of {input:?}");
            }
            // Fully byte-by-byte.
            let singles: Vec<&[u8]> = input.chunks(1).collect();
            assert_eq!(scan_chunks(&singles), whole);
        }
    }

    #[test]
    fn utf8_title_split_in_the_middle_of_a_char() {
        let input = "\x1b]0;\u{2733} ok\x07".as_bytes();
        let singles: Vec<&[u8]> = input.chunks(1).collect();
        assert_eq!(scan_chunks(&singles), vec![title("\u{2733} ok")]);
    }

    #[test]
    fn oversized_title_is_truncated() {
        let long = "x".repeat(10_000);
        let input = format!("\x1b]0;{long}\x07");
        let ev = scan_chunks(&[input.as_bytes()]);
        assert_eq!(ev, vec![title(&"x".repeat(MAX_TITLE_CHARS))]);
        // Multi-byte chars: counted in chars, not bytes.
        let long = "\u{2733}".repeat(2_000);
        let input = format!("\x1b]2;{long}\x1b\\");
        let ev = scan_chunks(&[input.as_bytes()]);
        assert_eq!(ev, vec![title(&"\u{2733}".repeat(MAX_TITLE_CHARS))]);
    }

    #[test]
    fn title_exactly_at_limit_is_kept() {
        let t = "y".repeat(MAX_TITLE_CHARS);
        let input = format!("\x1b]0;{t}\x07");
        assert_eq!(scan_chunks(&[input.as_bytes()]), vec![title(&t)]);
    }

    #[test]
    fn invalid_utf8_is_lossy() {
        let ev = scan_chunks(&[b"\x1b]0;a\xff\xfeb\x07"]);
        assert_eq!(ev, vec![title("a\u{FFFD}\u{FFFD}b")]);
    }

    #[test]
    fn empty_title_is_reported() {
        assert_eq!(scan_chunks(&[b"\x1b]0;\x07"]), vec![title("")]);
    }

    #[test]
    fn garbage_input_does_not_panic_or_emit() {
        let mut data = Vec::new();
        for i in 0..4096u32 {
            data.push((i.wrapping_mul(2654435761) >> 7) as u8 | 0x08);
        }
        let mut s = OscScanner::new();
        s.feed(&data, |_| {});
        // Still usable afterwards.
        let mut out = Vec::new();
        s.feed(b"\x1b\\\x1b]0;ok\x07", |e| out.push(e));
        assert_eq!(out.last(), Some(&title("ok")));
    }

    #[test]
    fn random_bytes_never_panic() {
        let mut x = 0x1234_5678u32;
        let mut data = Vec::new();
        for _ in 0..50_000 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            data.push(x as u8);
        }
        let mut s = OscScanner::new();
        for chunk in data.chunks(97) {
            s.feed(chunk, |_| {});
        }
    }

    #[test]
    fn truncated_sequences_emit_nothing() {
        assert!(scan_chunks(&[b"\x1b]0;never ends"]).is_empty());
        assert!(scan_chunks(&[b"\x1b]"]).is_empty());
        assert!(scan_chunks(&[b"\x1b"]).is_empty());
    }

    #[test]
    fn aborted_osc_by_other_escape_then_new_osc() {
        // ESC ] 0 ; a ESC ] 0 ; b BEL  -> the first OSC is dropped, the second one wins.
        assert_eq!(scan_chunks(&[b"\x1b]0;a\x1b]0;b\x07"]), vec![title("b")]);
    }

    #[test]
    fn can_aborts_title() {
        assert_eq!(scan_chunks(&[b"\x1b]0;a\x18zz\x07"]), vec![OscEvent::Bell]);
    }

    #[test]
    fn csi_sequences_pass_through() {
        assert_eq!(
            scan_chunks(&[b"\x1b[31mred\x1b[0m\x07"]),
            vec![OscEvent::Bell]
        );
    }

    #[test]
    fn two_titles_in_one_chunk() {
        assert_eq!(
            scan_chunks(&[b"\x1b]0;one\x07xx\x1b]2;two\x1b\\"]),
            vec![title("one"), title("two")]
        );
    }
}
