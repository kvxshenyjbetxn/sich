//! Identity of a Codex subagent from its rollout file (read-only, first line only).
//!
//! A child hook carries `transcript_path`: the subagent's own rollout `.jsonl`. Its first line is a
//! `session_meta` record that names the agent (`agent_path` like `root/explorer`, or a nickname).
//! We read at most [`MAX_FIRST_LINE_BYTES`] from offset 0, never beyond the first line, and never
//! log the contents.

use std::fs::File;
use std::io::Read as _;
use std::path::Path;

/// The first line is read from at most this many bytes.
pub const MAX_FIRST_LINE_BYTES: u64 = 256 * 1024;

/// Result of reading a subagent's identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityRead {
    /// The label was found.
    Found(String),
    /// The file is missing or empty, or its first line is not complete yet: try again later.
    Pending,
    /// A complete first line without a usable identity (or not JSON): never retry.
    Miss,
    /// Another I/O error: retry later.
    Unreadable,
}

/// A rollout read the tracker wants done for a child hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityPlan {
    pub agent_id: String,
    pub path: String,
    /// The file length at the last `Pending` read: skip the read while it is unchanged.
    pub pending_len: Option<u64>,
}

/// The outcome of an executed [`IdentityPlan`], fed back to the tracker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityResult {
    pub agent_id: String,
    pub path: String,
    /// File length at read time (0 when missing).
    pub len: u64,
    pub read: IdentityRead,
}

/// Runs the plan: `None` when a pending file did not grow since the last read.
pub fn execute(plan: &IdentityPlan) -> Option<IdentityResult> {
    let len = std::fs::metadata(&plan.path).map(|m| m.len()).unwrap_or(0);
    if plan.pending_len == Some(len) {
        return None;
    }
    let read = read_subagent_label(Path::new(&plan.path), &plan.agent_id);
    Some(IdentityResult { agent_id: plan.agent_id.clone(), path: plan.path.clone(), len, read })
}

/// `root/explorer` → `explorer`, `root/a/b` → `a/b`, `root` → none, `worker` → `worker`.
pub fn path_label(path: &str) -> Option<String> {
    let mut parts: Vec<&str> = path.split('/').map(str::trim).filter(|s| !s.is_empty()).collect();
    if parts.first() == Some(&"root") {
        parts.remove(0);
    }
    if parts.is_empty() { None } else { Some(parts.join("/")) }
}

fn text<'a>(v: Option<&'a serde_json::Value>, key: &str) -> Option<&'a str> {
    v?.get(key)?.as_str()
}

fn nonempty(s: Option<&str>) -> Option<String> {
    s.map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

/// Label from a `session_meta` line. Valid only when `type == "session_meta"` and
/// `payload.id == agent_id`. Priority: path label (`thread_spawn.agent_path`, else
/// `payload.agent_path`), then `thread_spawn.agent_nickname`, then `payload.agent_nickname`.
pub fn parse_session_meta_label(line: &str, agent_id: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    if v.get("type")?.as_str()? != "session_meta" {
        return None;
    }
    let payload = v.get("payload")?;
    if payload.get("id")?.as_str()? != agent_id {
        return None;
    }
    let spawn = payload.get("source").and_then(|s| s.get("subagent")).and_then(|s| s.get("thread_spawn"));
    let path = nonempty(text(spawn, "agent_path")).or_else(|| nonempty(text(Some(payload), "agent_path")));
    path.as_deref()
        .and_then(path_label)
        .or_else(|| nonempty(text(spawn, "agent_nickname")))
        .or_else(|| nonempty(text(Some(payload), "agent_nickname")))
}

/// Reads the first line of `path` (at most 256 KiB) and extracts the label for `agent_id`.
pub fn read_subagent_label(path: &Path, agent_id: &str) -> IdentityRead {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return IdentityRead::Pending,
        Err(_) => return IdentityRead::Unreadable,
    };
    let mut buf = Vec::new();
    if file.take(MAX_FIRST_LINE_BYTES).read_to_end(&mut buf).is_err() {
        return IdentityRead::Unreadable;
    }
    let Some(end) = buf.iter().position(|&b| b == b'\n') else {
        // No complete line yet; a full 256 KiB without a newline will never become a valid line.
        return if buf.len() as u64 >= MAX_FIRST_LINE_BYTES { IdentityRead::Miss } else { IdentityRead::Pending };
    };
    let Ok(line) = std::str::from_utf8(&buf[..end]) else { return IdentityRead::Miss };
    match parse_session_meta_label(line.trim_end_matches('\r'), agent_id) {
        Some(label) => IdentityRead::Found(label),
        None => IdentityRead::Miss,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn meta(payload: &str) -> String {
        format!(r#"{{"type":"session_meta","payload":{payload}}}"#)
    }

    #[test]
    fn path_labels() {
        assert_eq!(path_label("root/explorer").as_deref(), Some("explorer"));
        assert_eq!(path_label("root/a/b").as_deref(), Some("a/b"));
        assert_eq!(path_label("root"), None);
        assert_eq!(path_label(""), None);
        assert_eq!(path_label("worker").as_deref(), Some("worker"));
        assert_eq!(path_label("/root//x/").as_deref(), Some("x"));
        assert_eq!(path_label("a/root").as_deref(), Some("a/root"));
    }

    #[test]
    fn label_priority() {
        let l = |p: &str| parse_session_meta_label(&meta(p), "a1");
        assert_eq!(l(r#"{"id":"a1","agent_path":"root/top","source":{"subagent":{"thread_spawn":{"agent_path":"root/explorer","agent_nickname":"Nick"}}}}"#).as_deref(), Some("explorer"));
        assert_eq!(l(r#"{"id":"a1","agent_path":"root/top","source":{"subagent":{"thread_spawn":{"agent_nickname":"Nick"}}}}"#).as_deref(), Some("top"));
        assert_eq!(l(r#"{"id":"a1","agent_path":"root","source":{"subagent":{"thread_spawn":{"agent_nickname":"Nick"}}}}"#).as_deref(), Some("Nick"));
        assert_eq!(l(r#"{"id":"a1","agent_nickname":"PNick","source":{"subagent":{"thread_spawn":{}}}}"#).as_deref(), Some("PNick"));
        assert_eq!(l(r#"{"id":"a1","agent_nickname":"PNick","source":{"subagent":{"thread_spawn":{"agent_nickname":"SNick"}}}}"#).as_deref(), Some("SNick"));
        assert_eq!(l(r#"{"id":"a1","agent_path":"worker"}"#).as_deref(), Some("worker"));
        assert_eq!(l(r#"{"id":"a1","agent_nickname":"  "}"#), None);
        assert_eq!(l(r#"{"id":"a1"}"#), None);
    }

    #[test]
    fn mismatches_are_misses() {
        assert_eq!(parse_session_meta_label(&meta(r#"{"id":"other","agent_path":"root/x"}"#), "a1"), None);
        assert_eq!(parse_session_meta_label(&meta(r#"{"agent_path":"root/x"}"#), "a1"), None);
        assert_eq!(parse_session_meta_label(r#"{"type":"event_msg","payload":{"id":"a1","agent_path":"root/x"}}"#, "a1"), None);
        assert_eq!(parse_session_meta_label("not json", "a1"), None);
        assert_eq!(parse_session_meta_label(r#"{"type":"session_meta"}"#, "a1"), None);
        assert_eq!(parse_session_meta_label("[1]", "a1"), None);
    }

    fn temp_file(dir: &Path, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let p = dir.join(name);
        std::fs::File::create(&p).unwrap().write_all(bytes).unwrap();
        p
    }

    fn tmp() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("sich-rollout-{}-{}", std::process::id(), next()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn next() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        N.fetch_add(1, Ordering::Relaxed)
    }

    #[test]
    fn reads_found_and_ignores_later_lines() {
        let d = tmp();
        let mut content = meta(r#"{"id":"a1","agent_path":"root/explorer"}"#);
        content.push_str("\r\n{\"secret\":1}\n");
        let p = temp_file(&d, "r.jsonl", content.as_bytes());
        assert_eq!(read_subagent_label(&p, "a1"), IdentityRead::Found("explorer".into()));
        assert_eq!(read_subagent_label(&p, "zzz"), IdentityRead::Miss);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn pending_when_missing_empty_or_partial() {
        let d = tmp();
        assert_eq!(read_subagent_label(&d.join("nope.jsonl"), "a1"), IdentityRead::Pending);
        let p = temp_file(&d, "e.jsonl", b"");
        assert_eq!(read_subagent_label(&p, "a1"), IdentityRead::Pending);
        let p = temp_file(&d, "p.jsonl", meta(r#"{"id":"a1","agent_path":"root/x"}"#).as_bytes());
        assert_eq!(read_subagent_label(&p, "a1"), IdentityRead::Pending, "no newline yet");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn miss_for_complete_line_without_identity_or_garbage() {
        let d = tmp();
        let p = temp_file(&d, "m.jsonl", b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"a1\"}}\n");
        assert_eq!(read_subagent_label(&p, "a1"), IdentityRead::Miss);
        let p = temp_file(&d, "g.jsonl", b"garbage\n");
        assert_eq!(read_subagent_label(&p, "a1"), IdentityRead::Miss);
        let p = temp_file(&d, "b.jsonl", b"\xff\xfe\n");
        assert_eq!(read_subagent_label(&p, "a1"), IdentityRead::Miss);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn huge_first_line_is_a_miss_and_read_is_bounded() {
        let d = tmp();
        let big = vec![b'x'; (MAX_FIRST_LINE_BYTES as usize) + 10];
        let p = temp_file(&d, "big.jsonl", &big);
        assert_eq!(read_subagent_label(&p, "a1"), IdentityRead::Miss);
        // A short file without a newline is still pending.
        let p = temp_file(&d, "short.jsonl", &big[..1000]);
        assert_eq!(read_subagent_label(&p, "a1"), IdentityRead::Pending);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn execute_skips_unchanged_pending_file() {
        let d = tmp();
        let p = temp_file(&d, "x.jsonl", b"{\"type\"");
        let path = p.display().to_string();
        let plan = |pending_len| IdentityPlan { agent_id: "a1".into(), path: path.clone(), pending_len };
        let r = execute(&plan(None)).unwrap();
        assert_eq!((r.read.clone(), r.len), (IdentityRead::Pending, 7));
        assert_eq!(execute(&plan(Some(7))), None);
        assert!(execute(&plan(Some(3))).is_some());
        let missing = IdentityPlan {
            agent_id: "a1".into(),
            path: d.join("no.jsonl").display().to_string(),
            pending_len: Some(0),
        };
        assert_eq!(execute(&missing), None, "still missing: nothing to do");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn directory_is_unreadable() {
        let d = tmp();
        assert_eq!(read_subagent_label(&d, "a1"), IdentityRead::Unreadable);
        let _ = std::fs::remove_dir_all(&d);
    }
}
