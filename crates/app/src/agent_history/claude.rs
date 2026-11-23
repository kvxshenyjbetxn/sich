//! Claude Code session files: `<config>/projects/<project>/<session id>.jsonl`, one JSON record
//! per line. Only the fields the history list needs are read; unknown or malformed records are
//! skipped (the format is undocumented and may change).

use serde::Deserialize;
use serde_json::Value;

use super::text::{content_text, is_injected, lenient_bool, lenient_string, one_line, parse_ms};
use super::{Parsed, Role, TEXT_MAX, TITLE_MAX};

/// The record header: everything except the (possibly huge) `message`.
#[derive(Deserialize, Default)]
struct Head {
    #[serde(rename = "type", default, deserialize_with = "lenient_string")]
    kind: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    timestamp: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    cwd: Option<String>,
    #[serde(rename = "gitBranch", default, deserialize_with = "lenient_string")]
    git_branch: Option<String>,
    #[serde(rename = "customTitle", default, deserialize_with = "lenient_string")]
    custom_title: Option<String>,
    #[serde(rename = "aiTitle", default, deserialize_with = "lenient_string")]
    ai_title: Option<String>,
    #[serde(rename = "lastPrompt", default, deserialize_with = "lenient_string")]
    last_prompt: Option<String>,
    #[serde(rename = "isMeta", default, deserialize_with = "lenient_bool")]
    is_meta: Option<bool>,
    #[serde(rename = "isSidechain", default, deserialize_with = "lenient_bool")]
    is_sidechain: Option<bool>,
}

#[derive(Deserialize, Default)]
struct WithMessage {
    #[serde(default)]
    message: Option<Message>,
}

#[derive(Deserialize, Default)]
struct Message {
    #[serde(default, deserialize_with = "lenient_string")]
    model: Option<String>,
    #[serde(default)]
    content: Option<Value>,
}

fn non_empty(text: Option<String>) -> Option<String> {
    text.map(|t| t.trim().to_owned()).filter(|t| !t.is_empty())
}

/// The text of a real user prompt or an assistant reply of record `line`; `None` for everything
/// else (meta, injected, sidechain, tool-result-only records).
fn message_text(head: &Head, line: &str) -> Option<(Option<String>, String)> {
    if head.is_sidechain == Some(true) {
        return None;
    }
    let is_user = head.kind.as_deref() == Some("user");
    if is_user && head.is_meta == Some(true) {
        return None;
    }
    let message = serde_json::from_str::<WithMessage>(line).ok()?.message?;
    let text = content_text(message.content.as_ref()?)?;
    if is_user && is_injected(&text) {
        return None;
    }
    Some((message.model, text))
}

pub(super) fn parse<'a>(lines: impl IntoIterator<Item = &'a str>) -> Parsed {
    let mut p = Parsed::default();
    for line in lines {
        let Ok(head) = serde_json::from_str::<Head>(line) else { continue };
        if p.cwd.is_none() {
            p.cwd = non_empty(head.cwd.clone());
        }
        if let Some(branch) = non_empty(head.git_branch.clone()) {
            p.branch = Some(branch);
        }
        if p.created_ms.is_none() {
            p.created_ms = head.timestamp.as_deref().and_then(parse_ms);
        }
        match head.kind.as_deref() {
            Some("custom-title") => {
                p.custom_title = head.custom_title.as_deref().map(|t| one_line(t, TITLE_MAX)).filter(|t| !t.is_empty());
            }
            Some("ai-title") => {
                if let Some(title) = head.ai_title.as_deref().map(|t| one_line(t, TITLE_MAX)).filter(|t| !t.is_empty()) {
                    p.ai_title = Some(title);
                }
            }
            Some("last-prompt") => {
                if let Some(prompt) = head.last_prompt.as_deref().map(|t| one_line(t, TEXT_MAX)).filter(|t| !t.is_empty()) {
                    p.last_prompt = Some(prompt);
                }
            }
            Some("user") => {
                let Some((_, text)) = message_text(&head, line) else { continue };
                let text = one_line(&text, TEXT_MAX);
                if text.is_empty() {
                    continue;
                }
                p.first_prompt.get_or_insert_with(|| text.clone());
                p.last_prompt = Some(text);
            }
            Some("assistant") => {
                let Some((model, text)) = message_text(&head, line) else {
                    // A reply without text (tool use only) still names the model.
                    if head.is_sidechain != Some(true)
                        && let Some(model) = serde_json::from_str::<WithMessage>(line)
                            .ok()
                            .and_then(|w| w.message)
                            .and_then(|m| non_empty(m.model))
                            .filter(|m| m != "<synthetic>")
                    {
                        p.model = Some(model);
                    }
                    continue;
                };
                if let Some(model) = non_empty(model).filter(|m| m != "<synthetic>") {
                    p.model = Some(model);
                }
                let text = one_line(&text, TEXT_MAX);
                if !text.is_empty() {
                    p.last_assistant = Some(text);
                }
            }
            _ => {}
        }
    }
    p
}

/// A conversation turn of record `line`, for the expanded row.
pub(super) fn turn(line: &str) -> Option<(Role, String)> {
    if !line.contains("\"user\"") && !line.contains("\"assistant\"") {
        return None;
    }
    let head = serde_json::from_str::<Head>(line).ok()?;
    let role = match head.kind.as_deref()? {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        _ => return None,
    };
    let (_, text) = message_text(&head, line)?;
    Some((role, text))
}
