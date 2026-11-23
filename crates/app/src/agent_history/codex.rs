//! Codex session files: `<codex home>/sessions/YYYY/MM/DD/rollout-<time>-<session id>.jsonl`, one
//! `{type, payload}` record per line. Only the fields the history list needs are read; unknown or
//! malformed records are skipped (the format is undocumented and may change).
//!
//! The user's prompts and the agent's replies come from `event_msg` records; the `response_item`
//! messages (which also hold injected context) are only a fallback for files without them.

use serde::Deserialize;
use serde_json::Value;

use super::text::{content_text, is_injected, lenient_string, one_line, parse_ms};
use super::{Parsed, Role, TEXT_MAX};

#[derive(Deserialize, Default)]
struct Line {
    #[serde(rename = "type", default, deserialize_with = "lenient_string")]
    kind: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    timestamp: Option<String>,
    #[serde(default)]
    payload: Option<Payload>,
}

#[derive(Deserialize, Default)]
struct Payload {
    #[serde(rename = "type", default, deserialize_with = "lenient_string")]
    kind: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    id: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    cwd: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    model: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    message: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    role: Option<String>,
    #[serde(default)]
    git: Option<Value>,
    #[serde(default)]
    source: Option<Value>,
    #[serde(default)]
    content: Option<Value>,
}

fn non_empty(text: Option<String>) -> Option<String> {
    text.map(|t| t.trim().to_owned()).filter(|t| !t.is_empty())
}

/// First and last text of one role from one record source.
#[derive(Default)]
struct Seen {
    first: Option<String>,
    last: Option<String>,
}

impl Seen {
    fn add(&mut self, text: String) {
        self.first.get_or_insert_with(|| text.clone());
        self.last = Some(text);
    }
}

#[derive(Default)]
struct Source {
    user: Seen,
    assistant: Seen,
}

/// The session id in a rollout file name: `rollout-YYYY-MM-DDTHH-MM-SS-<id>.jsonl`.
pub(super) fn id_from_file_name(name: &str) -> Option<String> {
    let stem = name.strip_suffix(".jsonl")?.strip_prefix("rollout-")?;
    let id = stem.get(20..)?;
    (stem.as_bytes().get(19) == Some(&b'-') && !id.is_empty()).then(|| id.to_owned())
}

/// The text of a real user prompt or an assistant reply of `payload`, with its role and whether it
/// came from an `event_msg` (as opposed to a `response_item`).
fn message(kind: &str, payload: &Payload) -> Option<(Role, String, bool)> {
    let (role, text, event) = match (kind, payload.kind.as_deref()?) {
        ("event_msg", "user_message") => (Role::User, payload.message.clone()?, true),
        ("event_msg", "agent_message") => (Role::Assistant, payload.message.clone()?, true),
        ("response_item", "message") => {
            let role = match payload.role.as_deref()? {
                "user" => Role::User,
                "assistant" => Role::Assistant,
                _ => return None,
            };
            (role, content_text(payload.content.as_ref()?)?, false)
        }
        _ => return None,
    };
    if text.trim().is_empty() || (role == Role::User && is_injected(&text)) {
        return None;
    }
    Some((role, text, event))
}

pub(super) fn parse<'a>(lines: impl IntoIterator<Item = &'a str>) -> Parsed {
    let mut p = Parsed::default();
    let (mut events, mut items) = (Source::default(), Source::default());
    let mut saw_meta = false;
    for line in lines {
        let Ok(record) = serde_json::from_str::<Line>(line) else { continue };
        let Some(kind) = record.kind.as_deref() else { continue };
        if p.created_ms.is_none() {
            p.created_ms = record.timestamp.as_deref().and_then(parse_ms);
        }
        let Some(payload) = record.payload else { continue };
        match kind {
            // Forks may copy the parent's metadata later; the first one defines the thread.
            "session_meta" if !saw_meta => {
                saw_meta = true;
                p.id = non_empty(payload.id);
                p.cwd = non_empty(payload.cwd);
                p.branch = payload
                    .git
                    .as_ref()
                    .and_then(|git| git.get("branch"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|b| !b.is_empty())
                    .map(str::to_owned);
                p.subagent = payload.source.as_ref().is_some_and(|s| s.get("subagent").is_some());
            }
            "turn_context" => {
                if p.cwd.is_none() {
                    p.cwd = non_empty(payload.cwd);
                }
                if let Some(model) = non_empty(payload.model) {
                    p.model = Some(model);
                }
            }
            "event_msg" | "response_item" => {
                let Some((role, text, event)) = message(kind, &payload) else { continue };
                let text = one_line(&text, TEXT_MAX);
                if text.is_empty() {
                    continue;
                }
                let source = if event { &mut events } else { &mut items };
                match role {
                    Role::User => source.user.add(text),
                    Role::Assistant => source.assistant.add(text),
                }
            }
            _ => {}
        }
    }
    let user = if events.user.first.is_some() { events.user } else { items.user };
    let assistant = if events.assistant.last.is_some() { events.assistant } else { items.assistant };
    p.first_prompt = user.first;
    p.last_prompt = user.last;
    p.last_assistant = assistant.last;
    p
}

/// A conversation turn of record `line` and whether it came from an `event_msg`.
pub(super) fn turn(line: &str) -> Option<(Role, String, bool)> {
    if !line.contains("\"event_msg\"") && !line.contains("\"response_item\"") {
        return None;
    }
    let record = serde_json::from_str::<Line>(line).ok()?;
    message(record.kind.as_deref()?, &record.payload?)
}
