//! Transcript line decoders: one JSONL line -> chat messages. Pure and fail-soft.
//!
//! Message ids are the line's `uuid` (Claude) or `c<offset>` (Codex); when one line yields
//! several messages (different roles) the extra ones get `#1`, `#2`... All messages of a line
//! share its `order` (the byte offset of the line).
//!
//! Images are never inlined in a message: an `Image` block carries `i<offset>.<n>`, which names
//! the n-th image of the line at `offset`. `image::fetch` re-reads that line and decodes it again,
//! so the gateway keeps no image data in memory.

use std::path::PathBuf;

use serde_json::Value;
use sich_remote_proto::{ChatBlock, ChatMessage, ChatRole};

use super::Format;

pub const TEXT_CAP: usize = 64_000;
pub const RESULT_CAP: usize = 4_000;
pub const SUMMARY_CAP: usize = 300;
const TRUNCATED: &str = "… (обрізано)";

/// Where an image of a line lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageSrc {
    /// Base64 payload inside the transcript line.
    Inline { mime: String, b64: String },
    /// A file the transcript mentions.
    Path(PathBuf),
}

#[derive(Debug, Default, PartialEq)]
pub struct Decoded {
    pub messages: Vec<ChatMessage>,
    /// In the order of the `i<offset>.<n>` refs.
    pub images: Vec<ImageSrc>,
}

pub fn decode_line(format: Format, offset: u64, line: &str) -> Decoded {
    match format {
        Format::Claude => claude(offset, line),
        Format::Codex => codex(offset, line),
    }
}

/// Truncates to `max` chars (on a char boundary) and marks the cut.
pub fn cap_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        None => s.to_string(),
        Some((cut, _)) => format!("{}{TRUNCATED}", &s[..cut]),
    }
}

/// One line, whitespace collapsed, capped.
pub fn one_line(s: &str, max: usize) -> String {
    let joined = s.split_whitespace().collect::<Vec<_>>().join(" ");
    cap_chars(&joined, max)
}

/// `2026-11-23T12:34:56.789Z` -> unix ms. Offsets other than `Z` are not supported (None).
pub fn parse_iso_ms(s: &str) -> Option<u64> {
    let s = s.trim().strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-');
    let (y, m, day): (i64, i64, i64) = (d.next()?.parse().ok()?, d.next()?.parse().ok()?, d.next()?.parse().ok()?);
    let (hms, frac) = time.split_once('.').unwrap_or((time, ""));
    let mut t = hms.split(':');
    let (h, mi, sec): (i64, i64, i64) = (t.next()?.parse().ok()?, t.next()?.parse().ok()?, t.next()?.parse().ok()?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&day) || h > 23 || mi > 59 || sec > 60 || y < 1970 {
        return None;
    }
    let ms: i64 = if frac.is_empty() {
        0
    } else {
        let digits: String = frac.chars().take(3).collect();
        let n: i64 = digits.parse().ok()?;
        n * 10_i64.pow(3 - digits.len() as u32)
    };
    // Days from civil (Howard Hinnant).
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(((days * 24 + h) * 60 + mi) * 60_000 + sec * 1000 + ms).ok()
}

// ---------------------------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------------------------

struct Builder {
    offset: u64,
    base_id: String,
    ts: Option<u64>,
    groups: Vec<(ChatRole, Vec<ChatBlock>)>,
    images: Vec<ImageSrc>,
}

impl Builder {
    fn new(offset: u64, base_id: String, ts: Option<u64>) -> Self {
        Self { offset, base_id, ts, groups: Vec::new(), images: Vec::new() }
    }

    fn push(&mut self, role: ChatRole, block: ChatBlock) {
        if let Some((last, blocks)) = self.groups.last_mut() {
            if *last == role {
                blocks.push(block);
                return;
            }
        }
        self.groups.push((role, vec![block]));
    }

    fn text(&mut self, role: ChatRole, s: &str) {
        let s = s.trim_end();
        if s.trim().is_empty() {
            return;
        }
        self.push(role, ChatBlock::Text { markdown: cap_chars(s, TEXT_CAP) });
    }

    fn image(&mut self, role: ChatRole, src: ImageSrc, alt: &str) {
        let n = self.images.len();
        self.images.push(src);
        self.push(role, ChatBlock::Image { image_ref: format!("i{}.{n}", self.offset), alt: one_line(alt, 120) });
    }

    fn finish(self) -> Decoded {
        let messages = self
            .groups
            .into_iter()
            .enumerate()
            .map(|(i, (role, blocks))| ChatMessage {
                id: if i == 0 { self.base_id.clone() } else { format!("{}#{i}", self.base_id) },
                order: self.offset,
                role,
                timestamp_ms: self.ts,
                blocks,
            })
            .collect();
        Decoded { messages, images: self.images }
    }
}

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// A one-line preview of a tool's input: the command, file path, pattern...
pub fn tool_summary(input: &Value) -> String {
    const KEYS: [&str; 12] =
        ["command", "cmd", "file_path", "path", "pattern", "url", "query", "description", "prompt", "skill", "subject", "text"];
    match input {
        Value::String(s) => one_line(s, SUMMARY_CAP),
        Value::Object(map) => {
            for key in KEYS {
                match map.get(key) {
                    Some(Value::String(s)) if !s.trim().is_empty() => return one_line(s, SUMMARY_CAP),
                    Some(Value::Array(a)) if !a.is_empty() => {
                        let parts: Vec<&str> = a.iter().filter_map(Value::as_str).collect();
                        if !parts.is_empty() {
                            return one_line(&parts.join(" "), SUMMARY_CAP);
                        }
                    }
                    _ => {}
                }
            }
            if map.is_empty() { String::new() } else { one_line(&input.to_string(), SUMMARY_CAP) }
        }
        Value::Null => String::new(),
        other => one_line(&other.to_string(), SUMMARY_CAP),
    }
}

// ---------------------------------------------------------------------------------------------
// Claude Code
// ---------------------------------------------------------------------------------------------

/// Text Claude Code injects as a "user" message that the user never typed.
fn claude_injected(s: &str) -> bool {
    let t = s.trim_start();
    t.starts_with("<system-reminder>") || t.starts_with("<local-command-caveat>")
}

/// `<command-name>/x</command-name><command-args>y</command-args>` -> `/x y`.
fn slash_command(s: &str) -> Option<String> {
    fn between<'a>(s: &'a str, open: &str, close: &str) -> Option<&'a str> {
        let start = s.find(open)? + open.len();
        let end = s[start..].find(close)? + start;
        Some(&s[start..end])
    }
    let name = between(s, "<command-name>", "</command-name>")?.trim();
    let args = between(s, "<command-args>", "</command-args>").unwrap_or("").trim();
    Some(if args.is_empty() { name.to_string() } else { format!("{name} {args}") })
}

/// `[Image: source: <path>]` -> the path.
fn image_source_path(s: &str) -> Option<&str> {
    let t = s.trim();
    let inner = t.strip_prefix("[Image: source: ")?.strip_suffix(']')?.trim();
    (!inner.is_empty()).then_some(inner)
}

fn claude_image_src(block: &Value) -> Option<ImageSrc> {
    let source = block.get("source")?;
    match str_of(source, "type")? {
        "base64" => {
            let data = str_of(source, "data")?;
            (!data.is_empty()).then(|| ImageSrc::Inline {
                mime: str_of(source, "media_type").unwrap_or("image/png").to_string(),
                b64: data.to_string(),
            })
        }
        _ => None,
    }
}

fn claude(offset: u64, line: &str) -> Decoded {
    // Cheap pre-filter: skips the huge `api-request-blob` and bookkeeping lines unparsed.
    if !line.contains("\"user\"") && !line.contains("\"assistant\"") {
        return Decoded::default();
    }
    let Ok(v) = serde_json::from_str::<Value>(line) else { return Decoded::default() };
    let kind = str_of(&v, "type").unwrap_or("");
    if (kind != "user" && kind != "assistant") || v.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        return Decoded::default();
    }
    let meta = v.get("isMeta").and_then(Value::as_bool) == Some(true);
    let base_id = str_of(&v, "uuid").filter(|s| !s.is_empty()).map_or_else(|| format!("o{offset}"), String::from);
    let ts = str_of(&v, "timestamp").and_then(parse_iso_ms);
    let mut b = Builder::new(offset, base_id, ts);
    let role = if kind == "user" { ChatRole::User } else { ChatRole::Assistant };
    let content = v.get("message").map(|m| &m["content"]).unwrap_or(&Value::Null);

    match content {
        Value::String(s) => {
            if !meta && !claude_injected(s) {
                match slash_command(s) {
                    Some(cmd) if kind == "user" => b.text(role.clone(), &cmd),
                    _ => b.text(role.clone(), s),
                }
            }
        }
        Value::Array(blocks) => {
            let has_inline = blocks.iter().any(|x| str_of(x, "type") == Some("image") && claude_image_src(x).is_some());
            for blk in blocks {
                match str_of(blk, "type").unwrap_or("") {
                    "text" => {
                        let text = str_of(blk, "text").unwrap_or("");
                        if let Some(path) = image_source_path(text) {
                            // The pasted image itself is a sibling block; a path only matters without one.
                            if !has_inline {
                                b.image(role.clone(), ImageSrc::Path(PathBuf::from(path)), path);
                            }
                        } else if !meta && !claude_injected(text) {
                            match slash_command(text) {
                                Some(cmd) if kind == "user" => b.text(role.clone(), &cmd),
                                _ => b.text(role.clone(), text),
                            }
                        }
                    }
                    "image" => {
                        if let Some(src) = claude_image_src(blk) {
                            b.image(role.clone(), src, "image");
                        }
                    }
                    _ if meta => {}
                    "thinking" => b.text(ChatRole::Reasoning, str_of(blk, "thinking").unwrap_or("")),
                    "tool_use" => {
                        let call_id = str_of(blk, "id").unwrap_or("").to_string();
                        let name = str_of(blk, "name").unwrap_or("tool").to_string();
                        let summary = tool_summary(blk.get("input").unwrap_or(&Value::Null));
                        b.push(ChatRole::Tool, ChatBlock::ToolCall { call_id, name, summary });
                    }
                    "tool_result" => claude_tool_result(&mut b, blk),
                    _ => {}
                }
            }
        }
        _ => {}
    }
    b.finish()
}

fn claude_tool_result(b: &mut Builder, blk: &Value) {
    let call_id = str_of(blk, "tool_use_id").unwrap_or("").to_string();
    let is_error = blk.get("is_error").and_then(Value::as_bool) == Some(true);
    let mut output = String::new();
    let mut images = Vec::new();
    match blk.get("content") {
        Some(Value::String(s)) => output = s.clone(),
        Some(Value::Array(items)) => {
            for item in items {
                match str_of(item, "type") {
                    Some("text") => {
                        if !output.is_empty() {
                            output.push('\n');
                        }
                        output.push_str(str_of(item, "text").unwrap_or(""));
                    }
                    Some("image") => images.extend(claude_image_src(item)),
                    _ => {}
                }
            }
        }
        _ => {}
    }
    b.push(ChatRole::Tool, ChatBlock::ToolResult { call_id, output: cap_chars(&output, RESULT_CAP), is_error });
    for src in images {
        b.image(ChatRole::Tool, src, "image");
    }
}

// ---------------------------------------------------------------------------------------------
// Codex
// ---------------------------------------------------------------------------------------------

/// Context Codex adds as "user" messages.
fn codex_injected(s: &str) -> bool {
    let t = s.trim_start();
    ["<environment_context>", "<user_instructions>", "<permissions", "<turn_aborted>", "<user_action>", "# AGENTS.md instructions", "<INSTRUCTIONS>"]
        .iter()
        .any(|p| t.starts_with(p))
}

fn codex_image_src(url: &str) -> Option<ImageSrc> {
    if let Some(rest) = url.strip_prefix("data:") {
        let (head, data) = rest.split_once(',')?;
        let mime = head.split(';').next().filter(|m| !m.is_empty()).unwrap_or("image/png");
        return head.contains("base64").then(|| ImageSrc::Inline { mime: mime.to_string(), b64: data.to_string() });
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        return None;
    }
    let path = url.strip_prefix("file:///").map(|p| if cfg!(windows) { p.to_string() } else { format!("/{p}") }).unwrap_or_else(|| url.to_string());
    (!path.is_empty()).then(|| ImageSrc::Path(PathBuf::from(path)))
}

/// Tool output in any of the shapes Codex writes: plain text, `{"output":..,"metadata":..}` as a
/// string, a list of `input_text` items, or `{"content":..,"success":..}`.
fn codex_output(v: &Value) -> (String, bool) {
    match v {
        Value::String(s) => {
            if let Ok(Value::Object(o)) = serde_json::from_str::<Value>(s) {
                if let Some(out) = o.get("output").and_then(Value::as_str) {
                    let failed = o.get("metadata").and_then(|m| m.get("exit_code")).and_then(Value::as_i64).is_some_and(|c| c != 0);
                    return (out.to_string(), failed);
                }
            }
            (s.clone(), false)
        }
        Value::Array(items) => {
            let text: Vec<&str> = items.iter().filter_map(|i| str_of(i, "text")).collect();
            (text.join("\n"), false)
        }
        Value::Object(o) => {
            let text = o.get("content").and_then(Value::as_str).unwrap_or("").to_string();
            (text, o.get("success").and_then(Value::as_bool) == Some(false))
        }
        _ => (String::new(), false),
    }
}

fn codex(offset: u64, line: &str) -> Decoded {
    if !line.contains("response_item") {
        return Decoded::default();
    }
    let Ok(v) = serde_json::from_str::<Value>(line) else { return Decoded::default() };
    // Codex writes both `response_item` and `event_msg` records for the same conversation; the
    // response items are complete (tools included), so event messages are not used.
    if str_of(&v, "type") != Some("response_item") {
        return Decoded::default();
    }
    let p = &v["payload"];
    let ts = str_of(&v, "timestamp").and_then(parse_iso_ms);
    let mut b = Builder::new(offset, format!("c{offset}"), ts);
    match str_of(p, "type").unwrap_or("") {
        "message" => {
            let role = match str_of(p, "role") {
                Some("user") => ChatRole::User,
                Some("assistant") => ChatRole::Assistant,
                _ => return Decoded::default(),
            };
            for item in p.get("content").and_then(Value::as_array).into_iter().flatten() {
                match str_of(item, "type").unwrap_or("") {
                    "input_text" | "output_text" | "text" => {
                        let text = str_of(item, "text").unwrap_or("");
                        if role != ChatRole::User || !codex_injected(text) {
                            b.text(role.clone(), text);
                        }
                    }
                    "input_image" => {
                        if let Some(src) = str_of(item, "image_url").and_then(codex_image_src) {
                            b.image(role.clone(), src, "image");
                        }
                    }
                    _ => {}
                }
            }
        }
        "reasoning" => {
            let mut parts: Vec<&str> = Vec::new();
            for key in ["summary", "content"] {
                for item in p.get(key).and_then(Value::as_array).into_iter().flatten() {
                    if let Some(t) = str_of(item, "text").filter(|t| !t.trim().is_empty()) {
                        parts.push(t);
                    }
                }
            }
            b.text(ChatRole::Reasoning, &parts.join("\n\n"));
        }
        "function_call" => {
            let args = str_of(p, "arguments").and_then(|a| serde_json::from_str::<Value>(a).ok());
            let summary = match &args {
                Some(a) => tool_summary(a),
                None => one_line(str_of(p, "arguments").unwrap_or(""), SUMMARY_CAP),
            };
            b.push(
                ChatRole::Tool,
                ChatBlock::ToolCall {
                    call_id: str_of(p, "call_id").unwrap_or("").to_string(),
                    name: str_of(p, "name").unwrap_or("tool").to_string(),
                    summary,
                },
            );
        }
        "custom_tool_call" => {
            let input = p.get("input").cloned().unwrap_or(Value::Null);
            b.push(
                ChatRole::Tool,
                ChatBlock::ToolCall {
                    call_id: str_of(p, "call_id").unwrap_or("").to_string(),
                    name: str_of(p, "name").unwrap_or("tool").to_string(),
                    summary: tool_summary(&input),
                },
            );
        }
        "local_shell_call" => {
            let cmd = p.get("action").and_then(|a| a.get("command")).cloned().unwrap_or(Value::Null);
            let summary = match &cmd {
                Value::Array(a) => one_line(&a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" "), SUMMARY_CAP),
                other => tool_summary(other),
            };
            let call_id = str_of(p, "call_id").or_else(|| str_of(p, "id")).unwrap_or("").to_string();
            b.push(ChatRole::Tool, ChatBlock::ToolCall { call_id, name: "shell".into(), summary });
        }
        "web_search_call" => {
            let query = p.get("action").and_then(|a| str_of(a, "query")).unwrap_or("");
            let call_id = str_of(p, "id").unwrap_or("").to_string();
            b.push(ChatRole::Tool, ChatBlock::ToolCall { call_id, name: "web_search".into(), summary: one_line(query, SUMMARY_CAP) });
        }
        "function_call_output" | "custom_tool_call_output" | "local_shell_call_output" => {
            let (output, is_error) = codex_output(p.get("output").unwrap_or(&Value::Null));
            b.push(
                ChatRole::Tool,
                ChatBlock::ToolResult {
                    call_id: str_of(p, "call_id").unwrap_or("").to_string(),
                    output: cap_chars(&output, RESULT_CAP),
                    is_error,
                },
            );
        }
        _ => {}
    }
    b.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(m: &ChatMessage) -> Vec<&str> {
        m.blocks.iter().filter_map(|b| if let ChatBlock::Text { markdown } = b { Some(markdown.as_str()) } else { None }).collect()
    }

    fn one(d: Decoded) -> ChatMessage {
        assert_eq!(d.messages.len(), 1, "{:?}", d.messages);
        d.messages.into_iter().next().unwrap()
    }

    #[test]
    fn iso_timestamps() {
        assert_eq!(parse_iso_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso_ms("2026-11-23T12:34:56.789Z"), Some(1_795_437_296_789));
        assert_eq!(parse_iso_ms("2026-11-23T12:34:56.5Z"), Some(1_795_437_296_500));
        assert_eq!(parse_iso_ms("2024-02-29T00:00:00Z"), Some(1_709_164_800_000));
        assert_eq!(parse_iso_ms("nonsense"), None);
        assert_eq!(parse_iso_ms("2026-13-01T00:00:00Z"), None);
        assert_eq!(parse_iso_ms("2026-11-23T12:34:56+02:00"), None);
    }

    #[test]
    fn caps_cut_on_char_boundaries_and_mark_the_cut() {
        assert_eq!(cap_chars("abc", 3), "abc");
        assert_eq!(cap_chars("абвгд", 3), "абв… (обрізано)");
        let long = "я".repeat(TEXT_CAP + 10);
        let out = cap_chars(&long, TEXT_CAP);
        assert_eq!(out.chars().count(), TEXT_CAP + TRUNCATED.chars().count());
        assert_eq!(one_line("a \n b\t\tc", 10), "a b c");
        assert_eq!(one_line(&"x ".repeat(400), SUMMARY_CAP).chars().count(), SUMMARY_CAP + TRUNCATED.chars().count());
    }

    #[test]
    fn tool_summaries_pick_the_most_telling_field() {
        assert_eq!(tool_summary(&serde_json::json!({"command": "ls -la\n/tmp", "description": "x"})), "ls -la /tmp");
        assert_eq!(tool_summary(&serde_json::json!({"file_path": "/a/b.rs", "old_string": "q"})), "/a/b.rs");
        assert_eq!(tool_summary(&serde_json::json!({"cmd": ["bash", "-lc", "echo hi"]})), "bash -lc echo hi");
        assert_eq!(tool_summary(&serde_json::json!({"zzz": 1})), r#"{"zzz":1}"#);
        assert_eq!(tool_summary(&serde_json::json!({})), "");
        assert_eq!(tool_summary(&Value::Null), "");
    }

    // ----- Claude -----

    #[test]
    fn claude_user_text_string_and_blocks() {
        let l = r#"{"type":"user","uuid":"u1","timestamp":"2026-11-23T12:34:56.789Z","message":{"role":"user","content":"hello **world**"}}"#;
        let m = one(claude(10, l));
        assert_eq!((m.id.as_str(), m.order, m.role.clone(), m.timestamp_ms), ("u1", 10, ChatRole::User, Some(1_795_437_296_789)));
        assert_eq!(texts(&m), vec!["hello **world**"]);
        let l = r#"{"type":"user","uuid":"u2","message":{"role":"user","content":[{"type":"text","text":"a"},{"type":"text","text":"b"}]}}"#;
        assert_eq!(texts(&one(claude(0, l))), vec!["a", "b"]);
    }

    #[test]
    fn claude_assistant_text_thinking_and_tool_use_are_separate_roles() {
        let text = r#"{"type":"assistant","uuid":"a1","message":{"role":"assistant","content":[{"type":"text","text":"done"}]}}"#;
        assert_eq!(one(claude(0, text)).role, ChatRole::Assistant);
        let think = r#"{"type":"assistant","uuid":"a2","message":{"content":[{"type":"thinking","thinking":"hmm","signature":"x"}]}}"#;
        let m = one(claude(0, think));
        assert_eq!((m.role.clone(), texts(&m)), (ChatRole::Reasoning, vec!["hmm"]));
        let tool = r#"{"type":"assistant","uuid":"a3","message":{"content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"cargo test","description":"run"}}]}}"#;
        let m = one(claude(0, tool));
        assert_eq!(m.role, ChatRole::Tool);
        assert_eq!(m.blocks, vec![ChatBlock::ToolCall { call_id: "toolu_1".into(), name: "Bash".into(), summary: "cargo test".into() }]);
    }

    #[test]
    fn claude_mixed_line_splits_into_ordered_messages_sharing_the_order() {
        let l = r#"{"type":"assistant","uuid":"m","message":{"content":[{"type":"thinking","thinking":"t"},{"type":"text","text":"x"},{"type":"tool_use","id":"c","name":"Read","input":{"file_path":"/f"}}]}}"#;
        let d = claude(77, l);
        let got: Vec<(&str, ChatRole, u64)> = d.messages.iter().map(|m| (m.id.as_str(), m.role.clone(), m.order)).collect();
        assert_eq!(got, vec![("m", ChatRole::Reasoning, 77), ("m#1", ChatRole::Assistant, 77), ("m#2", ChatRole::Tool, 77)]);
    }

    #[test]
    fn claude_tool_results_string_blocks_error_and_cap() {
        let l = r#"{"type":"user","uuid":"r1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"ok","is_error":false}]}}"#;
        let m = one(claude(0, l));
        assert_eq!(m.role, ChatRole::Tool);
        assert_eq!(m.blocks, vec![ChatBlock::ToolResult { call_id: "toolu_1".into(), output: "ok".into(), is_error: false }]);
        let l = r#"{"type":"user","uuid":"r2","message":{"content":[{"type":"tool_result","tool_use_id":"t2","is_error":true,"content":[{"type":"text","text":"a"},{"type":"text","text":"b"},{"type":"tool_reference","tool_name":"x"}]}]}}"#;
        assert_eq!(
            one(claude(0, l)).blocks,
            vec![ChatBlock::ToolResult { call_id: "t2".into(), output: "a\nb".into(), is_error: true }]
        );
        let big = "z".repeat(RESULT_CAP + 50);
        let l = format!(r#"{{"type":"user","uuid":"r3","message":{{"content":[{{"type":"tool_result","tool_use_id":"t3","content":"{big}"}}]}}}}"#);
        let ChatBlock::ToolResult { output, .. } = &one(claude(0, &l)).blocks[0] else { panic!() };
        assert!(output.ends_with(TRUNCATED) && output.chars().count() == RESULT_CAP + TRUNCATED.chars().count());
    }

    #[test]
    fn claude_call_and_result_pair_by_id() {
        let call = claude(0, r#"{"type":"assistant","uuid":"a","message":{"content":[{"type":"tool_use","id":"X","name":"Grep","input":{"pattern":"foo"}}]}}"#);
        let res = claude(9, r#"{"type":"user","uuid":"b","message":{"content":[{"type":"tool_result","tool_use_id":"X","content":"hit"}]}}"#);
        let id_of = |d: &Decoded| match &d.messages[0].blocks[0] {
            ChatBlock::ToolCall { call_id, .. } | ChatBlock::ToolResult { call_id, .. } => call_id.clone(),
            _ => panic!(),
        };
        assert_eq!(id_of(&call), id_of(&res));
    }

    #[test]
    fn claude_inline_image_gets_a_ref_and_keeps_its_data_out_of_the_message() {
        let l = r#"{"type":"user","uuid":"i1","message":{"content":[{"type":"image","source":{"type":"base64","media_type":"image/jpeg","data":"QUJD"}},{"type":"text","text":"what is this"}]}}"#;
        let d = claude(1234, l);
        let m = &d.messages[0];
        assert_eq!(m.blocks[0], ChatBlock::Image { image_ref: "i1234.0".into(), alt: "image".into() });
        assert_eq!(d.images, vec![ImageSrc::Inline { mime: "image/jpeg".into(), b64: "QUJD".into() }]);
        assert!(!format!("{:?}", d.messages).contains("QUJD"));
    }

    #[test]
    fn claude_image_in_tool_result_follows_the_result() {
        let l = r#"{"type":"user","uuid":"i2","message":{"content":[{"type":"tool_result","tool_use_id":"t","content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"AAAA"}}]}]}}"#;
        let d = claude(5, l);
        assert!(matches!(d.messages[0].blocks[0], ChatBlock::ToolResult { .. }));
        assert!(matches!(d.messages[0].blocks[1], ChatBlock::Image { .. }));
        assert_eq!(d.images.len(), 1);
    }

    #[test]
    fn claude_meta_lines_are_skipped_unless_they_carry_an_image() {
        let l = r#"{"type":"user","uuid":"m1","isMeta":true,"message":{"content":"Caveat: injected"}}"#;
        assert!(claude(0, l).messages.is_empty());
        let l = r#"{"type":"user","uuid":"m2","isMeta":true,"message":{"content":[{"type":"text","text":"[Image: source: /tmp/a b.png]"}]}}"#;
        let d = claude(3, l);
        assert_eq!(d.images, vec![ImageSrc::Path(PathBuf::from("/tmp/a b.png"))]);
        assert!(matches!(d.messages[0].blocks[0], ChatBlock::Image { .. }));
        // With the inline image present the path text is not duplicated.
        let l = r#"{"type":"user","uuid":"m3","isMeta":true,"message":{"content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"AA"}},{"type":"text","text":"[Image: source: /tmp/a.png]"}]}}"#;
        assert_eq!(claude(0, l).images.len(), 1);
    }

    #[test]
    fn claude_injected_sidechain_and_unknown_lines_are_skipped() {
        let sys = r#"{"type":"user","uuid":"s","message":{"content":"<system-reminder>hidden</system-reminder>"}}"#;
        assert!(claude(0, sys).messages.is_empty());
        let side = r#"{"type":"assistant","uuid":"s","isSidechain":true,"message":{"content":[{"type":"text","text":"sub"}]}}"#;
        assert!(claude(0, side).messages.is_empty());
        for l in [
            r#"{"type":"system","subtype":"x","content":"c"}"#,
            r#"{"type":"api-request-blob","message":{"role":"user","content":[{"type":"text","text":"x"}]}}"#,
            r#"{"type":"user","uuid":"q","message":{"content":[{"type":"mystery","x":1}]}}"#,
            r#"{"type":"user","uuid":"q","message":{"content":42}}"#,
            "{not json \"user\"",
            "",
            "[]",
            "null \"assistant\"",
        ] {
            assert!(claude(0, l).messages.is_empty(), "{l}");
        }
    }

    #[test]
    fn claude_slash_command_is_shown_as_typed() {
        let l = r#"{"type":"user","uuid":"c","message":{"content":"<command-name>/compact</command-name>\n<command-message>compact</command-message>\n<command-args>focus on tests</command-args>"}}"#;
        assert_eq!(texts(&one(claude(0, l))), vec!["/compact focus on tests"]);
    }

    #[test]
    fn claude_missing_uuid_falls_back_to_the_offset_id() {
        let l = r#"{"type":"user","message":{"content":"hi"}}"#;
        assert_eq!(one(claude(42, l)).id, "o42");
    }

    #[test]
    fn claude_long_text_is_capped() {
        let big = "q".repeat(TEXT_CAP + 5);
        let l = format!(r#"{{"type":"assistant","uuid":"b","message":{{"content":[{{"type":"text","text":"{big}"}}]}}}}"#);
        let m = one(claude(0, &l));
        assert!(texts(&m)[0].ends_with(TRUNCATED));
    }

    // ----- Codex -----

    fn ritem(payload: &str) -> String {
        format!(r#"{{"timestamp":"2026-11-23T12:34:56.789Z","type":"response_item","payload":{payload}}}"#)
    }

    #[test]
    fn codex_user_and_assistant_messages() {
        let m = one(codex(8, &ritem(r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"fix it"}]}"#)));
        assert_eq!((m.id.as_str(), m.role.clone(), m.order, m.timestamp_ms), ("c8", ChatRole::User, 8, Some(1_795_437_296_789)));
        assert_eq!(texts(&m), vec!["fix it"]);
        let m = one(codex(9, &ritem(r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok","annotations":[]}]}"#)));
        assert_eq!((m.role.clone(), texts(&m)), (ChatRole::Assistant, vec!["ok"]));
    }

    #[test]
    fn codex_developer_and_injected_context_are_skipped() {
        let dev = ritem(r#"{"type":"message","role":"developer","content":[{"type":"input_text","text":"rules"}]}"#);
        assert!(codex(0, &dev).messages.is_empty());
        let env = ritem(r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>\n<cwd>/x</cwd>"}]}"#);
        assert!(codex(0, &env).messages.is_empty());
        let agents = ritem(r##"{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions for /x"},{"type":"input_text","text":"real prompt"}]}"##);
        assert_eq!(texts(&one(codex(0, &agents))), vec!["real prompt"]);
    }

    #[test]
    fn codex_event_msg_duplicates_are_ignored() {
        let ev = r#"{"timestamp":"2026-11-23T12:34:56Z","type":"event_msg","payload":{"type":"user_message","message":"fix it"}}"#;
        assert!(codex(0, ev).messages.is_empty());
        let ev = r#"{"type":"event_msg","payload":{"type":"agent_message","message":"done"}} response_item"#;
        assert!(codex(0, ev).messages.is_empty());
    }

    #[test]
    fn codex_reasoning_summary() {
        let m = one(codex(0, &ritem(r#"{"type":"reasoning","summary":[{"type":"summary_text","text":"**Plan**"},{"type":"summary_text","text":"go"}],"encrypted_content":"zz"}"#)));
        assert_eq!((m.role.clone(), texts(&m)), (ChatRole::Reasoning, vec!["**Plan**\n\ngo"]));
        assert!(codex(0, &ritem(r#"{"type":"reasoning","summary":[],"encrypted_content":"zz"}"#)).messages.is_empty());
    }

    #[test]
    fn codex_function_calls_and_outputs_pair_by_call_id() {
        let call = one(codex(0, &ritem(r#"{"type":"function_call","name":"shell","arguments":"{\"command\":[\"bash\",\"-lc\",\"ls\"]}","call_id":"call_1"}"#)));
        assert_eq!(call.blocks, vec![ChatBlock::ToolCall { call_id: "call_1".into(), name: "shell".into(), summary: "bash -lc ls".into() }]);
        let out = one(codex(1, &ritem(r#"{"type":"function_call_output","call_id":"call_1","output":"{\"output\":\"files\",\"metadata\":{\"exit_code\":2}}"}"#)));
        assert_eq!(out.blocks, vec![ChatBlock::ToolResult { call_id: "call_1".into(), output: "files".into(), is_error: true }]);
        let out = one(codex(2, &ritem(r#"{"type":"function_call_output","call_id":"call_2","output":"plain"}"#)));
        assert_eq!(out.blocks, vec![ChatBlock::ToolResult { call_id: "call_2".into(), output: "plain".into(), is_error: false }]);
    }

    #[test]
    fn codex_custom_tool_and_local_shell_and_search() {
        let c = one(codex(0, &ritem(r#"{"type":"custom_tool_call","name":"apply_patch","call_id":"c1","input":"*** Begin Patch\n*** Update File: a.rs"}"#)));
        assert_eq!(c.blocks, vec![ChatBlock::ToolCall { call_id: "c1".into(), name: "apply_patch".into(), summary: "*** Begin Patch *** Update File: a.rs".into() }]);
        let o = one(codex(1, &ritem(r#"{"type":"custom_tool_call_output","call_id":"c1","output":[{"type":"input_text","text":"Done"},{"type":"input_text","text":"more"}]}"#)));
        assert_eq!(o.blocks, vec![ChatBlock::ToolResult { call_id: "c1".into(), output: "Done\nmore".into(), is_error: false }]);
        let s = one(codex(2, &ritem(r#"{"type":"local_shell_call","call_id":"l1","action":{"type":"exec","command":["git","status"]}}"#)));
        assert_eq!(s.blocks, vec![ChatBlock::ToolCall { call_id: "l1".into(), name: "shell".into(), summary: "git status".into() }]);
        let w = one(codex(3, &ritem(r#"{"type":"web_search_call","id":"w1","action":{"type":"search","query":"rust iroh"}}"#)));
        assert_eq!(w.blocks, vec![ChatBlock::ToolCall { call_id: "w1".into(), name: "web_search".into(), summary: "rust iroh".into() }]);
        let f = one(codex(4, &ritem(r#"{"type":"function_call_output","call_id":"c","output":{"content":"nope","success":false}}"#)));
        assert_eq!(f.blocks, vec![ChatBlock::ToolResult { call_id: "c".into(), output: "nope".into(), is_error: true }]);
    }

    #[test]
    fn codex_images_inline_and_path() {
        let l = ritem(r#"{"type":"message","role":"user","content":[{"type":"input_image","image_url":"data:image/png;base64,QUJD"},{"type":"input_image","image_url":"/tmp/x.png"},{"type":"input_image","image_url":"https://e.com/a.png"},{"type":"input_text","text":"look"}]}"#);
        let d = codex(30, &l);
        assert_eq!(
            d.images,
            vec![ImageSrc::Inline { mime: "image/png".into(), b64: "QUJD".into() }, ImageSrc::Path(PathBuf::from("/tmp/x.png"))]
        );
        let refs: Vec<&str> = d.messages[0].blocks.iter().filter_map(|b| if let ChatBlock::Image { image_ref, .. } = b { Some(image_ref.as_str()) } else { None }).collect();
        assert_eq!(refs, vec!["i30.0", "i30.1"]);
    }

    #[test]
    fn codex_unknown_and_malformed_lines_are_skipped() {
        for l in [
            r#"{"type":"session_meta","payload":{"id":"x"}}"#,
            r#"{"type":"turn_context","payload":{}}"#,
            r#"{"type":"response_item","payload":{"type":"additional_tools","tools":[]}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":"notalist"}}"#,
            r#"{"type":"response_item"}"#,
            "response_item {oops",
            "",
        ] {
            assert!(codex(0, l).messages.is_empty(), "{l}");
        }
    }

    #[test]
    fn decode_line_dispatches_by_format() {
        let l = r#"{"type":"user","uuid":"u","message":{"content":"hi"}}"#;
        assert_eq!(decode_line(Format::Claude, 0, l).messages.len(), 1);
        assert!(decode_line(Format::Codex, 0, l).messages.is_empty());
    }
}
