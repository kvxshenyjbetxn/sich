//! Streaming pre-filter that cuts inline-image escape sequences out of the PTY byte stream.
//!
//! alacritty_terminal ignores APC and knows nothing about images, so everything else (the
//! `Bytes` segments, in their original order) goes to it unchanged while the image commands are
//! handed out separately. Sequences may be split across any number of `push` calls.

/// Longest accepted sequence body; a longer one is dropped and the stream resyncs after it.
pub const MAX_SEQUENCE: usize = 8 * 1024 * 1024;

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;
const CAN: u8 = 0x18;
const SUB: u8 = 0x1a;
const ITERM_PREFIX: &[u8] = b"1337;";
/// Longest DCS parameter string examined when looking for a Sixel `q` introducer.
const MAX_DCS_PARAMS: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    /// Ordinary terminal output.
    Bytes(Vec<u8>),
    /// Kitty graphics command: `ESC _ G <control> ; <payload> ESC \`.
    Kitty { control: Vec<u8>, payload: Vec<u8> },
    /// iTerm2 `ESC ] 1337 ; <body>` (body is everything after `1337;`).
    Iterm { body: Vec<u8> },
    /// A Sixel image; it is recognized and dropped.
    Sixel,
    /// `CSI 16 t` (cell size in pixels), which alacritty_terminal does not answer.
    CellSizeQuery,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Kitty,
    Iterm,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Ground,
    /// `held` contains an ESC and the prefix of a possible image sequence.
    Held,
    Body { kind: Kind, esc: bool, overflow: bool },
    Sixel { esc: bool },
}

enum Decision {
    NeedMore,
    Start(Kind),
    StartSixel,
    CellSizeQuery,
    Reject,
}

pub struct Interceptor {
    state: State,
    held: Vec<u8>,
    body: Vec<u8>,
    max: usize,
}

impl Default for Interceptor {
    fn default() -> Self {
        Self::with_limit(MAX_SEQUENCE)
    }
}

fn decide(held: &[u8], b: u8) -> Decision {
    match held {
        [ESC] => match b {
            b'_' | b']' | b'P' | b'[' => Decision::NeedMore,
            _ => Decision::Reject,
        },
        [ESC, b'['] => if b == b'1' { Decision::NeedMore } else { Decision::Reject },
        [ESC, b'[', b'1'] => if b == b'6' { Decision::NeedMore } else { Decision::Reject },
        [ESC, b'[', b'1', b'6'] => if b == b't' { Decision::CellSizeQuery } else { Decision::Reject },
        [ESC, b'_'] => {
            if b == b'G' {
                Decision::Start(Kind::Kitty)
            } else {
                Decision::Reject
            }
        }
        [ESC, b']', rest @ ..] => {
            if rest.len() < ITERM_PREFIX.len() && b == ITERM_PREFIX[rest.len()] {
                if rest.len() + 1 == ITERM_PREFIX.len() {
                    Decision::Start(Kind::Iterm)
                } else {
                    Decision::NeedMore
                }
            } else {
                Decision::Reject
            }
        }
        [ESC, b'P', params @ ..] => match b {
            b'q' => Decision::StartSixel,
            b'0'..=b'9' | b';' | b':' if params.len() < MAX_DCS_PARAMS => Decision::NeedMore,
            _ => Decision::Reject,
        },
        _ => Decision::Reject,
    }
}

fn flush(normal: &mut Vec<u8>, out: &mut Vec<Segment>) {
    if !normal.is_empty() {
        out.push(Segment::Bytes(std::mem::take(normal)));
    }
}

impl Interceptor {
    pub fn with_limit(max: usize) -> Self {
        Interceptor { state: State::Ground, held: Vec::new(), body: Vec::new(), max }
    }

    pub fn push(&mut self, data: &[u8]) -> Vec<Segment> {
        let mut out = Vec::new();
        let mut normal: Vec<u8> = Vec::new();
        let mut i = 0;
        while i < data.len() {
            match self.state {
                State::Ground => match data[i..].iter().position(|&b| b == ESC) {
                    None => {
                        normal.extend_from_slice(&data[i..]);
                        i = data.len();
                    }
                    Some(p) => {
                        normal.extend_from_slice(&data[i..i + p]);
                        i += p + 1;
                        self.held.clear();
                        self.held.push(ESC);
                        self.state = State::Held;
                    }
                },
                State::Held => {
                    let b = data[i];
                    match decide(&self.held, b) {
                        Decision::NeedMore => {
                            self.held.push(b);
                            i += 1;
                        }
                        Decision::Start(kind) => {
                            self.held.clear();
                            self.body.clear();
                            self.state = State::Body { kind, esc: false, overflow: false };
                            i += 1;
                        }
                        Decision::StartSixel => {
                            self.held.clear();
                            self.state = State::Sixel { esc: false };
                            i += 1;
                        }
                        Decision::CellSizeQuery => {
                            self.held.clear();
                            self.state = State::Ground;
                            flush(&mut normal, &mut out);
                            out.push(Segment::CellSizeQuery);
                            i += 1;
                        }
                        Decision::Reject => {
                            // Not ours: pass the held bytes on and look at `b` again.
                            normal.append(&mut self.held);
                            self.state = State::Ground;
                        }
                    }
                }
                State::Body { kind, esc, overflow } => {
                    if esc {
                        if data[i] == b'\\' {
                            i += 1;
                            self.state = State::Ground;
                            if !overflow {
                                flush(&mut normal, &mut out);
                                out.push(self.finish(kind));
                            }
                        } else {
                            // ESC inside a body that is not ST: the sequence is aborted and
                            // the ESC starts something new.
                            self.body.clear();
                            self.held.clear();
                            self.held.push(ESC);
                            self.state = State::Held;
                        }
                        continue;
                    }
                    let end = data[i..]
                        .iter()
                        .position(|&b| matches!(b, ESC | BEL | CAN | SUB))
                        .map_or(data.len(), |p| i + p);
                    let mut overflow = overflow;
                    if !overflow {
                        if self.body.len() + (end - i) > self.max {
                            overflow = true;
                            self.body = Vec::new();
                        } else {
                            self.body.extend_from_slice(&data[i..end]);
                        }
                    }
                    if end == data.len() {
                        i = end;
                        self.state = State::Body { kind, esc: false, overflow };
                        continue;
                    }
                    let term = data[end];
                    i = end + 1;
                    match term {
                        ESC => self.state = State::Body { kind, esc: true, overflow },
                        BEL => {
                            self.state = State::Ground;
                            if !overflow {
                                flush(&mut normal, &mut out);
                                out.push(self.finish(kind));
                            }
                        }
                        _ => {
                            self.body.clear();
                            self.state = State::Ground;
                        }
                    }
                }
                State::Sixel { esc } => {
                    if esc {
                        if data[i] == b'\\' {
                            i += 1;
                            self.state = State::Ground;
                            flush(&mut normal, &mut out);
                            out.push(Segment::Sixel);
                        } else {
                            self.held.clear();
                            self.held.push(ESC);
                            self.state = State::Held;
                        }
                        continue;
                    }
                    match data[i..].iter().position(|&b| matches!(b, ESC | CAN | SUB)) {
                        None => i = data.len(),
                        Some(p) => {
                            let b = data[i + p];
                            i += p + 1;
                            self.state = if b == ESC { State::Sixel { esc: true } } else { State::Ground };
                        }
                    }
                }
            }
        }
        flush(&mut normal, &mut out);
        out
    }

    fn finish(&mut self, kind: Kind) -> Segment {
        let body = std::mem::take(&mut self.body);
        match kind {
            Kind::Iterm => Segment::Iterm { body },
            Kind::Kitty => match body.iter().position(|&b| b == b';') {
                Some(p) => Segment::Kitty { control: body[..p].to_vec(), payload: body[p + 1..].to_vec() },
                None => Segment::Kitty { control: body, payload: Vec::new() },
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(chunks: &[&[u8]]) -> Vec<Segment> {
        run_with(Interceptor::default(), chunks)
    }

    /// Runs the chunks and merges adjacent `Bytes` segments (chunking may split them).
    fn run_with(mut ic: Interceptor, chunks: &[&[u8]]) -> Vec<Segment> {
        let mut out: Vec<Segment> = Vec::new();
        for c in chunks {
            for s in ic.push(c) {
                match (out.last_mut(), s) {
                    (Some(Segment::Bytes(a)), Segment::Bytes(b)) => a.extend(b),
                    (_, s) => out.push(s),
                }
            }
        }
        out
    }

    fn bytes(b: &[u8]) -> Segment {
        Segment::Bytes(b.to_vec())
    }

    fn kitty(c: &[u8], p: &[u8]) -> Segment {
        Segment::Kitty { control: c.to_vec(), payload: p.to_vec() }
    }

    #[test]
    fn plain_text_and_other_sequences_pass_through_unchanged() {
        let input: &[u8] = b"hi\x1b[31mred\x1b]0;title\x07\x1b_Xother\x1b\\\x1bP1$rx\x1b\\\x1b]52;c;YQ==\x07end";
        assert_eq!(run(&[input]), vec![bytes(input)]);
    }

    #[test]
    fn kitty_command_with_st_is_extracted_in_order() {
        let r = run(&[b"before\x1b_Ga=T,f=100;QUJD\x1b\\after"]);
        assert_eq!(r, vec![bytes(b"before"), kitty(b"a=T,f=100", b"QUJD"), bytes(b"after")]);
    }

    #[test]
    fn kitty_command_without_payload_and_with_bel() {
        let r = run(&[b"\x1b_Ga=d,d=a\x1b\\", b"\x1b_Gi=1;AA\x07x"]);
        assert_eq!(r, vec![kitty(b"a=d,d=a", b""), kitty(b"i=1", b"AA"), bytes(b"x")]);
    }

    #[test]
    fn iterm_command_with_bel_and_st() {
        let r = run(&[b"a\x1b]1337;File=inline=1:QUJD\x07b\x1b]1337;File=inline=1:REU=\x1b\\c"]);
        assert_eq!(
            r,
            vec![
                bytes(b"a"),
                Segment::Iterm { body: b"File=inline=1:QUJD".to_vec() },
                bytes(b"b"),
                Segment::Iterm { body: b"File=inline=1:REU=".to_vec() },
                bytes(b"c"),
            ]
        );
    }

    #[test]
    fn sixel_is_dropped() {
        let r = run(&[b"x\x1bPq\"1;1;4;4#0;2;0;0;0~~~~\x1b\\y\x1bP0;1;0q~-~\x1b\\z"]);
        assert_eq!(r, vec![bytes(b"x"), Segment::Sixel, bytes(b"y"), Segment::Sixel, bytes(b"z")]);
    }

    #[test]
    fn splitting_at_every_boundary_gives_the_same_result() {
        let input: &[u8] =
            b"a\x1b_Gi=1,m=1;QUJD\x1b\\b\x1b]1337;File=inline=1:QQ==\x07c\x1bPq~~\x1b\\d\x1b[0m\x1b]0;t\x07e";
        let whole = run(&[input]);
        for cut in 1..input.len() {
            assert_eq!(run(&[&input[..cut], &input[cut..]]), whole, "cut at {cut}");
        }
        let singles: Vec<&[u8]> = input.chunks(1).collect();
        assert_eq!(run(&singles), whole);
    }

    #[test]
    fn cell_size_query_is_recognized_and_similar_csi_passes_through() {
        let r = run(&[b"a\x1b[16tb"]);
        assert_eq!(r, vec![bytes(b"a"), Segment::CellSizeQuery, bytes(b"b")]);
        let input: &[u8] = b"\x1b[16m\x1b[1;5H\x1b[1mx\x1b[160t\x1b[16;1t\x1b[";
        let r = run(&[input]);
        assert_eq!(r, vec![bytes(&input[..input.len() - 2])]);
        assert_eq!(run(&[b"\x1b[1", b"6", b"t"]), vec![Segment::CellSizeQuery]);
    }

    #[test]
    fn lone_escape_is_held_until_the_next_chunk() {
        let mut ic = Interceptor::default();
        assert_eq!(ic.push(b"x\x1b"), vec![bytes(b"x")]);
        assert_eq!(ic.push(b"[1m"), vec![bytes(b"\x1b[1m")]);
    }

    #[test]
    fn partial_prefixes_that_do_not_match_are_released_intact() {
        for input in [&b"\x1b]133;A\x07"[..], b"\x1b_Hx\x1b\\", b"\x1bP1;2p\x1b\\", b"\x1b]1338;x\x07", b"\x1b\x1b[0m"] {
            assert_eq!(run(&[input]), vec![bytes(input)], "{input:?}");
        }
    }

    #[test]
    fn oversized_sequence_is_dropped_and_the_stream_resyncs() {
        let mut big = b"\x1b_Ga=t;".to_vec();
        big.extend(std::iter::repeat_n(b'A', 100));
        big.extend_from_slice(b"\x1b\\after\x1b_Gi=2;OK\x1b\\");
        let ic = Interceptor::with_limit(50);
        let r = run_with(ic, &[&big[..30], &big[30..]]);
        assert_eq!(r, vec![bytes(b"after"), kitty(b"i=2", b"OK")]);
    }

    #[test]
    fn aborted_sequences_resync() {
        // ESC + non-backslash inside a body aborts it; the new ESC sequence is kept.
        let r = run(&[b"\x1b_Ga=t;AAA\x1b[1mtext"]);
        assert_eq!(r, vec![bytes(b"\x1b[1mtext")]);
        // CAN cancels.
        let r = run(&[b"\x1b_Ga=t;AAA\x18text"]);
        assert_eq!(r, vec![bytes(b"text")]);
        // An unfinished sequence followed by a new command.
        let r = run(&[b"\x1b_Ga=t;AAA\x1b_Gi=1;B\x1b\\"]);
        assert_eq!(r, vec![kitty(b"i=1", b"B")]);
    }
}
