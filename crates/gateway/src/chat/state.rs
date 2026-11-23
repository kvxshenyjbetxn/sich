//! The decoded messages of one transcript file: initial load, incremental updates, paging.

use std::io;
use std::path::{Path, PathBuf};

use sich_remote_proto::{ChatBlock, ChatMessage};

use super::Format;
use super::decode::decode_line;
use super::tail::Tailer;

/// Phones may ask for at most this many messages at once.
pub const MAX_PAGE: u32 = 300;
/// Rough size budget of one page, well below the 16 MiB frame limit.
const PAGE_BYTES: usize = 6 * 1024 * 1024;

#[derive(Debug, PartialEq)]
pub enum Change {
    None,
    /// New (or replaced, by id) messages.
    Appended(Vec<ChatMessage>),
    /// The file was truncated or replaced; everything was decoded again.
    Reloaded,
}

pub struct ChatState {
    format: Format,
    tailer: Tailer,
    /// Sorted by `order`; ids are unique.
    messages: Vec<ChatMessage>,
}

fn approx_size(m: &ChatMessage) -> usize {
    64 + m
        .blocks
        .iter()
        .map(|b| match b {
            ChatBlock::Text { markdown } => markdown.len(),
            ChatBlock::ToolCall { call_id, name, summary } => call_id.len() + name.len() + summary.len(),
            ChatBlock::ToolResult { call_id, output, .. } => call_id.len() + output.len(),
            ChatBlock::Image { image_ref, alt } => image_ref.len() + alt.len(),
        })
        .sum::<usize>()
}

/// The slice `[start, end)` of the newest messages before index `end`: at most `limit` messages and
/// about `PAGE_BYTES`, never splitting messages that share an `order` (one transcript line).
pub fn page_bounds(messages: &[ChatMessage], end: usize, limit: usize) -> usize {
    let end = end.min(messages.len());
    let mut start = end.saturating_sub(limit.max(1));
    let mut bytes = 0usize;
    for i in (start..end).rev() {
        bytes += approx_size(&messages[i]);
        if bytes > PAGE_BYTES && i + 1 < end {
            start = i + 1;
            break;
        }
    }
    while start > 0 && start < end && messages[start - 1].order == messages[start].order {
        start -= 1;
    }
    start
}

impl ChatState {
    /// Reads the whole file.
    pub fn open(path: PathBuf, format: Format) -> io::Result<Self> {
        let mut state = Self { format, tailer: Tailer::new(path), messages: Vec::new() };
        state.poll()?;
        Ok(state)
    }

    pub fn path(&self) -> &Path {
        self.tailer.path()
    }

    pub fn format(&self) -> Format {
        self.format
    }

    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    pub fn poll(&mut self) -> io::Result<Change> {
        let poll = self.tailer.poll()?;
        if poll.reset {
            self.messages.clear();
        }
        let mut fresh = Vec::new();
        for line in &poll.lines {
            fresh.extend(decode_line(self.format, line.offset, &line.text).messages);
        }
        if poll.reset {
            self.messages = fresh;
            return Ok(Change::Reloaded);
        }
        if fresh.is_empty() {
            return Ok(Change::None);
        }
        for m in &fresh {
            match self.messages.iter().rposition(|x| x.id == m.id) {
                Some(i) => self.messages[i] = m.clone(),
                None => self.messages.push(m.clone()),
            }
        }
        self.messages.sort_by_key(|m| m.order);
        Ok(Change::Appended(fresh))
    }

    /// The newest `limit` messages; the flag says whether older ones exist.
    pub fn latest(&self, limit: u32) -> (Vec<ChatMessage>, bool) {
        self.page(self.messages.len(), limit)
    }

    /// Up to `limit` messages older than `before_order`.
    pub fn before(&self, before_order: u64, limit: u32) -> (Vec<ChatMessage>, bool) {
        let end = self.messages.partition_point(|m| m.order < before_order);
        self.page(end, limit)
    }

    fn page(&self, end: usize, limit: u32) -> (Vec<ChatMessage>, bool) {
        let start = page_bounds(&self.messages, end, limit.clamp(1, MAX_PAGE) as usize);
        (self.messages[start..end].to_vec(), start > 0)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use sich_remote_proto::ChatRole;

    use super::*;

    fn user(n: usize) -> String {
        format!(r#"{{"type":"user","uuid":"u{n}","message":{{"content":"msg {n}"}}}}"#)
    }

    fn append(path: &Path, lines: &[String]) {
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
    }

    fn ids(ms: &[ChatMessage]) -> Vec<&str> {
        ms.iter().map(|m| m.id.as_str()).collect()
    }

    #[test]
    fn open_decodes_everything_and_poll_reports_appends() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        append(&path, &[user(0), "garbage".into(), r#"{"type":"mode"}"#.into(), user(1)]);
        let mut s = ChatState::open(path.clone(), Format::Claude).unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(s.poll().unwrap(), Change::None);
        append(&path, &[user(2)]);
        let Change::Appended(new) = s.poll().unwrap() else { panic!() };
        assert_eq!(ids(&new), vec!["u2"]);
        assert_eq!(s.len(), 3);
    }

    #[test]
    fn a_partial_line_is_not_decoded_until_complete() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let line = user(5);
        let (a, b) = line.split_at(20);
        std::fs::write(&path, a).unwrap();
        let mut s = ChatState::open(path.clone(), Format::Claude).unwrap();
        assert!(s.is_empty());
        append_raw(&path, b);
        assert_eq!(s.poll().unwrap(), Change::None, "still no newline");
        append_raw(&path, "\n");
        assert!(matches!(s.poll().unwrap(), Change::Appended(m) if m.len() == 1));
    }

    fn append_raw(path: &Path, s: &str) {
        std::fs::OpenOptions::new().append(true).open(path).unwrap().write_all(s.as_bytes()).unwrap();
    }

    #[test]
    fn truncation_reloads_and_drops_old_messages() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        append(&path, &[user(0), user(1), user(2)]);
        let mut s = ChatState::open(path.clone(), Format::Claude).unwrap();
        std::fs::write(&path, format!("{}\n", user(9))).unwrap();
        assert_eq!(s.poll().unwrap(), Change::Reloaded);
        assert_eq!(ids(&s.latest(10).0), vec!["u9"]);
    }

    #[test]
    fn same_id_replaces_the_message() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        append(&path, &[user(0)]);
        let mut s = ChatState::open(path.clone(), Format::Claude).unwrap();
        append(&path, &[r#"{"type":"user","uuid":"u0","message":{"content":"edited"}}"#.into()]);
        s.poll().unwrap();
        let (all, _) = s.latest(10);
        assert_eq!(all.len(), 1);
        assert!(matches!(&all[0].blocks[0], ChatBlock::Text { markdown } if markdown == "edited"));
    }

    #[test]
    fn paging_walks_back_by_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        append(&path, &(0..10).map(user).collect::<Vec<_>>());
        let s = ChatState::open(path, Format::Claude).unwrap();
        let (last, more) = s.latest(4);
        assert_eq!(ids(&last), vec!["u6", "u7", "u8", "u9"]);
        assert!(more);
        let (mid, more) = s.before(last[0].order, 4);
        assert_eq!(ids(&mid), vec!["u2", "u3", "u4", "u5"]);
        assert!(more);
        let (first, more) = s.before(mid[0].order, 4);
        assert_eq!(ids(&first), vec!["u0", "u1"]);
        assert!(!more);
        let (none, more) = s.before(0, 4);
        assert!(none.is_empty() && !more);
        let (all, more) = s.latest(1000);
        assert_eq!((all.len(), more), (10, false));
        assert_eq!(s.latest(0).0.len(), 1, "limit 0 still returns something");
    }

    #[test]
    fn paging_never_splits_the_messages_of_one_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let mixed = r#"{"type":"assistant","uuid":"m","message":{"content":[{"type":"thinking","thinking":"t"},{"type":"text","text":"x"},{"type":"tool_use","id":"c","name":"Read","input":{}}]}}"#;
        append(&path, &[user(0), mixed.into(), user(1)]);
        let s = ChatState::open(path, Format::Claude).unwrap();
        // Asking for the last 2 would cut the 3-message line in half; it is kept whole.
        let (page, more) = s.latest(2);
        assert_eq!(ids(&page), vec!["m", "m#1", "m#2", "u1"]);
        assert!(more);
        assert_eq!(page[0].role, ChatRole::Reasoning);
    }

    #[test]
    fn page_bounds_respects_the_byte_budget() {
        let big = |i: u64| ChatMessage {
            id: format!("b{i}"),
            order: i,
            role: ChatRole::Assistant,
            timestamp_ms: None,
            blocks: vec![ChatBlock::Text { markdown: "x".repeat(2 * 1024 * 1024) }],
        };
        let msgs: Vec<ChatMessage> = (0..10).map(big).collect();
        let start = page_bounds(&msgs, 10, 300);
        assert!((10 - start) * 2 <= 8 && start > 0, "start = {start}");
        assert!(start < 10, "at least one message");
    }

    #[test]
    fn a_missing_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(ChatState::open(dir.path().join("none.jsonl"), Format::Codex).is_err());
    }
}
