//! A terminal's in-memory activity. Keys bind records to a particular child run and root session.

use std::collections::VecDeque;

use serde_json::{Map, Value};
use sich_proto::{
    AgentKind, AgentStatus, SubagentActivity, SubagentActivityEntry, SubagentActivityKey,
    SubagentActivityKind as Kind, SubagentStatus,
};

const MAX_RUNS: usize = 48;
const MAX_ENTRIES: usize = 128;
const MAX_TITLE_BYTES: usize = 120;
const MAX_TEXT_BYTES: usize = 2048;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Note {
    pub kind: Kind,
    pub title: Option<String>,
    pub text: Option<String>,
}

pub(super) fn terminal(kind: Kind) -> bool {
    matches!(kind, Kind::Completed | Kind::Failed | Kind::Aborted | Kind::Ended)
}

impl Note {
    pub fn new(kind: Kind, title: Option<&str>, text: Option<&str>) -> Self {
        Self {
            kind,
            title: title.and_then(|s| preview([s], MAX_TITLE_BYTES)),
            text: text.and_then(|s| preview([s], MAX_TEXT_BYTES)),
        }
    }
}

/// Render only the textual fields of documented hook payloads. Do not retain tool inputs,
/// arbitrary JSON metadata, images, or other transcript content.
pub(super) fn hook_note(
    event: &str,
    agent_id: Option<&str>,
    tool: Option<&str>,
    fields: &Map<String, Value>,
) -> Option<Note> {
    let id = agent_id?;
    if id.starts_with(super::HOOK_AGENT_PREFIX) {
        return None;
    }
    let string = |name| fields.get(name).and_then(Value::as_str);
    let asks_user = matches!(tool, Some("AskUserQuestion" | "request_user_input"));
    let (kind, title, text) = match event {
        "PreToolUse" if asks_user => (Kind::Waiting, tool, None),
        "PreToolUse" => (Kind::ToolStarted, tool, None),
        "PostToolUse" => {
            let response = fields.get("tool_response");
            let failed = response.and_then(|r| r.get("isError")).and_then(Value::as_bool) == Some(true);
            return Some(Note {
                kind: if failed { Kind::ToolFailed } else { Kind::ToolFinished },
                title: tool.and_then(|s| preview([s], MAX_TITLE_BYTES)),
                // These results can hold direct user input, rather than agent output.
                text: if asks_user { None } else { response.and_then(result_preview) },
            });
        }
        "PostToolUseFailure" => (Kind::ToolFailed, tool, string("error")),
        "PermissionRequest" => (Kind::Waiting, tool, None),
        "Notification" if string("notification_type") != Some("idle_prompt") => {
            (Kind::Waiting, None, string("message"))
        }
        "Elicitation" => (Kind::Waiting, None, string("message")),
        "SubagentStop" | "Stop" => (Kind::Completed, None, string("last_assistant_message")),
        "StopFailure" => (Kind::Failed, None, string("last_assistant_message").or_else(|| string("error"))),
        _ => return None,
    };
    Some(Note::new(kind, title, text))
}

fn result_preview(response: &Value) -> Option<String> {
    if let Some(s) = response.as_str() {
        return preview([s], MAX_TEXT_BYTES);
    }
    let parts = ["stdout", "stderr", "output"].into_iter().filter_map(|k| response.get(k)?.as_str());
    if let Some(text) = preview(parts, MAX_TEXT_BYTES) {
        return Some(text);
    }
    let blocks = response.as_array().or_else(|| response.get("content")?.as_array())?;
    preview(blocks.iter().filter_map(|b| {
        (b.get("type")?.as_str()? == "text").then(|| b.get("text")?.as_str()).flatten()
    }), MAX_TEXT_BYTES)
}

fn preview<'a>(parts: impl IntoIterator<Item = &'a str>, cap: usize) -> Option<String> {
    let mut out = String::new();
    for part in parts {
        let part = part.trim();
        if part.is_empty() { continue; }
        if !out.is_empty() && out.len() < cap { out.push('\n'); }
        for c in part.chars().filter(|c| !c.is_control() || matches!(c, '\n' | '\t')) {
            if out.len() + c.len_utf8() > cap { return Some(out); }
            out.push(c);
        }
    }
    (!out.is_empty()).then_some(out)
}

// An omp roster snapshot may overtake a completion queued in a later hook batch.
const SETTLEMENT_GRACE_MS: u64 = 5_000;

#[derive(Debug)]
struct Run {
    key: SubagentActivityKey,
    entries: VecDeque<SubagentActivityEntry>,
    running: bool,
    listed: bool,
    missing_since: Option<u64>,
}

impl Run {
    fn push(&mut self, note: Note, now: u64) {
        if !self.running { return; }
        self.running = !terminal(note.kind);
        let seq = self.entries.back().map_or(1, |e| e.seq + 1);
        if self.entries.len() == MAX_ENTRIES { self.entries.pop_front(); }
        self.entries.push_back(SubagentActivityEntry { seq, unix_ms: now, kind: note.kind, title: note.title, text: note.text });
    }

    fn belongs_to(&self, status: &AgentStatus) -> bool {
        self.key.agent == status.kind && self.key.session_id == status.session_id
    }

    fn matches(&self, status: &AgentStatus, sub: &SubagentStatus) -> bool {
        self.belongs_to(status) && self.key.id == sub.id && self.key.since_unix_ms == sub.since_unix_ms
    }
}

#[derive(Debug, Default)]
pub(super) struct Book(VecDeque<Run>);

impl Book {
    pub fn delta(&mut self, key: &SubagentActivityKey, after_seq: u64, now: u64) -> Option<SubagentActivity> {
        let run = self.0.iter_mut().find(|r| r.key == *key)?;
        if run.missing_since.is_some_and(|since| now.saturating_sub(since) >= SETTLEMENT_GRACE_MS) {
            run.push(Note::new(Kind::Ended, None, None), now);
        }
        let first_seq = run.entries.front()?.seq;
        let skip = after_seq.saturating_add(1).saturating_sub(first_seq).min(MAX_ENTRIES as u64) as usize;
        Some(SubagentActivity { running: run.running, first_seq, entries: run.entries.iter().skip(skip).cloned().collect() })
    }

    fn start(&mut self, key: SubagentActivityKey, note: Note, now: u64) {
        if self.0.len() == MAX_RUNS {
            let Some(index) = self.0.iter().position(|run| !run.running)
                .or_else(|| self.0.iter().position(|run| !run.listed)) else { return };
            self.0.remove(index);
        }
        let mut run = Run { key, entries: VecDeque::new(), running: true, listed: false, missing_since: None };
        run.push(note, now);
        self.0.push_back(run);
    }

    /// Claude may resume the same ID in the same millisecond. Give the new row a distinct run key.
    pub fn distinguish_reused_ids(&self, status: Option<&mut AgentStatus>) {
        let Some(status) = status else { return };
        for sub in &mut status.subagents {
            let mut matching = self.0.iter().filter(|run| {
                run.key.agent == status.kind && run.key.session_id == status.session_id && run.key.id == sub.id
            });
            if let Some(last) = matching.next_back()
                && !last.running && last.key.since_unix_ms >= sub.since_unix_ms
            {
                sub.since_unix_ms = last.key.since_unix_ms.saturating_add(1);
            }
        }
    }

    /// Reconcile native lifecycle with the existing roster. An omp run may be outside the first
    /// 16 listed rows; retain its explicit lifecycle without confusing it with a removed row.
    pub fn sync(&mut self, status: Option<&AgentStatus>, now: u64) {
        for run in &mut self.0 {
            if !run.running { continue; }
            match status.filter(|s| run.belongs_to(s)) {
                Some(s) if s.subagents.iter().any(|sub| run.matches(s, sub)) => {
                    run.listed = true;
                    run.missing_since = None;
                }
                Some(s) if s.kind == AgentKind::Omp => {
                    if run.listed {
                        let since = *run.missing_since.get_or_insert(now);
                        if now.saturating_sub(since) >= SETTLEMENT_GRACE_MS {
                            run.push(Note::new(Kind::Ended, None, None), now);
                        }
                    }
                }
                _ => run.push(Note::new(Kind::Ended, None, None), now),
            }
        }
        if let Some(status) = status {
            for sub in &status.subagents {
                if !self.0.iter().any(|run| run.matches(status, sub)) {
                    self.start(SubagentActivityKey {
                        agent: status.kind, session_id: status.session_id.clone(), id: sub.id.clone(), since_unix_ms: sub.since_unix_ms,
                    }, Note::new(Kind::Started, sub.description.as_deref().or(sub.agent_type.as_deref()), None), now);
                    if let Some(run) = self.0.back_mut().filter(|run| run.matches(status, sub)) { run.listed = true; }
                }
            }
        }
    }

    pub fn hook(&mut self, status: Option<&AgentStatus>, id: &str, note: Note, now: u64) {
        let Some(status) = status else { return };
        let Some(sub) = status.subagents.iter().find(|sub| sub.id == id) else { return };
        if let Some(run) = self.0.iter_mut().find(|run| run.matches(status, sub)) {
            run.push(note, now);
        }
    }

    pub fn omp(&mut self, session: &str, id: String, since_unix_ms: u64, note: Note, now: u64) {
        let run = self.0.iter_mut().find(|run| run.key.agent == AgentKind::Omp && run.key.id == id
            && run.key.session_id.as_deref() == Some(session) && run.key.since_unix_ms == since_unix_ms);
        match (run, note.kind) {
            (None, Kind::Started) => self.start(SubagentActivityKey {
                agent: AgentKind::Omp, session_id: Some(session.to_owned()), id, since_unix_ms,
            }, note, now),
            (Some(run), kind) if kind != Kind::Started => run.push(note, now),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncated_history_resumes_without_repeating_records() {
        let mut book = Book::default();
        let key = SubagentActivityKey { agent: AgentKind::Omp, session_id: Some("s".into()), id: "Probe".into(), since_unix_ms: 1 };
        let add = |book: &mut Book, kind, n| book.omp("s", "Probe".into(), 1, Note::new(kind, None, None), n);
        add(&mut book, Kind::Started, 1);
        for n in 2..=MAX_ENTRIES as u64 + 2 { add(&mut book, Kind::ToolStarted, n); }
        let delta = book.delta(&key, 0, 160).unwrap();
        assert_eq!(delta.entries.len(), MAX_ENTRIES);
        assert_eq!(delta.first_seq, 3);
        let last = delta.entries.last().unwrap().seq;
        add(&mut book, Kind::Completed, 200);
        add(&mut book, Kind::ToolStarted, 201);
        let tail = book.delta(&key, last, 201).unwrap();
        assert!(!tail.running);
        assert_eq!(tail.entries[0].kind, Kind::Completed);
        assert_eq!(tail.entries[0].seq, last + 1);
        assert_eq!(tail.entries.len(), 1);
    }

    #[test]
    fn omp_completion_can_follow_roster_removal() {
        let mut tracker = super::super::AgentTracker::default();
        tracker.on_omp_hook(r#"{"session_id":"s","state":"working","subagents":[
            {"id":"Keep","since_unix_ms":1,"state":"working"},
            {"id":"Lost","since_unix_ms":2,"state":"working"}]}"#, 10);
        tracker.on_omp_hook(r#"{"session_id":"s","state":"working"}"#, 20);
        let key = |id: &str, since_unix_ms| SubagentActivityKey {
            agent: AgentKind::Omp, session_id: Some("s".into()), id: id.into(), since_unix_ms,
        };
        let keep = key("Keep", 1);
        let lost = key("Lost", 2);
        assert!(tracker.subagent_activity(&keep, 1, 30).unwrap().running);
        tracker.on_omp_hook(r#"{"session_id":"s","state":"working","activity":[
            {"id":"Keep","since_unix_ms":1,"kind":"completed"}]}"#, 40);
        let final_entry = tracker.subagent_activity(&keep, 1, 50).unwrap();
        assert!(!final_entry.running);
        assert_eq!(final_entry.entries[0].kind, Kind::Completed);
        let expired = tracker.subagent_activity(&lost, 1, 20 + SETTLEMENT_GRACE_MS).unwrap();
        assert!(!expired.running);
        assert_eq!(expired.entries[0].kind, Kind::Ended);
    }
}
