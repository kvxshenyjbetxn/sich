//! Incremental reader of an append-only JSONL file.
//!
//! `Tailer::poll` returns the complete lines written since the last call. A trailing partial line
//! is left unread (the byte offset stays at its start) and re-read on the next poll. When the file
//! shrinks or its beginning changes (replaced), the tailer starts over from byte 0 and says so.

use std::fs::File;
use std::io::{self, BufRead as _, Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};

/// Most bytes read per step; a longer line without a newline is skipped.
pub const DEFAULT_CHUNK: u64 = 32 * 1024 * 1024;
/// Lines longer than this are not read back by `read_line_at`.
pub const MAX_LINE: u64 = 64 * 1024 * 1024;
/// How many leading bytes identify a file (to notice replacement).
const HEAD_LEN: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawLine {
    /// Byte offset of the line start.
    pub offset: u64,
    pub text: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Poll {
    /// The file was truncated or replaced: earlier lines are void, `lines` start from byte 0.
    pub reset: bool,
    pub lines: Vec<RawLine>,
}

pub struct Tailer {
    path: PathBuf,
    /// Start of the first byte not yet returned as part of a complete line.
    offset: u64,
    head: Vec<u8>,
    /// Inside a line longer than a chunk: drop bytes up to the next newline.
    skipping: bool,
    chunk: u64,
}

impl Tailer {
    pub fn new(path: PathBuf) -> Self {
        Self::with_chunk(path, DEFAULT_CHUNK)
    }

    pub fn with_chunk(path: PathBuf, chunk: u64) -> Self {
        Self { path, offset: 0, head: Vec::new(), skipping: false, chunk: chunk.max(1) }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Everything new, however many chunks that takes.
    pub fn poll(&mut self) -> io::Result<Poll> {
        let mut file = File::open(&self.path)?;
        let len = file.metadata()?.len();
        let mut out = Poll::default();
        if len < self.offset || !self.head_unchanged(&mut file, len)? {
            out.reset = true;
            self.offset = 0;
            self.skipping = false;
            self.head.clear();
        }
        self.refresh_head(&mut file, len)?;
        loop {
            let (lines, full) = self.step(&mut file)?;
            out.lines.extend(lines);
            if !full {
                break;
            }
        }
        Ok(out)
    }

    /// The remembered head still matches the start of the file.
    fn head_unchanged(&self, file: &mut File, len: u64) -> io::Result<bool> {
        if self.head.is_empty() {
            return Ok(true);
        }
        if len < self.head.len() as u64 {
            return Ok(false);
        }
        let mut buf = vec![0u8; self.head.len()];
        file.seek(SeekFrom::Start(0))?;
        file.read_exact(&mut buf)?;
        Ok(buf == self.head)
    }

    fn refresh_head(&mut self, file: &mut File, len: u64) -> io::Result<()> {
        let want = (len as usize).min(HEAD_LEN);
        if self.head.len() >= want {
            return Ok(());
        }
        let mut buf = vec![0u8; want];
        file.seek(SeekFrom::Start(0))?;
        file.read_exact(&mut buf)?;
        self.head = buf;
        Ok(())
    }

    /// One chunk. Returns the complete lines and whether the chunk was full (more may follow).
    fn step(&mut self, file: &mut File) -> io::Result<(Vec<RawLine>, bool)> {
        file.seek(SeekFrom::Start(self.offset))?;
        let mut buf = Vec::new();
        file.by_ref().take(self.chunk).read_to_end(&mut buf)?;
        let full = buf.len() as u64 == self.chunk;
        let mut lines = Vec::new();
        let mut start = 0usize;
        while let Some(rel) = buf[start..].iter().position(|&b| b == b'\n') {
            let end = start + rel;
            if self.skipping {
                self.skipping = false;
            } else {
                let mut raw = &buf[start..end];
                if raw.last() == Some(&b'\r') {
                    raw = &raw[..raw.len() - 1];
                }
                if !raw.iter().all(u8::is_ascii_whitespace) {
                    lines.push(RawLine { offset: self.offset + start as u64, text: String::from_utf8_lossy(raw).into_owned() });
                }
            }
            start = end + 1;
        }
        if start == 0 && full {
            // A single line longer than a whole chunk.
            self.skipping = true;
            self.offset += buf.len() as u64;
            return Ok((lines, true));
        }
        self.offset += start as u64;
        Ok((lines, full && start > 0))
    }
}

/// The line starting at `offset` (without its newline). `None` when `offset` is past the end or
/// the line is longer than `MAX_LINE`.
pub fn read_line_at(path: &Path, offset: u64) -> io::Result<Option<String>> {
    let mut file = File::open(path)?;
    if offset >= file.metadata()?.len() {
        return Ok(None);
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut reader = io::BufReader::new(file.take(MAX_LINE + 1));
    let mut buf = Vec::new();
    reader.read_until(b'\n', &mut buf)?;
    if buf.len() as u64 > MAX_LINE {
        return Ok(None);
    }
    if buf.last() == Some(&b'\n') {
        buf.pop();
    }
    if buf.last() == Some(&b'\r') {
        buf.pop();
    }
    Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;

    fn append(path: &Path, s: &str) {
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
        f.write_all(s.as_bytes()).unwrap();
    }

    fn texts(p: &Poll) -> Vec<(u64, &str)> {
        p.lines.iter().map(|l| (l.offset, l.text.as_str())).collect()
    }

    #[test]
    fn reads_all_then_only_appended_lines_with_offsets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        append(&path, "aa\nbbb\n");
        let mut t = Tailer::new(path.clone());
        let p = t.poll().unwrap();
        assert!(!p.reset);
        assert_eq!(texts(&p), vec![(0, "aa"), (3, "bbb")]);
        assert_eq!(t.poll().unwrap(), Poll::default());
        append(&path, "c\n");
        assert_eq!(texts(&t.poll().unwrap()), vec![(7, "c")]);
    }

    #[test]
    fn partial_last_line_waits_for_its_newline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        append(&path, "one\ntw");
        let mut t = Tailer::new(path.clone());
        assert_eq!(texts(&t.poll().unwrap()), vec![(0, "one")]);
        assert!(t.poll().unwrap().lines.is_empty());
        append(&path, "o\nthree");
        assert_eq!(texts(&t.poll().unwrap()), vec![(4, "two")]);
        append(&path, "\n");
        assert_eq!(texts(&t.poll().unwrap()), vec![(8, "three")]);
    }

    #[test]
    fn crlf_and_blank_lines_are_handled() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        append(&path, "a\r\n\r\n  \nb\n");
        let mut t = Tailer::new(path);
        assert_eq!(texts(&t.poll().unwrap()), vec![(0, "a"), (8, "b")]);
    }

    #[test]
    fn truncation_restarts_from_the_beginning() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        append(&path, "first line\nsecond line\n");
        let mut t = Tailer::new(path.clone());
        t.poll().unwrap();
        std::fs::write(&path, "new\n").unwrap();
        let p = t.poll().unwrap();
        assert!(p.reset);
        assert_eq!(texts(&p), vec![(0, "new")]);
        append(&path, "more\n");
        let p = t.poll().unwrap();
        assert!(!p.reset);
        assert_eq!(texts(&p), vec![(4, "more")]);
    }

    #[test]
    fn replacement_with_a_bigger_file_is_detected_by_its_head() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        append(&path, "old head\n");
        let mut t = Tailer::new(path.clone());
        t.poll().unwrap();
        std::fs::write(&path, "different file entirely\nplus more\n").unwrap();
        let p = t.poll().unwrap();
        assert!(p.reset);
        assert_eq!(texts(&p), vec![(0, "different file entirely"), (24, "plus more")]);
    }

    #[test]
    fn a_missing_file_is_an_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = Tailer::new(dir.path().join("nope.jsonl"));
        assert!(t.poll().is_err());
    }

    #[test]
    fn small_chunks_still_deliver_every_line_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        append(&path, "alpha\nbeta\ngamma\ndelta\n");
        let mut t = Tailer::with_chunk(path, 8);
        assert_eq!(texts(&t.poll().unwrap()), vec![(0, "alpha"), (6, "beta"), (11, "gamma"), (17, "delta")]);
    }

    #[test]
    fn a_line_longer_than_a_chunk_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        append(&path, &format!("ok\n{}\nafter\n", "x".repeat(50)));
        let mut t = Tailer::with_chunk(path, 10);
        let p = t.poll().unwrap();
        assert_eq!(p.lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(), vec!["ok", "after"]);
    }

    #[test]
    fn invalid_utf8_is_replaced_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        std::fs::write(&path, b"a\xffb\n").unwrap();
        let mut t = Tailer::new(path);
        assert_eq!(t.poll().unwrap().lines[0].text, "a\u{fffd}b");
    }

    #[test]
    fn read_line_at_returns_the_line_from_an_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        append(&path, "aa\nbbb\r\nlast");
        assert_eq!(read_line_at(&path, 0).unwrap().as_deref(), Some("aa"));
        assert_eq!(read_line_at(&path, 3).unwrap().as_deref(), Some("bbb"));
        assert_eq!(read_line_at(&path, 8).unwrap().as_deref(), Some("last"));
        assert_eq!(read_line_at(&path, 999).unwrap(), None);
    }
}
