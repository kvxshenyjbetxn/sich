//! The decoded messages of one transcript file: initial load, incremental updates, paging.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

use sich_remote_proto::{ChatBlock, ChatMessage};

use super::Format;
use super::decode::{Link, decode_line};
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
    /// Uuids of user lines that carry an inline image.
    inline_image_uuids: HashSet<String>,
    /// Whether the latest real (non-meta) user line carried an inline image.
    last_user_inline: bool,
}

/// True when an `isMeta` image companion repeats an image its user message already shows: its
/// `parentUuid` names a user line with an inline image, or (no parent) the preceding user line has one.
fn is_duplicate_companion(link: &Link, inline_uuids: &HashSet<String>, last_user_inline: bool) -> bool {
    match &link.parent_uuid {
        Some(p) => inline_uuids.contains(p),
        None => last_user_inline,
    }
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
        let mut state = Self { format, tailer: Tailer::new(path), messages: Vec::new(), inline_image_uuids: HashSet::new(), last_user_inline: false };
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
            self.inline_image_uuids.clear();
            self.last_user_inline = false;
        }
        let mut fresh = Vec::new();
        for line in &poll.lines {
            let d = decode_line(self.format, line.offset, &line.text);
            if let Some(link) = &d.link {
                if link.meta {
                    if !d.messages.is_empty() && is_duplicate_companion(link, &self.inline_image_uuids, self.last_user_inline) {
                        continue;
                    }
                } else if !d.messages.is_empty() {
                    self.last_user_inline = link.inline_image;
                    if let (true, Some(u)) = (link.inline_image, &link.uuid) {
                        self.inline_image_uuids.insert(u.clone());
                    }
                }
            }
            fresh.extend(d.messages);
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

    fn image_count(ms: &[ChatMessage]) -> usize {
        ms.iter().flat_map(|m| &m.blocks).filter(|b| matches!(b, ChatBlock::Image { .. })).count()
    }

    const USER_WITH_IMAGE: &str = r#"{"type":"user","uuid":"u1","parentUuid":"p0","message":{"role":"user","content":[{"type":"image","source":{"type":"base64","media_type":"image/jpeg","data":"QUJD"}},{"type":"text","text":"[Image #1] тест"}]}}"#;
    const USER_NO_IMAGE: &str = r#"{"type":"user","uuid":"u1","parentUuid":"p0","message":{"role":"user","content":[{"type":"text","text":"[Image #1] тест"}]}}"#;
    const COMPANION: &str = r#"{"type":"user","uuid":"c1","parentUuid":"u1","isMeta":true,"message":{"role":"user","content":[{"type":"text","text":"[Image: source: C:\\tmp\\a.jpg]"}]}}"#;
    const COMPANION_NO_PARENT: &str = r#"{"type":"user","uuid":"c1","isMeta":true,"message":{"role":"user","content":[{"type":"text","text":"[Image: source: C:\\tmp\\a.jpg]"}]}}"#;

    fn open_with(lines: &[&str]) -> ChatState {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        append(&path, &lines.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        let s = ChatState::open(path, Format::Claude).unwrap();
        std::mem::forget(dir);
        s
    }

    #[test]
    fn image_companion_is_dropped_when_the_user_message_has_the_inline_image() {
        let s = open_with(&[USER_WITH_IMAGE, COMPANION]);
        assert_eq!(image_count(&s.latest(10).0), 1);
        let s = open_with(&[USER_WITH_IMAGE, COMPANION_NO_PARENT]);
        assert_eq!(image_count(&s.latest(10).0), 1, "falls back to the preceding user message");
    }

    #[test]
    fn image_companion_is_kept_without_an_inline_image() {
        let s = open_with(&[USER_NO_IMAGE, COMPANION]);
        assert_eq!(image_count(&s.latest(10).0), 1);
        let s = open_with(&[USER_NO_IMAGE, COMPANION_NO_PARENT]);
        assert_eq!(image_count(&s.latest(10).0), 1);
    }

    #[test]
    fn image_companion_dedupe_works_across_polls() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        append(&path, &[USER_WITH_IMAGE.to_string()]);
        let mut s = ChatState::open(path.clone(), Format::Claude).unwrap();
        append(&path, &[COMPANION.to_string()]);
        assert_eq!(s.poll().unwrap(), Change::None);
        assert_eq!(image_count(&s.latest(10).0), 1);
    }

    #[test]
    fn plain_path_text_next_to_an_inline_image_does_not_duplicate() {
        let l = r#"{"type":"user","uuid":"u9","message":{"content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"AA"}},{"type":"text","text":"[Image: source: /tmp/a.png]"}]}}"#;
        assert_eq!(image_count(&open_with(&[l]).latest(10).0), 1);
    }
}
