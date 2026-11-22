//! AI agent status state machine. Pure logic: no IPC, no clocks (the caller passes `now`).
//!
//! Two sources feed it:
//! - Claude Code, Codex and omp hook events, which are authoritative;
//! - terminal title glyphs, used only while the terminal has not received any hook event
//!   during the current agent session.

use std::collections::{HashMap, VecDeque};

use sich_proto::{AgentKind, AgentState, AgentStatus, SubagentActivity, SubagentActivityKey, SubagentStatus};

use crate::codex_rollout::{IdentityPlan, IdentityRead, IdentityResult};
use crate::model_probe::{self, ModelPlan, ModelResult, ModelSource};

mod activity;
mod omp;

/// `AgentStatus::detail` is cut to this many characters.
pub const MAX_DETAIL_CHARS: usize = 120;

/// A `PostToolUse` arriving this soon after `Stop` is a straggler and does not reopen the turn.
pub const STRAY_POST_TOOL_MS: u64 = 1000;

/// Hook events that change (or end) the state; anything else is ignored.
const KNOWN_CLAUDE_EVENTS: &[&str] = &[
    "SessionStart",
    "SessionEnd",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "SubagentStart",
    "PermissionRequest",
    "Notification",
    "Elicitation",
    "Stop",
    "StopFailure",
    "SubagentStop",
    "PostModelSwitch",
];

/// At most this many subagents are tracked per terminal.
pub const MAX_SUBAGENTS: usize = 16;

/// At most this many spawn requests (main-agent `Agent`/`Task` tool calls) wait for their start.
pub const MAX_PENDING_SPAWNS: usize = 16;

/// `agent_id` prefix of agent-type hook runs: they never get `SubagentStop`, so they are ignored.
const HOOK_AGENT_PREFIX: &str = "hook-agent-";

/// `agent_id` is cut to this many characters.
const MAX_AGENT_ID_CHARS: usize = 64;

/// Prompt / assistant message / session title are cut to this many characters.
pub const MAX_TEXT_CHARS: usize = 400;

/// The fields we use from a Claude hook payload (only documented hook input fields).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaudeHook {
    pub session_id: Option<String>,
    pub event: String,
    pub tool_name: Option<String>,
    /// Ready-made label for the tool call: `Bash: <command>`, `Edit: <file name>`.
    pub tool_label: Option<String>,
    pub notification_type: Option<String>,
    pub message: Option<String>,
    pub error: Option<String>,
    /// `SessionStart.model`.
    pub model: Option<String>,
    /// `PostModelSwitch.to_model`.
    pub to_model: Option<String>,
    /// `SessionStart.session_title`.
    pub session_title: Option<String>,
    /// `UserPromptSubmit.prompt`, normalized.
    pub prompt: Option<String>,
    /// `Stop` / `SubagentStop` / `StopFailure` `last_assistant_message`, normalized.
    pub last_assistant_message: Option<String>,
    /// Set when the event happened inside a subagent (`agent_id`, trimmed, <= 64 chars).
    pub agent_id: Option<String>,
    /// Subagent type, e.g. "Explore" (trimmed).
    pub agent_type: Option<String>,
    /// Main-agent `PreToolUse` of the `Agent`/`Task` tool: a subagent is about to start.
    pub spawn: Option<SubagentSpawn>,
    /// `tool_input.subagent_type` of a main-agent `Agent`/`Task` tool event (any event).
    pub spawn_tool_type: Option<String>,
    /// `Stop` / `SubagentStop` inventory: `None` when absent or not an array.
    pub background_tasks: Option<Vec<BgAgentTask>>,
    /// Transcript of the agent that fired the event (a subagent's own for child events).
    pub transcript_path: Option<String>,
    activity: Option<activity::Note>,
}

/// A queued request to start a subagent (from the main agent's `Agent`/`Task` tool call).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentSpawn {
    pub agent_type: String,
    pub description: Option<String>,
}

/// One agent entry of `background_tasks`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BgAgentTask {
    pub id: String,
    pub agent_type: Option<String>,
    pub description: Option<String>,
    pub running: bool,
}

const BG_AGENT_TYPES: &[&str] = &["local_agent", "local_subagent", "subagent"];

const BG_TERMINAL_STATUSES: &[&str] = &[
    "idle", "done", "success", "succeeded", "complete", "completed", "finished", "failed", "error",
    "terminated", "exited", "aborted", "expired", "skipped", "crashed", "killed", "cancelled",
    "canceled", "timed_out",
];

fn parse_background_tasks(v: Option<&serde_json::Value>) -> Option<Vec<BgAgentTask>> {
    let arr = v?.as_array()?;
    let mut out = Vec::new();
    for item in arr {
        let Some(o) = item.as_object() else { continue };
        let text = |k: &str| o.get(k).and_then(|x| x.as_str());
        if !text("type").is_some_and(|t| BG_AGENT_TYPES.contains(&t)) {
            continue;
        }
        let Some(id) = text("id").map(str::trim).filter(|s| !s.is_empty()) else { continue };
        let running = match text("status") {
            Some(st) => !BG_TERMINAL_STATUSES.contains(&st.trim().to_lowercase().as_str()),
            None => true,
        };
        out.push(BgAgentTask {
            id: id.chars().take(MAX_AGENT_ID_CHARS).collect(),
            agent_type: text("agent_type").map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
            description: text("description").and_then(|s| normalize_text(s, false)),
            running,
        });
    }
    Some(out)
}

/// "unknown", "untitled", "task", "subagent" carry no information.
pub fn is_placeholder_label(s: &str) -> bool {
    let l = s.trim().to_lowercase();
    ["unknown", "untitled", "task", "subagent"].contains(&l.as_str())
}

fn real_label(s: &Option<String>) -> Option<String> {
    s.clone().filter(|s| !is_placeholder_label(s))
}

impl ClaudeHook {
    pub fn is_known(&self) -> bool {
        KNOWN_CLAUDE_EVENTS.contains(&self.event.as_str())
    }
}

/// Cuts at `max` chars on a char boundary, appending "…" when something was removed.
fn cap_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('\u{2026}');
    out
}

/// Replaces every `<pasted_content …>…</pasted_content>` block with "[…]" (an unclosed block
/// swallows the rest of the text).
fn strip_pasted(s: &str) -> String {
    const OPEN: &str = "<pasted_content";
    const CLOSE: &str = "</pasted_content>";
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find(OPEN) {
        out.push_str(&rest[..start]);
        out.push_str("[\u{2026}]");
        let after = &rest[start..];
        match after.find(CLOSE) {
            Some(end) => rest = &after[end + CLOSE.len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Normalizes free text for display: optionally strips pasted blocks, collapses all whitespace
/// to single spaces, trims, caps at [`MAX_TEXT_CHARS`]. Empty → `None`.
pub fn normalize_text(s: &str, strip_pasted_blocks: bool) -> Option<String> {
    let stripped;
    let s = if strip_pasted_blocks {
        stripped = strip_pasted(s);
        stripped.as_str()
    } else {
        s
    };
    let collapsed = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() { None } else { Some(cap_chars(&collapsed, MAX_TEXT_CHARS)) }
}

fn file_name_of(path: &str) -> Option<String> {
    let name = path.trim_end_matches(['/', '\\']).rsplit(['/', '\\']).next()?.trim();
    if name.is_empty() { None } else { Some(name.to_string()) }
}

/// `<Tool>: <command>` for any tool whose input has a string `command` (Bash, PowerShell, …),
/// `<Tool>: <file name>` for file tools, `None` otherwise.
pub fn tool_label(tool: &str, input: Option<&serde_json::Map<String, serde_json::Value>>) -> Option<String> {
    let input = input?;
    let text = |k: &str| input.get(k).and_then(|x| x.as_str());
    match tool {
        "Edit" | "MultiEdit" | "Write" | "Read" => {
            Some(format!("{tool}: {}", file_name_of(text("file_path")?)?))
        }
        "NotebookEdit" => Some(format!("{tool}: {}", file_name_of(text("notebook_path")?)?)),
        // Bash, PowerShell, Codex shell tools: anything with a string `command`.
        _ => Some(format!("{tool}: {}", normalize_text(text("command")?, false)?)),
    }
}

/// Parses the hook stdin JSON. `None` if it is not a JSON object with `hook_event_name`.
pub fn parse_claude_payload(json: &str) -> Option<ClaudeHook> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let obj = v.as_object()?;
    let get = |k: &str| obj.get(k).and_then(|x| x.as_str()).map(str::to_string);
    let nonempty = |k: &str| get(k).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let tool_name = get("tool_name");
    let tool_label = tool_name
        .as_deref()
        .and_then(|t| tool_label(t, obj.get("tool_input").and_then(|x| x.as_object())));
    let event = get("hook_event_name")?;
    let agent_id = nonempty("agent_id").map(|s| s.chars().take(MAX_AGENT_ID_CHARS).collect::<String>());
    let is_spawn_tool = agent_id.is_none() && matches!(tool_name.as_deref(), Some("Agent" | "Task"));
    let spawn_input = obj.get("tool_input").and_then(|x| x.as_object());
    let spawn_tool_type = is_spawn_tool.then(|| {
        spawn_input
            .and_then(|i| i.get("subagent_type"))
            .and_then(|x| x.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("general-purpose")
            .to_string()
    });
    let spawn = match (&spawn_tool_type, event == "PreToolUse") {
        (Some(t), true) => Some(SubagentSpawn {
            agent_type: t.clone(),
            description: spawn_input
                .and_then(|i| i.get("description"))
                .and_then(|x| x.as_str())
                .and_then(|s| normalize_text(s, false)),
        }),
        _ => None,
    };
    let activity = activity::hook_note(&event, agent_id.as_deref(), tool_name.as_deref(), obj);
    Some(ClaudeHook {
        session_id: get("session_id").filter(|id| !id.is_empty()),
        event,
        agent_id,
        agent_type: nonempty("agent_type"),
        spawn,
        spawn_tool_type,
        background_tasks: parse_background_tasks(obj.get("background_tasks")),
        tool_name,
        tool_label,
        notification_type: get("notification_type"),
        message: get("message"),
        error: get("error"),
        model: nonempty("model"),
        to_model: nonempty("to_model"),
        session_title: get("session_title").and_then(|s| normalize_text(&s, false)),
        prompt: get("prompt").and_then(|s| normalize_text(&s, true)),
        last_assistant_message: get("last_assistant_message").and_then(|s| normalize_text(&s, false)),
        transcript_path: get("transcript_path"),
        activity,
    })
}

/// Hook events of the OpenAI Codex CLI that change (or end) the state.
const KNOWN_CODEX_EVENTS: &[&str] = &[
    "SessionStart",
    "SessionEnd",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PermissionRequest",
    "Stop",
    "Interrupt",
    "SubagentStart",
    "SubagentStop",
];

/// The fields we use from a Codex hook payload (documented hook input fields only).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CodexHook {
    pub session_id: Option<String>,
    pub event: String,
    pub tool_name: Option<String>,
    /// `<tool>: <command>` when `tool_input.command` exists (string, or array joined by spaces),
    /// else the Claude-style file label.
    pub tool_label: Option<String>,
    /// `PermissionRequest` `tool_input.description`, normalized.
    pub description: Option<String>,
    pub model: Option<String>,
    pub prompt: Option<String>,
    pub last_assistant_message: Option<String>,
    /// Set when the event fired inside a subagent (trimmed, non-empty, <= 64 chars).
    pub agent_id: Option<String>,
    pub agent_type: Option<String>,
    /// Rollout file of the agent that fired the event (a child's own rollout for child events).
    pub transcript_path: Option<String>,
    activity: Option<activity::Note>,
}

impl CodexHook {
    pub fn is_known(&self) -> bool {
        KNOWN_CODEX_EVENTS.contains(&self.event.as_str())
    }
}

fn codex_tool_label(tool: &str, input: Option<&serde_json::Map<String, serde_json::Value>>) -> Option<String> {
    let input = input?;
    let command = match input.get("command") {
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        Some(serde_json::Value::Array(a)) => {
            let parts: Vec<&str> = a.iter().filter_map(|x| x.as_str()).collect();
            Some(parts.join(" "))
        }
        _ => None,
    };
    if let Some(cmd) = command.and_then(|c| normalize_text(&c, false)) {
        return Some(format!("{tool}: {cmd}"));
    }
    tool_label(tool, Some(input))
}

/// Parses the Codex hook stdin JSON. `None` if it is not a JSON object with `hook_event_name`.
pub fn parse_codex_payload(json: &str) -> Option<CodexHook> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let obj = v.as_object()?;
    let get = |k: &str| obj.get(k).and_then(|x| x.as_str()).map(str::to_string);
    let tool_name = get("tool_name");
    let input = obj.get("tool_input").and_then(|x| x.as_object());
    let tool_label = tool_name.as_deref().and_then(|t| codex_tool_label(t, input));
    let nonempty = |k: &str| get(k).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let description =
        input.and_then(|i| i.get("description")).and_then(|x| x.as_str()).and_then(|s| normalize_text(s, false));
    let event = get("hook_event_name")?;
    let agent_id = nonempty("agent_id").map(|s| s.chars().take(MAX_AGENT_ID_CHARS).collect::<String>());
    let model = obj.get("model").and_then(|x| x.as_str()).map(str::trim).filter(|s| !s.is_empty())
        .map(|s| if agent_id.is_some() { clip(s) } else { s.to_owned() });
    let activity = activity::hook_note(&event, agent_id.as_deref(), tool_name.as_deref(), obj);
    Some(CodexHook {
        session_id: get("session_id").filter(|id| !id.is_empty()),
        event,
        tool_name,
        tool_label,
        description,
        model,
        prompt: get("prompt").and_then(|s| normalize_text(&s, true)),
        last_assistant_message: get("last_assistant_message").and_then(|s| normalize_text(&s, false)),
        agent_id,
        agent_type: nonempty("agent_type"),
        transcript_path: get("transcript_path"),
        activity,
    })
}

pub enum AgentInput<'a> {
    Claude(&'a ClaudeHook),
    Codex(&'a CodexHook),
    /// A new terminal title. `hooks_seen`: this terminal already got hook events in the current
    /// agent session, so the title must not override them.
    Title { title: &'a str, hooks_seen: bool },
    /// The process exited or the terminal was removed.
    ProcessExit,
    /// The user looked at the terminal.
    MarkSeen,
}

fn clip(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() <= MAX_DETAIL_CHARS {
        return s.to_string();
    }
    s.chars().take(MAX_DETAIL_CHARS).collect()
}

fn non_empty(s: &Option<String>) -> Option<String> {
    s.as_deref().map(clip).filter(|s| !s.is_empty())
}

/// Moves `prev` to the given state. Keeps `since` while the state stays the same and returns
/// `prev` untouched when nothing changed.
fn settle(
    prev: Option<AgentStatus>,
    kind: AgentKind,
    state: AgentState,
    unread: bool,
    detail: Option<String>,
    now: u64,
) -> Option<AgentStatus> {
    let since = match &prev {
        Some(p) if p.kind == kind && p.state == state => p.since_unix_ms,
        _ => now,
    };
    // Model, title, prompt, message and session ID are sticky within one agent session.
    let (model, title, prompt, message) = match &prev {
        Some(p) if p.kind == kind => (p.model.clone(), p.title.clone(), p.prompt.clone(), p.message.clone()),
        _ => (None, None, None, None),
    };
    let subagents = match &prev {
        Some(p) if p.kind == kind => p.subagents.clone(),
        _ => Vec::new(),
    };
    let session_id = prev.filter(|p| p.kind == kind).and_then(|p| p.session_id);
    Some(AgentStatus { kind, state, unread, since_unix_ms: since, detail, model, title, prompt, message, subagents, session_id })
}

/// All transitions. Returns the new status; equal to `prev` when nothing changed.
pub fn apply(prev: Option<AgentStatus>, input: AgentInput<'_>, now: u64) -> Option<AgentStatus> {
    apply_with(prev, input, now, &mut VecDeque::new())
}

/// Like [`apply`], with the terminal's queue of pending subagent spawns.
pub fn apply_with(
    prev: Option<AgentStatus>,
    input: AgentInput<'_>,
    now: u64,
    pending: &mut VecDeque<SubagentSpawn>,
) -> Option<AgentStatus> {
    match input {
        AgentInput::ProcessExit => None,
        AgentInput::MarkSeen => match prev {
            Some(mut p) => {
                // Seen only clears `unread`: a finished agent stays Done (the UI shows a quiet
                // "finished" dot) and keeps its finish time.
                let _ = now;
                p.unread = false;
                Some(p)
            }
            None => None,
        },
        AgentInput::Claude(h) => apply_claude(prev, h, now, pending),
        AgentInput::Codex(h) => apply_codex(prev, h, now),
        AgentInput::Title { title, hooks_seen } => {
            if hooks_seen {
                prev
            } else {
                apply_title(prev, title, now)
            }
        }
    }
}

/// Finds or creates a subagent entry (`None` when the roster is full and the id is new).
fn upsert_sub<'a>(list: &'a mut Vec<SubagentStatus>, id: &str, now: u64) -> Option<&'a mut SubagentStatus> {
    if let Some(i) = list.iter().position(|s| s.id == id) {
        return Some(&mut list[i]);
    }
    if list.len() >= MAX_SUBAGENTS {
        return None;
    }
    list.push(SubagentStatus {
        id: id.to_string(),
        agent_type: None,
        description: None,
        state: AgentState::Working,
        detail: None,
        since_unix_ms: now,
        model: None,
    });
    list.last_mut()
}

/// Syncs the roster with the `background_tasks` inventory (`None` = keep the roster). `exclude`
/// is a subagent that just stopped: its task may still be listed as running, so it is never
/// retained or re-added.
fn reconcile(list: &mut Vec<SubagentStatus>, bg: &Option<Vec<BgAgentTask>>, now: u64, exclude: Option<&str>) {
    let Some(bg) = bg else { return };
    list.retain(|s| Some(s.id.as_str()) != exclude && bg.iter().any(|t| t.running && t.id == s.id));
    for t in bg.iter().filter(|t| Some(t.id.as_str()) != exclude) {
        if let Some(s) = list.iter_mut().find(|s| s.id == t.id) {
            if let Some(a) = real_label(&t.agent_type) {
                s.agent_type = Some(a);
            }
            if let Some(d) = real_label(&t.description) {
                s.description = Some(d);
            }
        } else if t.running && list.len() < MAX_SUBAGENTS {
            list.push(SubagentStatus {
                id: t.id.clone(),
                agent_type: real_label(&t.agent_type),
                description: real_label(&t.description),
                state: AgentState::Working,
                detail: None,
                since_unix_ms: now,
                model: None,
            });
        }
    }
}

fn apply_claude(
    prev: Option<AgentStatus>,
    h: &ClaudeHook,
    now: u64,
    pending: &mut VecDeque<SubagentSpawn>,
) -> Option<AgentStatus> {
    let kind = AgentKind::Claude;
    let tool = h.tool_label.as_ref().or(h.tool_name.as_ref()).map(|t| clip(t)).filter(|t| !t.is_empty());
    if h.agent_id.as_deref().is_some_and(|id| id.starts_with(HOOK_AGENT_PREFIX)) {
        return prev;
    }
    if let Some(sp) = &h.spawn {
        if pending.len() >= MAX_PENDING_SPAWNS {
            pending.pop_front();
        }
        pending.push_back(sp.clone());
    }
    if matches!(h.event.as_str(), "PostToolUse" | "PostToolUseFailure") {
        if let Some(t) = &h.spawn_tool_type {
            // The spawn call finished without its subagent ever starting.
            if let Some(i) = pending.iter().position(|p| p.agent_type.eq_ignore_ascii_case(t)) {
                pending.remove(i);
            }
        }
    }
    if matches!(h.event.as_str(), "SessionStart" | "SessionEnd" | "Stop" | "StopFailure") && h.agent_id.is_none() {
        pending.clear();
    }
    if let Some(id) = h.agent_id.as_deref() {
        // Events from inside a subagent never change the main status.
        let waiting_detail = match h.event.as_str() {
            "PreToolUse" if h.tool_name.as_deref() == Some("AskUserQuestion") => Some(tool.clone()),
            "PermissionRequest" => Some(tool.clone()),
            "Notification" if h.notification_type.as_deref() != Some("idle_prompt") => Some(non_empty(&h.message)),
            "Elicitation" => Some(non_empty(&h.message)),
            _ => None,
        };
        match h.event.as_str() {
            "SubagentStart" | "SubagentStop" => {}
            "SessionStart" | "PostModelSwitch" => {
                let mut p = prev?;
                if let Some(sub) = p.kind.eq(&kind).then(|| p.subagents.iter_mut().find(|s| s.id == id)).flatten() {
                    let model = if h.event == "SessionStart" { &h.model } else { &h.to_model };
                    if let Some(model) = non_empty(model) {
                        sub.model = Some(model);
                    }
                }
                return Some(p);
            }
            "Notification" if h.notification_type.as_deref() == Some("idle_prompt") => return prev,
            "StopFailure" => {
                let mut p = prev?;
                p.subagents.retain(|s| s.id != id);
                return Some(p);
            }
            "PreToolUse" | "PostToolUse" | "PostToolUseFailure" | "PermissionRequest" | "Notification"
            | "Elicitation" => {
                let mut p = prev?;
                if let Some(sub) = upsert_sub(&mut p.subagents, id, now) {
                    if let Some(a) = real_label(&h.agent_type) {
                        sub.agent_type = Some(a);
                    }
                    match waiting_detail {
                        Some(d) => {
                            sub.state = AgentState::Waiting;
                            sub.detail = d;
                        }
                        None if h.event == "Notification" => {}
                        None => {
                            sub.state = AgentState::Working;
                            sub.detail = tool;
                        }
                    }
                }
                return Some(p);
            }
            _ => return prev,
        }
    }
    match h.event.as_str() {
        // Only record the model / the subagent's final message; the state does not change.
        "PostModelSwitch" | "SubagentStop" => {
            let mut p = prev?;
            if h.event == "PostModelSwitch" {
                if h.session_id.is_some() && p.kind == kind {
                    p.session_id.clone_from(&h.session_id);
                }
                if let Some(m) = &h.to_model {
                    p.model = Some(m.clone());
                }
            }
            if h.event == "SubagentStop" {
                if h.last_assistant_message.is_some() {
                    p.message = h.last_assistant_message.clone();
                }
                if let Some(id) = h.agent_id.as_deref() {
                    p.subagents.retain(|s| s.id != id);
                }
                reconcile(&mut p.subagents, &h.background_tasks, now, h.agent_id.as_deref());
            }
            return Some(p);
        }
        _ => {}
    }
    let (state, unread, detail) = match h.event.as_str() {
        "SessionEnd" => return None,
        "SessionStart" => (AgentState::Idle, false, None),
        "UserPromptSubmit" | "SubagentStart" => (AgentState::Working, false, None),
        "PreToolUse" if h.tool_name.as_deref() == Some("AskUserQuestion") => {
            (AgentState::Waiting, false, tool)
        }
        "PreToolUse" => (AgentState::Working, false, tool),
        "PostToolUse" | "PostToolUseFailure" => {
            let straggler = prev.as_ref().is_some_and(|p| {
                matches!(p.state, AgentState::Done | AgentState::Failed)
                    && now.saturating_sub(p.since_unix_ms) < STRAY_POST_TOOL_MS
            });
            if straggler {
                return prev;
            }
            (AgentState::Working, false, tool)
        }
        "PermissionRequest" => (AgentState::Waiting, false, tool),
        // `idle_prompt` fires ~60 s after the turn ended; Done already means "your turn",
        // so it must not turn a finished agent into Waiting.
        "Notification" if h.notification_type.as_deref() == Some("idle_prompt") => return prev,
        "Notification" => (AgentState::Waiting, false, non_empty(&h.message)),
        "Elicitation" => (AgentState::Waiting, false, non_empty(&h.message)),
        "Stop" => (AgentState::Done, true, None),
        "StopFailure" => (AgentState::Failed, true, non_empty(&h.error)),
        _ => return prev,
    };
    let mut s = settle(prev, kind, state, unread, detail, now)?;
    if h.agent_id.is_none() && h.session_id.is_some() {
        s.session_id.clone_from(&h.session_id);
    }
    if h.event == "SessionStart" {
        s.subagents.clear();
    }
    if h.event == "Stop" || h.event == "StopFailure" {
        reconcile(&mut s.subagents, &h.background_tasks, now, None);
    }
    if h.event == "SubagentStart" {
        if let Some(id) = h.agent_id.as_deref() {
            let is_new = !s.subagents.iter().any(|x| x.id == id);
            let desc = if is_new && s.subagents.len() < MAX_SUBAGENTS {
                let want = h.agent_type.as_deref().unwrap_or("");
                pending
                    .iter()
                    .position(|p| p.agent_type.eq_ignore_ascii_case(want))
                    .and_then(|i| pending.remove(i))
                    .and_then(|p| real_label(&p.description))
            } else {
                None
            };
            if let Some(sub) = upsert_sub(&mut s.subagents, id, now) {
                if let Some(a) = real_label(&h.agent_type) {
                    sub.agent_type = Some(a);
                }
                sub.state = AgentState::Working;
                sub.detail = None;
                if desc.is_some() {
                    sub.description = desc;
                }
            }
        }
    }
    // SubagentStart has no documented model; a child SessionStart is handled above.
    if h.event == "SessionStart" {
        if let Some(m) = &h.model {
            s.model = Some(m.clone());
        }
    }
    if let Some(t) = &h.session_title {
        s.title = Some(t.clone());
    }
    match h.event.as_str() {
        "UserPromptSubmit" => {
            if h.prompt.is_some() {
                s.prompt = h.prompt.clone();
            }
            s.message = None;
        }
        "Stop" | "StopFailure" => {
            if h.last_assistant_message.is_some() {
                s.message = h.last_assistant_message.clone();
            }
        }
        _ => {}
    }
    Some(s)
}

fn apply_codex(prev: Option<AgentStatus>, h: &CodexHook, now: u64) -> Option<AgentStatus> {
    let kind = AgentKind::Codex;
    let tool = h.tool_label.as_ref().or(h.tool_name.as_ref()).map(|t| clip(t)).filter(|t| !t.is_empty());
    if let Some(id) = h.agent_id.as_deref() {
        // Events from inside a subagent never change the main status.
        if prev.as_ref().is_some_and(|s| s.kind != kind) {
            return prev;
        }
        match h.event.as_str() {
            "SubagentStart" | "PreToolUse" | "PostToolUse" | "PermissionRequest" => {
                let mut p = prev?;
                if let Some(sub) = upsert_sub(&mut p.subagents, id, now) {
                    if let Some(a) = real_label(&h.agent_type) {
                        sub.agent_type = Some(a);
                    }
                    if let Some(model) = &h.model
                        && sub.model.as_ref() != Some(model)
                    {
                        sub.model = Some(model.clone());
                    }
                    match h.event.as_str() {
                        "SubagentStart" => {
                            sub.state = AgentState::Working;
                            sub.detail = None;
                        }
                        "PermissionRequest" => {
                            sub.state = AgentState::Waiting;
                            sub.detail = non_empty(&h.description).or(tool);
                        }
                        _ => {
                            sub.state = AgentState::Working;
                            sub.detail = tool;
                        }
                    }
                }
                return Some(p);
            }
            "SubagentStop" | "Stop" => {
                let mut p = prev?;
                p.subagents.retain(|s| s.id != id);
                return Some(p);
            }
            // Child SessionStart and anything else: ignored.
            _ => return prev,
        }
    }
    let (state, unread, detail) = match h.event.as_str() {
        "SessionEnd" => return None,
        "SubagentStart" | "SubagentStop" => return prev,
        "SessionStart" => (AgentState::Idle, false, None),
        "UserPromptSubmit" => (AgentState::Working, false, None),
        "PreToolUse" => (AgentState::Working, false, tool),
        "PostToolUse" => {
            let straggler = prev.as_ref().is_some_and(|p| {
                matches!(p.state, AgentState::Done | AgentState::Failed)
                    && now.saturating_sub(p.since_unix_ms) < STRAY_POST_TOOL_MS
            });
            if straggler {
                return prev;
            }
            (AgentState::Working, false, tool)
        }
        "PermissionRequest" => (AgentState::Waiting, false, non_empty(&h.description).or(tool)),
        "Stop" => (AgentState::Done, true, None),
        // The turn was cancelled: nothing to read, back to idle (prompt/model stay).
        "Interrupt" => (AgentState::Idle, false, None),
        _ => return prev,
    };
    let mut s = settle(prev, kind, state, unread, detail, now)?;
    if h.session_id.is_some() {
        s.session_id.clone_from(&h.session_id);
    }
    // New session / the turn ended or was cancelled: any child still listed is stale (Codex
    // sometimes misses child stops).
    if matches!(h.event.as_str(), "SessionStart" | "Stop" | "Interrupt") {
        s.subagents.clear();
    }
    if let Some(m) = &h.model {
        s.model = Some(m.clone());
    }
    match h.event.as_str() {
        "UserPromptSubmit" => {
            if h.prompt.is_some() {
                s.prompt = h.prompt.clone();
            }
            s.message = None;
        }
        "Stop" => {
            if h.last_assistant_message.is_some() {
                s.message = h.last_assistant_message.clone();
            }
        }
        _ => {}
    }
    Some(s)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TitleSignal {
    Working,
    Waiting,
    Idle,
}

const CLAUDE_IDLE: char = '\u{2733}'; // ✳
const GEMINI_WORKING: char = '\u{2726}'; // ✦
const GEMINI_SILENT_WORKING: char = '\u{23f2}'; // ⏲
const GEMINI_IDLE: char = '\u{25c7}'; // ◇
const GEMINI_PERMISSION: char = '\u{270b}'; // ✋

fn is_braille(c: char) -> bool {
    ('\u{2800}'..='\u{28ff}').contains(&c)
}

fn is_quarter_circle(c: char) -> bool {
    ('\u{25d0}'..='\u{25d3}').contains(&c)
}

fn title_signal(title: &str) -> Option<(AgentKind, TitleSignal)> {
    let c = title.trim_start().chars().next()?;
    match c {
        // Codex v0.170.0 uses these frames at startup and during work. The rest of the
        // title is configurable and can look like a Claude session title.
        '⠋' | '⠙' | '⠹' | '⠸' | '⠼' | '⠴' | '⠦' | '⠧' | '⠇' | '⠏' => {
            Some((AgentKind::Other, TitleSignal::Working))
        }
        c if is_braille(c) || is_quarter_circle(c) => Some((AgentKind::Claude, TitleSignal::Working)),
        CLAUDE_IDLE => Some((AgentKind::Claude, TitleSignal::Idle)),
        GEMINI_WORKING | GEMINI_SILENT_WORKING => Some((AgentKind::Gemini, TitleSignal::Working)),
        GEMINI_IDLE => Some((AgentKind::Gemini, TitleSignal::Idle)),
        GEMINI_PERMISSION => Some((AgentKind::Gemini, TitleSignal::Waiting)),
        _ => None,
    }
}

fn apply_title(prev: Option<AgentStatus>, title: &str, now: u64) -> Option<AgentStatus> {
    let Some((kind, sig)) = title_signal(title) else {
        // Codex drops the spinner from its title when ready. Clear unconfirmed activity
        // on a plain title, which may be transient for an identified working agent.
        return match prev {
            Some(p) if p.kind != AgentKind::Other && p.state == AgentState::Working => Some(p),
            _ => None,
        };
    };
    // A different agent kind than before: start from scratch.
    let prev = prev.filter(|p| p.kind == kind);
    let prev_state = prev.as_ref().map(|p| p.state);
    match sig {
        TitleSignal::Working => settle(prev, kind, AgentState::Working, false, None, now),
        TitleSignal::Waiting => settle(prev, kind, AgentState::Waiting, false, None, now),
        TitleSignal::Idle => match prev_state {
            Some(AgentState::Working) => settle(prev, kind, AgentState::Done, true, None, now),
            // Already finished (or failed) and maybe unread: the idle glyph changes nothing.
            Some(AgentState::Done | AgentState::Failed | AgentState::Idle) => prev,
            Some(AgentState::Waiting) | None => {
                settle(prev, kind, AgentState::Idle, false, None, now)
            }
        },
    }
}

/// Per-terminal agent state: the status plus whether hooks took over from the title fallback.
#[derive(Debug, Default)]
pub struct AgentTracker {
    status: Option<AgentStatus>,
    hooks_seen: bool,
    /// Main-agent `Agent`/`Task` calls whose subagent has not started yet (oldest first).
    pending_spawns: VecDeque<SubagentSpawn>,
    /// Codex subagent rollout identity bookkeeping (agent_id -> state); roster members only.
    identities: HashMap<String, IdentityState>,
    /// Subagent model read bookkeeping (agent_id -> attempts); roster members only.
    model_probes: HashMap<String, ModelProbe>,
    activity: activity::Book,
}

/// How often the model of one subagent was looked up in its agent's files.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ModelProbe {
    tries: u8,
    last_ms: u64,
}

/// A subagent's model file is read no more often than this ...
const MODEL_PROBE_INTERVAL_MS: u64 = 2000;
/// ... and never more than this many times.
const MODEL_PROBE_MAX_TRIES: u8 = 4;

/// Where the rollout identity read of one Codex subagent stands.
#[derive(Debug, Clone, PartialEq, Eq)]
enum IdentityState {
    /// The label was found: never read again.
    Named,
    /// A complete first line without identity for this path: never retried for it.
    Missed(String),
    /// Not readable yet at this file length: retried only when the length changes.
    Pending(String, u64),
    /// Other I/O error: retried no sooner than this time (unix ms).
    Unreadable(u64),
}

/// Unreadable rollouts are retried no sooner than this.
const IDENTITY_RETRY_MS: u64 = 5000;

/// A `transcript_path` we may read: absolute and ending with `.jsonl`.
fn valid_rollout_path(p: &str) -> bool {
    std::path::Path::new(p).is_absolute() && p.to_ascii_lowercase().ends_with(".jsonl")
}

impl AgentTracker {
    pub fn status(&self) -> Option<&AgentStatus> {
        self.status.as_ref()
    }

    #[cfg(test)]
    fn pending_spawns(&self) -> &VecDeque<SubagentSpawn> {
        &self.pending_spawns
    }

    fn update(&mut self, f: impl FnOnce(Option<AgentStatus>) -> Option<AgentStatus>) -> bool {
        let old = self.status.clone();
        self.status = f(self.status.take());
        self.status != old
    }

    /// Returns true if the status changed.
    pub fn on_claude_hook(&mut self, h: &ClaudeHook, now: u64) -> bool {
        if !h.is_known() {
            return false;
        }
        self.hooks_seen = h.event != "SessionEnd";
        let old = self.status.clone();
        self.status = apply_with(self.status.take(), AgentInput::Claude(h), now, &mut self.pending_spawns);
        // SessionStart normally sets this; a first observed Claude hook can be SubagentStart.
        if let Some(status) = self.status.as_mut().filter(|s| s.kind == AgentKind::Claude && s.session_id.is_none()) {
            status.session_id.clone_from(&h.session_id);
        }
        self.record_hook(AgentKind::Claude, old.as_ref(), h.session_id.as_deref(), h.agent_id.as_deref(), h.activity.as_ref(), now);
        self.prune_model_probes();
        self.status != old
    }

    fn prune_model_probes(&mut self) {
        let roster: Vec<&str> =
            self.status.as_ref().map(|s| s.subagents.iter().map(|x| x.id.as_str()).collect()).unwrap_or_default();
        self.model_probes.retain(|id, _| roster.contains(&id.as_str()));
    }

    /// True when the subagent's model may be looked up now (attempt and interval limits).
    fn model_probe_due(&self, id: &str, now: u64) -> bool {
        self.model_probes.get(id).is_none_or(|p| {
            p.tries < MODEL_PROBE_MAX_TRIES && now >= p.last_ms.saturating_add(MODEL_PROBE_INTERVAL_MS)
        })
    }

    /// Whether `id` is known to already have a model (so nothing needs reading).
    fn sub_has_model(&self, kind: AgentKind, id: &str) -> bool {
        self.status
            .as_ref()
            .filter(|s| s.kind == kind)
            .and_then(|s| s.subagents.iter().find(|x| x.id == id))
            .is_some_and(|x| x.model.is_some())
    }

    /// The model file read (if any) a Claude child event needs. Cheap and pure: the caller reads
    /// outside of any lock ([`model_probe::execute`]) and then calls [`Self::apply_model`] after
    /// applying the hook (a `SubagentStart` only puts the subagent on the roster then).
    pub fn claude_model_plan(&self, h: &ClaudeHook, now: u64) -> Option<ModelPlan> {
        let id = h.agent_id.as_deref().filter(|id| !id.starts_with(HOOK_AGENT_PREFIX))?;
        if !matches!(
            h.event.as_str(),
            "SubagentStart" | "PreToolUse" | "PostToolUse" | "PostToolUseFailure" | "PermissionRequest"
                | "Notification" | "Elicitation"
        ) {
            return None;
        }
        let path = model_probe::claude_meta_path(h.transcript_path.as_deref()?, id)?;
        if self.sub_has_model(AgentKind::Claude, id) || !self.model_probe_due(id, now) {
            return None;
        }
        Some(ModelPlan { agent_id: id.to_string(), session_id: h.session_id.clone(), source: ModelSource::ClaudeMeta, path })
    }

    /// The rollout read a Codex child hook needs for its model: only when the hook itself did not
    /// carry one. Same shape as [`Self::claude_model_plan`].
    pub fn codex_model_plan(&self, h: &CodexHook, now: u64) -> Option<ModelPlan> {
        let id = h.agent_id.as_deref()?;
        if h.model.is_some()
            || !matches!(h.event.as_str(), "SubagentStart" | "PreToolUse" | "PostToolUse" | "PermissionRequest")
        {
            return None;
        }
        let path = h.transcript_path.as_deref().filter(|p| valid_rollout_path(p))?;
        self.status.as_ref().filter(|s| s.kind == AgentKind::Codex)?;
        if self.sub_has_model(AgentKind::Codex, id) || !self.model_probe_due(id, now) {
            return None;
        }
        Some(ModelPlan {
            agent_id: id.to_string(),
            session_id: h.session_id.clone(),
            source: ModelSource::CodexRollout,
            path: path.into(),
        })
    }

    /// Applies a model read. Never overwrites a model, and ignores results for another agent kind,
    /// another session or a subagent that is no longer on the roster. True if the status changed.
    pub fn apply_model(&mut self, r: ModelResult, now: u64) -> bool {
        let kind = match r.source {
            ModelSource::ClaudeMeta => AgentKind::Claude,
            ModelSource::CodexRollout => AgentKind::Codex,
        };
        let changed = 'apply: {
            let Some(status) = self.status.as_mut().filter(|s| s.kind == kind) else { break 'apply false };
            if r.session_id.is_some() && status.session_id.is_some() && status.session_id != r.session_id {
                break 'apply false;
            }
            let Some(sub) = status.subagents.iter_mut().find(|x| x.id == r.agent_id) else { break 'apply false };
            let probe = self.model_probes.entry(r.agent_id).or_default();
            probe.tries = probe.tries.saturating_add(1);
            probe.last_ms = now;
            match r.model.as_deref().map(clip).filter(|m| !m.is_empty()) {
                Some(m) if sub.model.is_none() => {
                    sub.model = Some(m);
                    true
                }
                _ => false,
            }
        };
        self.prune_model_probes();
        changed
    }

    pub(crate) fn subagent_activity(&mut self, key: &SubagentActivityKey, after_seq: u64, now: u64) -> Option<SubagentActivity> {
        self.activity.delta(key, after_seq, now)
    }

    fn record_hook(
        &mut self, kind: AgentKind, previous: Option<&AgentStatus>, session: Option<&str>, id: Option<&str>,
        note: Option<&activity::Note>, now: u64,
    ) {
        let matches = |s: &&AgentStatus| s.kind == kind
            && (session.is_none() || s.session_id.is_none() || s.session_id.as_deref() == session);
        let child = id.zip(note);
        self.activity.distinguish_reused_ids(self.status.as_mut());
        if let Some((id, note)) = child.filter(|(_, note)| activity::terminal(note.kind)) {
            // Explicit stop is applied while the prior roster key is still available.
            self.activity.hook(previous.filter(matches), id, note.clone(), now);
        }
        self.activity.sync(self.status.as_ref(), now);
        if let Some((id, note)) = child.filter(|(_, note)| !activity::terminal(note.kind)) {
            self.activity.hook(self.status.as_ref().filter(matches), id, note.clone(), now);
        }
    }

    /// Returns true if the status changed.
    #[cfg(test)]
    pub fn on_codex_hook(&mut self, h: &CodexHook, now: u64) -> bool {
        self.on_codex_hook_with(h, now, None)
    }

    pub(crate) fn on_omp_hook(&mut self, payload: &str, now: u64) -> bool {
        let Some(mut snapshot) = omp::Snapshot::parse(payload) else {
            return false;
        };
        let owns_current = self.status.as_ref().is_some_and(|status| snapshot.owns(status));
        let status = snapshot.status(self.status.as_ref(), now);
        if status.is_none() && !owns_current {
            return false;
        }
        snapshot.record_activity(&mut self.activity, now);
        self.activity.sync(status.as_ref(), now);
        self.hooks_seen = status.is_some();
        let changed = self.status != status;
        self.status = status;
        self.pending_spawns.clear();
        self.identities.clear();
        self.model_probes.clear();
        changed
    }

    /// The rollout read (if any) a Codex child hook needs before it is applied. Cheap and pure:
    /// the caller does the file read outside of any lock ([`crate::codex_rollout::execute`]).
    pub fn identity_plan(&self, h: &CodexHook, now: u64) -> Option<IdentityPlan> {
        let id = h.agent_id.as_deref()?;
        if !matches!(h.event.as_str(), "SubagentStart" | "PreToolUse" | "PostToolUse" | "PermissionRequest") {
            return None;
        }
        let path = h.transcript_path.as_deref().filter(|p| valid_rollout_path(p))?;
        let status = self.status.as_ref().filter(|s| s.kind == AgentKind::Codex)?;
        match status.subagents.iter().find(|s| s.id == id) {
            Some(sub) if sub.description.is_some() => return None,
            Some(_) => {}
            None if status.subagents.len() >= MAX_SUBAGENTS => return None,
            None => {}
        }
        let pending_len = match self.identities.get(id) {
            None => None,
            Some(IdentityState::Named) => return None,
            Some(IdentityState::Missed(p)) if p == path => return None,
            Some(IdentityState::Pending(p, len)) if p == path => Some(*len),
            Some(IdentityState::Unreadable(retry_at)) if now < *retry_at => return None,
            Some(_) => None,
        };
        Some(IdentityPlan { agent_id: id.to_string(), path: path.to_string(), pending_len })
    }

    /// Applies a Codex hook, then the identity read made for it (if any). True if the status
    /// changed.
    pub fn on_codex_hook_with(&mut self, h: &CodexHook, now: u64, ident: Option<IdentityResult>) -> bool {
        if !h.is_known() {
            return false;
        }
        self.hooks_seen = h.event != "SessionEnd";
        let old = self.status.clone();
        self.status = apply(self.status.take(), AgentInput::Codex(h), now);
        self.record_hook(AgentKind::Codex, old.as_ref(), h.session_id.as_deref(), h.agent_id.as_deref(), h.activity.as_ref(), now);
        let mut changed = self.status != old;
        if let Some(r) = ident {
            changed |= self.apply_identity(r, now);
        }
        // Bookkeeping lives only as long as the subagent is on the roster.
        let roster: Vec<&str> =
            self.status.as_ref().map(|s| s.subagents.iter().map(|x| x.id.as_str()).collect()).unwrap_or_default();
        self.identities.retain(|id, _| roster.contains(&id.as_str()));
        self.prune_model_probes();
        changed
    }

    fn apply_identity(&mut self, r: IdentityResult, now: u64) -> bool {
        let Some(sub) =
            self.status.as_mut().and_then(|s| s.subagents.iter_mut().find(|x| x.id == r.agent_id))
        else {
            return false;
        };
        let (state, changed) = match r.read {
            IdentityRead::Found(label) => {
                let label = clip(&label);
                let changed = !label.is_empty() && sub.description.as_deref() != Some(label.as_str());
                if !label.is_empty() {
                    sub.description = Some(label);
                }
                (IdentityState::Named, changed)
            }
            IdentityRead::Miss => (IdentityState::Missed(r.path), false),
            IdentityRead::Pending => (IdentityState::Pending(r.path, r.len), false),
            IdentityRead::Unreadable => (IdentityState::Unreadable(now + IDENTITY_RETRY_MS), false),
        };
        self.identities.insert(r.agent_id, state);
        changed
    }

    pub fn on_title(&mut self, title: &str, now: u64) -> bool {
        let hooks_seen = self.hooks_seen;
        self.update(|prev| apply(prev, AgentInput::Title { title, hooks_seen }, now))
    }

    pub fn on_exit(&mut self, now: u64) -> bool {
        self.hooks_seen = false;
        self.pending_spawns.clear();
        self.identities.clear();
        self.model_probes.clear();
        self.activity.sync(None, now);
        self.update(|prev| apply(prev, AgentInput::ProcessExit, 0))
    }

    pub fn mark_seen(&mut self, now: u64) -> bool {
        self.update(|prev| apply(prev, AgentInput::MarkSeen, now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u64 = 1_000_000;

    fn hook(json: &str) -> ClaudeHook {
        parse_claude_payload(json).expect("valid hook payload")
    }

    /// A payload shaped like the real thing: common fields plus event specifics.
    fn payload(event: &str, extra: &str) -> String {
        format!(
            r#"{{"session_id":"7d1f6c52-0a41-4a43-9f0e-aa6c2f3a1b11","transcript_path":"C:\\Users\\u\\.claude\\projects\\p\\7d1f.jsonl","cwd":"C:\\dev\\p","permission_mode":"default","hook_event_name":"{event}"{extra}}}"#
        )
    }

    fn run(prev: Option<AgentStatus>, event: &str, extra: &str, now: u64) -> Option<AgentStatus> {
        let h = hook(&payload(event, extra));
        apply(prev, AgentInput::Claude(&h), now)
    }

    fn st(state: AgentState) -> Option<AgentStatus> {
        Some(AgentStatus {
            kind: AgentKind::Claude,
            state,
            unread: false,
            since_unix_ms: T0,
            detail: None,
            model: None,
            title: None,
            prompt: None,
            message: None,
            subagents: Vec::new(),
            session_id: None,
        })
    }

    fn json_str(s: &str) -> String {
        serde_json::to_string(s).unwrap()
    }

    #[test]
    fn normalize_collapses_whitespace_and_trims() {
        assert_eq!(normalize_text("  a \n\n b\t\tc  ", false).as_deref(), Some("a b c"));
        assert_eq!(normalize_text("   \n\t ", false), None);
        assert_eq!(normalize_text("", true), None);
    }

    #[test]
    fn normalize_caps_on_char_boundary() {
        let long = "ї".repeat(MAX_TEXT_CHARS + 50);
        let out = normalize_text(&long, false).unwrap();
        assert_eq!(out.chars().count(), MAX_TEXT_CHARS + 1);
        assert!(out.ends_with('\u{2026}'));
        let exact = "x".repeat(MAX_TEXT_CHARS);
        assert_eq!(normalize_text(&exact, false).unwrap(), exact);
    }

    #[test]
    fn normalize_strips_pasted_blocks_only_when_asked() {
        let text = "look at this\n<pasted_content id=\"1\">\nline1\nline2\n</pasted_content>\nand <pasted_content id=\"2\">x</pasted_content> done";
        assert_eq!(normalize_text(text, true).as_deref(), Some("look at this [\u{2026}] and [\u{2026}] done"));
        assert!(normalize_text(text, false).unwrap().contains("line1 line2"));
        // Unclosed block swallows the rest.
        assert_eq!(normalize_text("a <pasted_content id=\"1\">zzz", true).as_deref(), Some("a [\u{2026}]"));
        // Only a pasted block.
        assert_eq!(normalize_text("<pasted_content id=\"1\">q</pasted_content>", true).as_deref(), Some("[\u{2026}]"));
    }

    #[test]
    fn session_start_model_and_title() {
        let s = run(None, "SessionStart", r#","model":"claude-opus-5-5","session_title":"Sync  phone""#, T0).unwrap();
        assert_eq!(s.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(s.title.as_deref(), Some("Sync phone"));
        // Absent (or empty) model/title are fine.
        let s = run(None, "SessionStart", r#","model":"""#, T0).unwrap();
        assert_eq!((s.model, s.title), (None, None));
    }

    #[test]
    fn model_and_title_are_sticky_until_session_end() {
        let s = run(None, "SessionStart", r#","model":"claude-opus-5-5","session_title":"T""#, T0);
        let s = run(s, "UserPromptSubmit", r#","prompt":"hi""#, T0 + 1);
        let s = run(s, "PreToolUse", r#","tool_name":"Bash""#, T0 + 2);
        let s = run(s, "Stop", "", T0 + 3);
        let s = run(s, "Notification", r#","message":"x""#, T0 + 4).unwrap();
        assert_eq!(s.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(s.title.as_deref(), Some("T"));
        // A later SessionStart without model keeps them.
        let s = run(Some(s), "SessionStart", "", T0 + 5).unwrap();
        assert_eq!(s.model.as_deref(), Some("claude-opus-5-5"));
        // Present again: replaced.
        let s = run(Some(s), "SessionStart", r#","model":"claude-sonnet-5-5""#, T0 + 6).unwrap();
        assert_eq!(s.model.as_deref(), Some("claude-sonnet-5-5"));
        assert_eq!(run(Some(s), "SessionEnd", "", T0 + 7), None);
        let fresh = run(None, "SessionStart", "", T0 + 8).unwrap();
        assert_eq!((fresh.model, fresh.title), (None, None));
    }

    #[test]
    fn post_model_switch_updates_model_only() {
        let s = run(None, "SessionStart", r#","model":"claude-opus-5-5""#, T0);
        let w = run(s, "UserPromptSubmit", r#","prompt":"p""#, T0 + 1);
        let sw = run(w.clone(), "PostModelSwitch", r#","from_model":"a","to_model":"claude-sonnet-5-5""#, T0 + 2).unwrap();
        assert_eq!(sw.model.as_deref(), Some("claude-sonnet-5-5"));
        assert_eq!(sw.state, AgentState::Working);
        assert_eq!(sw.since_unix_ms, w.unwrap().since_unix_ms);
        assert_eq!(sw.prompt.as_deref(), Some("p"));
        // Without status it creates nothing; without to_model nothing changes.
        assert_eq!(run(None, "PostModelSwitch", r#","to_model":"m""#, T0), None);
        let same = run(Some(sw.clone()), "PostModelSwitch", "", T0 + 3);
        assert_eq!(same, Some(sw));
    }

    #[test]
    fn claude_child_models_require_a_live_id_and_a_documented_model_event() {
        let parent = run(None, "SessionStart", r#","model":"parent""#, T0);
        let unknown = run(parent.clone(), "SessionStart", r#","agent_id":"c","model":"child""#, T0 + 1);
        assert_eq!(unknown, parent);

        let started = run(parent, "SubagentStart", r#","agent_id":"c","model":"not-documented""#, T0 + 2).unwrap();
        assert_eq!(started.model.as_deref(), Some("parent"));
        assert_eq!(started.subagents[0].model, None);
        let live = run(Some(started), "SessionStart", r#","agent_id":"c","model":" child ""#, T0 + 3).unwrap();
        assert_eq!(live.model.as_deref(), Some("parent"));
        assert_eq!(live.subagents[0].model.as_deref(), Some("child"));
        let switched = run(Some(live), "PostModelSwitch", r#","agent_id":"c","to_model":"next""#, T0 + 4).unwrap();
        assert_eq!(switched.model.as_deref(), Some("parent"));
        assert_eq!(switched.subagents[0].model.as_deref(), Some("next"));

        let stopped = run(Some(switched), "SubagentStop", r#","agent_id":"c""#, T0 + 5);
        let reused = run(stopped, "SubagentStart", r#","agent_id":"c""#, T0 + 6).unwrap();
        assert_eq!(reused.subagents[0].model, None);
    }

    #[test]
    fn prompt_is_normalized_sticky_and_clears_message() {
        let pasted = r#","prompt":"fix\n\nthis\n<pasted_content id=\"1\">\nbig\n</pasted_content>""#;
        let s = run(None, "UserPromptSubmit", pasted, T0).unwrap();
        assert_eq!(s.prompt.as_deref(), Some("fix this [\u{2026}]"));
        let s = run(Some(s), "PreToolUse", r#","tool_name":"Bash""#, T0 + 1);
        let s = run(s, "Stop", r#","last_assistant_message":"all\ndone""#, T0 + 2).unwrap();
        assert_eq!(s.prompt.as_deref(), Some("fix this [\u{2026}]"), "kept after Stop");
        assert_eq!(s.message.as_deref(), Some("all done"));
        // New turn: message cleared, prompt replaced.
        let s = run(Some(s), "UserPromptSubmit", r#","prompt":"next""#, T0 + 3).unwrap();
        assert_eq!(s.prompt.as_deref(), Some("next"));
        assert_eq!(s.message, None);
        // Prompt-less submit keeps the previous prompt but still clears the message.
        let s = run(Some(s), "UserPromptSubmit", "", T0 + 4).unwrap();
        assert_eq!(s.prompt.as_deref(), Some("next"));
    }

    #[test]
    fn message_set_on_stop_failure_and_subagent_stop() {
        let s = run(st(AgentState::Working), "StopFailure", r#","error":"rate_limit","last_assistant_message":"API Error: 529""#, T0 + 1).unwrap();
        assert_eq!(s.message.as_deref(), Some("API Error: 529"));
        assert_eq!(s.detail.as_deref(), Some("rate_limit"));
        let w = run(None, "UserPromptSubmit", r#","prompt":"p""#, T0);
        let s = run(w, "SubagentStop", r#","last_assistant_message":"sub done""#, T0 + 1).unwrap();
        assert_eq!(s.message.as_deref(), Some("sub done"));
        assert_eq!(s.state, AgentState::Working);
        // Messages stay after notification / tool events.
        let s = run(Some(s), "Notification", r#","message":"n""#, T0 + 2).unwrap();
        assert_eq!(s.message.as_deref(), Some("sub done"));
        // Stop without a message does not invent one.
        let s = run(run(None, "UserPromptSubmit", "", T0), "Stop", "", T0 + 1).unwrap();
        assert_eq!(s.message, None);
    }

    #[test]
    fn tool_detail_for_bash_and_file_tools() {
        let s = run(None, "PreToolUse", r#","tool_name":"Bash","tool_input":{"command":"cargo  test\n  -p x"}"#, T0).unwrap();
        assert_eq!(s.detail.as_deref(), Some("Bash: cargo test -p x"));
        for (tool, key) in [("Edit", "file_path"), ("Write", "file_path"), ("Read", "file_path")] {
            let extra = format!(r#","tool_name":"{tool}","tool_input":{{"{key}":"C:\\dev\\p\\src\\main.rs"}}"#);
            let s = run(None, "PreToolUse", &extra, T0).unwrap();
            assert_eq!(s.detail, Some(format!("{tool}: main.rs")));
        }
        let s = run(None, "PreToolUse", r#","tool_name":"Edit","tool_input":{"file_path":"/a/b/c.txt"}"#, T0).unwrap();
        assert_eq!(s.detail.as_deref(), Some("Edit: c.txt"));
        // Other tools and missing input fall back to the tool name.
        let s = run(None, "PreToolUse", r#","tool_name":"Grep","tool_input":{"pattern":"x"}"#, T0).unwrap();
        assert_eq!(s.detail.as_deref(), Some("Grep"));
        let s = run(None, "PreToolUse", r#","tool_name":"Bash","tool_input":{}"#, T0).unwrap();
        assert_eq!(s.detail.as_deref(), Some("Bash"));
    }

    #[test]
    fn long_bash_command_is_clipped() {
        let cmd = "a".repeat(500);
        let extra = format!(r#","tool_name":"Bash","tool_input":{{"command":{}}}"#, json_str(&cmd));
        let s = run(None, "PreToolUse", &extra, T0).unwrap();
        assert_eq!(s.detail.unwrap().chars().count(), MAX_DETAIL_CHARS);
    }

    #[test]
    fn root_hook_session_ids_are_sticky_and_end_with_the_session() {
        for (agent, root) in [("claude", "claude1234"), ("codex", "codex1234")] {
            let apply = |prev, event: &str, extra: &str| {
                let json = format!(r#"{{"hook_event_name":"{event}"{extra}}}"#);
                match agent {
                    "claude" => super::apply(prev, AgentInput::Claude(&parse_claude_payload(&json).unwrap()), T0),
                    _ => super::apply(prev, AgentInput::Codex(&parse_codex_payload(&json).unwrap()), T0),
                }
            };
            let root_status = apply(None, "SessionStart", &format!(r#","session_id":"{root}""#));
            assert_eq!(root_status.as_ref().unwrap().session_id.as_deref(), Some(root));
            let working = apply(root_status, "UserPromptSubmit", "");
            assert_eq!(working.as_ref().unwrap().session_id.as_deref(), Some(root));
            let child = apply(working, "PreToolUse", r#","session_id":"other1234","agent_id":"child1234""#);
            assert_eq!(child.as_ref().unwrap().session_id.as_deref(), Some(root));
            assert_eq!(apply(child, "SessionEnd", ""), None);
        }
    }

    #[test]
    fn parse_new_fields() {
        let h = hook(&payload(
            "Stop",
            r#","model":"m1","to_model":"m2","session_title":" t ","prompt":" p ","last_assistant_message":" a  b ""#,
        ));
        assert_eq!(h.model.as_deref(), Some("m1"));
        assert_eq!(h.to_model.as_deref(), Some("m2"));
        assert_eq!(h.session_title.as_deref(), Some("t"));
        assert_eq!(h.prompt.as_deref(), Some("p"));
        assert_eq!(h.last_assistant_message.as_deref(), Some("a b"));
        assert!(h.is_known());
        let bare = hook(&payload("Stop", ""));
        assert_eq!(
            (bare.model, bare.to_model, bare.session_title, bare.prompt, bare.last_assistant_message),
            (None, None, None, None, None)
        );
        assert!(hook(&payload("PostModelSwitch", "")).is_known());
        assert!(hook(&payload("SubagentStop", "")).is_known());
    }

    fn state_of(s: &Option<AgentStatus>) -> AgentState {
        s.as_ref().expect("status").state
    }

    #[test]
    fn parse_extracts_fields() {
        let h = hook(&payload(
            "Notification",
            r#","message":"Claude needs your permission to use Bash","notification_type":"permission_prompt""#,
        ));
        assert_eq!(h.event, "Notification");
        assert_eq!(h.message.as_deref(), Some("Claude needs your permission to use Bash"));
        assert_eq!(h.notification_type.as_deref(), Some("permission_prompt"));
        assert!(h.is_known());
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(parse_claude_payload("").is_none());
        assert!(parse_claude_payload("not json").is_none());
        assert!(parse_claude_payload("[1,2]").is_none());
        assert!(parse_claude_payload(r#"{"foo":1}"#).is_none());
        assert!(parse_claude_payload(r#"{"hook_event_name":5}"#).is_none());
    }

    #[test]
    fn session_start_is_idle_claude() {
        let s = run(None, "SessionStart", r#","source":"startup","model":"claude-sonnet""#, T0);
        let s = s.unwrap();
        assert_eq!(s.kind, AgentKind::Claude);
        assert_eq!(s.state, AgentState::Idle);
        assert!(!s.unread);
        assert_eq!(s.since_unix_ms, T0);
    }

    #[test]
    fn working_events() {
        for (ev, extra) in [
            ("UserPromptSubmit", r#","prompt":"fix the bug""#),
            ("PreToolUse", r#","tool_name":"Bash","tool_input":{"command":"ls"},"tool_use_id":"toolu_01""#),
            ("PostToolUse", r#","tool_name":"Read","tool_input":{},"tool_response":{},"tool_use_id":"toolu_02""#),
            ("PostToolUseFailure", r#","tool_name":"Bash","error":"exit 1","tool_use_id":"toolu_03""#),
            ("SubagentStart", r#","agent_id":"a1","agent_type":"Explore""#),
        ] {
            let s = run(st(AgentState::Idle), ev, extra, T0 + 5_000);
            assert_eq!(state_of(&s), AgentState::Working, "{ev}");
            assert!(!s.unwrap().unread);
        }
    }

    #[test]
    fn pre_tool_use_detail_is_tool_name() {
        let s = run(None, "PreToolUse", r#","tool_name":"Edit""#, T0).unwrap();
        assert_eq!(s.detail.as_deref(), Some("Edit"));
    }

    #[test]
    fn waiting_events() {
        for (ev, extra) in [
            ("PermissionRequest", r#","tool_name":"Bash","tool_input":{"command":"rm x"}"#),
            ("Notification", r#","message":"permission","notification_type":"permission_prompt""#),
            ("Notification", r#","message":"x","notification_type":"something_new""#),
            ("Elicitation", r#","mcp_server_name":"srv","message":"Pick one""#),
        ] {
            let s = run(st(AgentState::Working), ev, extra, T0 + 10);
            assert_eq!(state_of(&s), AgentState::Waiting, "{ev} {extra}");
        }
    }

    #[test]
    fn idle_prompt_keeps_previous_state() {
        let idle = r#","message":"Claude is waiting for your input","notification_type":"idle_prompt""#;
        let done = run(st(AgentState::Working), "Stop", "", T0).unwrap();
        let after = run(Some(done.clone()), "Notification", idle, T0 + 60_000).unwrap();
        assert_eq!(after.state, AgentState::Done);
        assert!(after.unread);
        assert!(run(None, "Notification", idle, T0).is_none());
    }

    #[test]
    fn notification_detail_is_message() {
        let s = run(None, "Notification", r#","message":"Claude needs your permission""#, T0).unwrap();
        assert_eq!(s.detail.as_deref(), Some("Claude needs your permission"));
    }

    #[test]
    fn detail_is_clipped() {
        let long = "m".repeat(500);
        let s = run(None, "Notification", &format!(r#","message":"{long}""#), T0).unwrap();
        assert_eq!(s.detail.unwrap().chars().count(), MAX_DETAIL_CHARS);
    }

    #[test]
    fn ask_user_question_pre_tool_use_is_waiting() {
        let s = run(
            st(AgentState::Working),
            "PreToolUse",
            r#","tool_name":"AskUserQuestion","tool_input":{"questions":[]}"#,
            T0 + 10,
        );
        assert_eq!(state_of(&s), AgentState::Waiting);
        // Other tools stay Working.
        let s = run(st(AgentState::Working), "PreToolUse", r#","tool_name":"Askanything""#, T0 + 10);
        assert_eq!(state_of(&s), AgentState::Working);
    }

    #[test]
    fn stop_is_done_unread() {
        let s = run(st(AgentState::Working), "Stop", r#","stop_hook_active":false"#, T0 + 10).unwrap();
        assert_eq!(s.state, AgentState::Done);
        assert!(s.unread);
        assert_eq!(s.since_unix_ms, T0 + 10);
    }

    #[test]
    fn stop_failure_is_failed_unread() {
        let s = run(st(AgentState::Working), "StopFailure", r#","error":"rate_limit""#, T0 + 10).unwrap();
        assert_eq!(s.state, AgentState::Failed);
        assert!(s.unread);
        assert_eq!(s.detail.as_deref(), Some("rate_limit"));
    }

    #[test]
    fn session_end_clears() {
        assert_eq!(run(st(AgentState::Done), "SessionEnd", r#","reason":"exit""#, T0), None);
        assert_eq!(run(None, "SessionEnd", "", T0), None);
    }

    #[test]
    fn unknown_events_are_ignored() {
        let prev = st(AgentState::Working);
        for ev in ["SubagentStop", "PreCompact", "PostCompact", "TeammateIdle", "Whatever"] {
            assert_eq!(run(prev.clone(), ev, "", T0 + 5), prev, "{ev}");
        }
        assert_eq!(run(None, "SubagentStop", "", T0), None);
    }

    #[test]
    fn same_state_keeps_since() {
        let first = run(None, "PreToolUse", r#","tool_name":"Bash""#, T0);
        let second = run(first.clone(), "PostToolUse", r#","tool_name":"Bash""#, T0 + 500);
        assert_eq!(second.as_ref().unwrap().since_unix_ms, T0);
        assert_eq!(state_of(&second), AgentState::Working);
    }

    #[test]
    fn stray_post_tool_use_after_stop_is_ignored() {
        let done = run(st(AgentState::Working), "Stop", "", T0 + 100);
        let after = run(done.clone(), "PostToolUse", r#","tool_name":"Bash""#, T0 + 600);
        assert_eq!(after, done);
        let after = run(done.clone(), "PostToolUseFailure", r#","tool_name":"Bash""#, T0 + 1099);
        assert_eq!(after, done);
    }

    #[test]
    fn post_tool_use_after_stop_after_1s_reopens() {
        let done = run(st(AgentState::Working), "Stop", "", T0 + 100);
        let after = run(done, "PostToolUse", r#","tool_name":"Bash""#, T0 + 1100);
        assert_eq!(state_of(&after), AgentState::Working);
    }

    #[test]
    fn stray_rule_applies_only_to_post_tool_use() {
        let done = run(st(AgentState::Working), "Stop", "", T0 + 100);
        let after = run(done, "UserPromptSubmit", "", T0 + 200);
        assert_eq!(state_of(&after), AgentState::Working);
    }

    #[test]
    fn mark_seen_done_stays_done_but_read() {
        let done = run(st(AgentState::Working), "Stop", "", T0 + 100);
        let s = apply(done, AgentInput::MarkSeen, T0 + 900).unwrap();
        assert_eq!(s.state, AgentState::Done);
        assert!(!s.unread);
        assert_eq!(s.since_unix_ms, T0 + 100, "keeps the finish time");
    }

    #[test]
    fn mark_seen_failed_stays_failed_but_read() {
        let failed = run(st(AgentState::Working), "StopFailure", "", T0 + 100);
        let s = apply(failed, AgentInput::MarkSeen, T0 + 900).unwrap();
        assert_eq!(s.state, AgentState::Failed);
        assert!(!s.unread);
        assert_eq!(s.since_unix_ms, T0 + 100);
    }

    #[test]
    fn mark_seen_leaves_working_and_waiting() {
        for state in [AgentState::Working, AgentState::Waiting, AgentState::Idle] {
            let prev = st(state);
            assert_eq!(apply(prev.clone(), AgentInput::MarkSeen, T0 + 1), prev);
        }
        assert_eq!(apply(None, AgentInput::MarkSeen, T0), None);
    }

    #[test]
    fn process_exit_clears() {
        assert_eq!(apply(st(AgentState::Working), AgentInput::ProcessExit, T0), None);
        assert_eq!(apply(None, AgentInput::ProcessExit, T0), None);
    }

    fn title(prev: Option<AgentStatus>, t: &str, hooks_seen: bool, now: u64) -> Option<AgentStatus> {
        apply(prev, AgentInput::Title { title: t, hooks_seen }, now)
    }

    #[test]
    fn claude_title_glyphs() {
        for t in ["\u{2802} Fix bug", "\u{28ff} x", "\u{25d0} x", "\u{25d1} x", "\u{25d2} x", "\u{25d3} x"] {
            let s = title(None, t, false, T0).unwrap();
            assert_eq!((s.kind, s.state), (AgentKind::Claude, AgentState::Working), "{t}");
        }
        let s = title(None, "\u{2733} Claude Code", false, T0).unwrap();
        assert_eq!((s.kind, s.state, s.unread), (AgentKind::Claude, AgentState::Idle, false));
    }

    #[test]
    fn codex_spinner_titles_only_identify_generic_activity() {
        let mut tracker = AgentTracker::default();
        for frame in ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'] {
            tracker.on_title(&format!("{frame} sich"), T0);
            let s = tracker.status().unwrap();
            assert_eq!((s.kind, s.state), (AgentKind::Other, AgentState::Working), "{frame}");
        }
        assert!(tracker.on_title("sich", T0 + 1));
        assert!(tracker.status().is_none());

        assert!(tracker.on_title("⠋ Fix bug | sich", T0 + 2));
        assert_eq!(tracker.status().unwrap().kind, AgentKind::Other);
        assert!(tracker.on_title("Fix bug | sich", T0 + 3));
        assert!(tracker.status().is_none());

        // Claude's own spinner and idle marker still work with the same project title.
        assert!(tracker.on_title("⠂ sich", T0 + 4));
        let s = tracker.status().unwrap();
        assert_eq!((s.kind, s.state), (AgentKind::Claude, AgentState::Working));
        assert!(tracker.on_title("✳ sich", T0 + 5));
        let s = tracker.status().unwrap();
        assert_eq!((s.kind, s.state, s.unread), (AgentKind::Claude, AgentState::Done, true));
    }

    #[test]
    fn codex_hook_replaces_ambiguous_spinner_and_keeps_authority() {
        let mut tracker = AgentTracker::default();
        assert!(tracker.on_title("⠋ sich", T0));
        assert_eq!(tracker.status().unwrap().kind, AgentKind::Other);
        assert!(tracker.on_codex_hook(&chook("SessionStart", ""), T0 + 1));
        assert!(!tracker.on_title("⠙ sich", T0 + 2));
        assert!(!tracker.on_title("sich", T0 + 3));
        let s = tracker.status().unwrap();
        assert_eq!((s.kind, s.state), (AgentKind::Codex, AgentState::Idle));
    }

    #[test]
    fn claude_title_working_to_idle_is_done_unread() {
        let w = title(None, "\u{2802} x", false, T0);
        let d = title(w, "\u{2733} x", false, T0 + 50).unwrap();
        assert_eq!(d.state, AgentState::Done);
        assert!(d.unread);
        // Another idle glyph keeps Done + unread.
        let d2 = title(Some(d.clone()), "\u{2733} y", false, T0 + 80).unwrap();
        assert_eq!(d2, d);
    }

    #[test]
    fn spinner_frames_do_not_churn() {
        let a = title(None, "\u{2802} x", false, T0);
        let b = title(a.clone(), "\u{2810} x", false, T0 + 100);
        assert_eq!(a, b);
    }

    #[test]
    fn gemini_title_glyphs() {
        for t in ["\u{2726} Gemini CLI", "\u{23f2} Gemini CLI"] {
            let s = title(None, t, false, T0).unwrap();
            assert_eq!((s.kind, s.state), (AgentKind::Gemini, AgentState::Working), "{t}");
        }
        let s = title(None, "\u{270b} Gemini CLI", false, T0).unwrap();
        assert_eq!(s.state, AgentState::Waiting);
        let idle = title(None, "\u{25c7} Gemini CLI", false, T0).unwrap();
        assert_eq!(idle.state, AgentState::Idle);
        let w = title(None, "\u{2726} Gemini CLI", false, T0);
        let d = title(w, "\u{25c7} Gemini CLI", false, T0 + 9).unwrap();
        assert_eq!((d.state, d.unread), (AgentState::Done, true));
    }

    #[test]
    fn plain_title_after_idle_means_agent_exited() {
        let idle = title(None, "\u{2733} x", false, T0);
        assert_eq!(title(idle, "pwsh", false, T0 + 5), None);
        let done = title(title(None, "\u{2802} x", false, T0), "\u{2733} x", false, T0 + 1);
        assert_eq!(title(done, "pwsh", false, T0 + 5), None);
    }

    #[test]
    fn plain_title_while_working_is_transient() {
        let w = title(None, "\u{2802} x", false, T0);
        assert_eq!(title(w.clone(), "some title", false, T0 + 5), w);
        assert_eq!(title(None, "pwsh", false, T0), None);
    }

    #[test]
    fn hooks_take_precedence_over_title() {
        let prev = st(AgentState::Working);
        // Title says idle, but hooks were seen: ignored.
        assert_eq!(title(prev.clone(), "\u{2733} x", true, T0 + 5), prev);
        assert_eq!(title(None, "\u{2802} x", true, T0), None);
    }

    #[test]
    fn tracker_hooks_disable_title_fallback() {
        let mut t = AgentTracker::default();
        assert!(t.on_title("\u{2802} x", T0));
        assert_eq!(t.status().unwrap().state, AgentState::Working);
        // A hook arrives: from now on titles are ignored.
        let h = hook(&payload("Notification", r#","message":"hi""#));
        assert!(t.on_claude_hook(&h, T0 + 10));
        assert_eq!(t.status().unwrap().state, AgentState::Waiting);
        assert!(!t.on_title("\u{2733} x", T0 + 20));
        assert_eq!(t.status().unwrap().state, AgentState::Waiting);
    }

    #[test]
    fn tracker_session_end_reenables_title_fallback() {
        let mut t = AgentTracker::default();
        assert!(t.on_claude_hook(&hook(&payload("SessionStart", "")), T0));
        assert!(t.on_claude_hook(&hook(&payload("SessionEnd", "")), T0 + 1));
        assert!(t.status().is_none());
        assert!(t.on_title("\u{2802} x", T0 + 2));
        assert_eq!(t.status().unwrap().state, AgentState::Working);
    }

    #[test]
    fn tracker_exit_resets_hooks_seen() {
        let mut t = AgentTracker::default();
        t.on_claude_hook(&hook(&payload("UserPromptSubmit", "")), T0);
        assert!(t.on_exit(T0));
        assert!(!t.on_exit(T0));
        assert!(t.on_title("\u{2802} x", T0 + 1));
    }

    #[test]
    fn tracker_reports_changes_only() {
        let mut t = AgentTracker::default();
        let h = hook(&payload("UserPromptSubmit", ""));
        assert!(t.on_claude_hook(&h, T0));
        assert!(!t.on_claude_hook(&h, T0 + 5), "same state again is not a change");
        let unknown = hook(&payload("PreCompact", ""));
        assert!(!t.on_claude_hook(&unknown, T0 + 6));
    }

    #[test]
    fn tracker_full_flow() {
        let mut t = AgentTracker::default();
        assert!(t.on_claude_hook(&hook(&payload("UserPromptSubmit", "")), T0));
        assert_eq!(t.status().unwrap().state, AgentState::Working);
        assert!(t.on_claude_hook(&hook(&payload("PermissionRequest", r#","tool_name":"Bash""#)), T0 + 1));
        assert_eq!(t.status().unwrap().state, AgentState::Waiting);
        assert!(t.on_claude_hook(&hook(&payload("Stop", "")), T0 + 2));
        let s = t.status().unwrap();
        assert_eq!((s.state, s.unread), (AgentState::Done, true));
        assert!(t.mark_seen(T0 + 3));
        let s = t.status().unwrap();
        assert_eq!((s.state, s.unread), (AgentState::Done, false));
        assert!(!t.mark_seen(T0 + 4));
    }

    #[test]
    fn powershell_and_generic_command_tools_are_labelled() {
        let s = run(None, "PreToolUse", r#","tool_name":"PowerShell","tool_input":{"command":"Get-ChildItem  -Recurse\n"}"#, T0).unwrap();
        assert_eq!(s.detail.as_deref(), Some("PowerShell: Get-ChildItem -Recurse"));
        let s = run(None, "PreToolUse", r#","tool_name":"Shell","tool_input":{"command":"ls"}"#, T0).unwrap();
        assert_eq!(s.detail.as_deref(), Some("Shell: ls"));
        // No command -> tool name.
        let s = run(None, "PreToolUse", r#","tool_name":"PowerShell","tool_input":{}"#, T0).unwrap();
        assert_eq!(s.detail.as_deref(), Some("PowerShell"));
        // A non-string command is not a label.
        let s = run(None, "PreToolUse", r#","tool_name":"Foo","tool_input":{"command":5}"#, T0).unwrap();
        assert_eq!(s.detail.as_deref(), Some("Foo"));
    }

    // ---- Codex ----

    fn chook(event: &str, extra: &str) -> CodexHook {
        parse_codex_payload(&format!(
            r#"{{"session_id":"s1","transcript_path":"C:\\x\\r.jsonl","cwd":"C:\\dev\\p","hook_event_name":"{event}"{extra}}}"#
        ))
        .expect("valid codex payload")
    }

    fn crun(prev: Option<AgentStatus>, event: &str, extra: &str, now: u64) -> Option<AgentStatus> {
        apply(prev, AgentInput::Codex(&chook(event, extra)), now)
    }

    #[test]
    fn codex_session_start_is_idle_with_model() {
        let s = crun(None, "SessionStart", r#","source":"startup","model":"gpt-5-codex""#, T0).unwrap();
        assert_eq!((s.kind, s.state, s.unread), (AgentKind::Codex, AgentState::Idle, false));
        assert_eq!(s.model.as_deref(), Some("gpt-5-codex"));
    }

    #[test]
    fn codex_prompt_working_clears_message() {
        let prev = crun(crun(None, "UserPromptSubmit", r#","prompt":"one""#, T0), "Stop", r#","last_assistant_message":"done  it""#, T0 + 5);
        assert_eq!(prev.as_ref().unwrap().message.as_deref(), Some("done it"));
        let s = crun(prev, "UserPromptSubmit", r#","prompt":"two\nlines""#, T0 + 10).unwrap();
        assert_eq!(s.state, AgentState::Working);
        assert_eq!(s.prompt.as_deref(), Some("two lines"));
        assert_eq!(s.message, None);
    }

    #[test]
    fn codex_tool_events_are_working_with_label() {
        for ev in ["PreToolUse", "PostToolUse"] {
            let s = crun(None, ev, r#","tool_name":"Bash","tool_input":{"command":"cargo  test"}"#, T0).unwrap();
            assert_eq!(s.state, AgentState::Working, "{ev}");
            assert_eq!(s.detail.as_deref(), Some("Bash: cargo test"));
        }
        let s = crun(None, "PreToolUse", r#","tool_name":"apply_patch","tool_input":{"input":"x"}"#, T0).unwrap();
        assert_eq!(s.detail.as_deref(), Some("apply_patch"));
    }

    #[test]
    fn codex_array_command_is_joined() {
        let s = crun(None, "PreToolUse", r#","tool_name":"shell","tool_input":{"command":["bash","-lc","ls -la"]}"#, T0).unwrap();
        assert_eq!(s.detail.as_deref(), Some("shell: bash -lc ls -la"));
        let s = crun(None, "PreToolUse", r#","tool_name":"shell","tool_input":{"command":[]}"#, T0).unwrap();
        assert_eq!(s.detail.as_deref(), Some("shell"));
    }

    #[test]
    fn codex_permission_request_is_waiting() {
        let s = crun(None, "PermissionRequest", r#","tool_name":"Bash","tool_input":{"command":"rm x"}"#, T0).unwrap();
        assert_eq!(s.state, AgentState::Waiting);
        assert_eq!(s.detail.as_deref(), Some("Bash: rm x"));
        let s = crun(None, "PermissionRequest", r#","tool_name":"Bash","tool_input":{"command":"rm x","description":"Delete  x"}"#, T0).unwrap();
        assert_eq!(s.detail.as_deref(), Some("Delete x"));
    }

    #[test]
    fn codex_stop_is_done_unread_with_message() {
        let s = crun(None, "UserPromptSubmit", "", T0);
        let s = crun(s, "Stop", r#","last_assistant_message":"all\ndone""#, T0 + 3).unwrap();
        assert_eq!((s.state, s.unread), (AgentState::Done, true));
        assert_eq!(s.message.as_deref(), Some("all done"));
    }

    #[test]
    fn codex_interrupt_is_idle_read_and_keeps_prompt_and_model() {
        let s = crun(None, "SessionStart", r#","model":"m1""#, T0);
        let s = crun(s, "UserPromptSubmit", r#","prompt":"p""#, T0 + 1);
        let s = crun(s, "Interrupt", r#","turn_id":"t1""#, T0 + 2).unwrap();
        assert_eq!((s.state, s.unread), (AgentState::Idle, false));
        assert_eq!(s.prompt.as_deref(), Some("p"));
        assert_eq!(s.model.as_deref(), Some("m1"));
    }

    #[test]
    fn codex_session_end_clears_and_unknown_is_ignored() {
        assert_eq!(crun(crun(None, "SessionStart", "", T0), "SessionEnd", "", T0 + 1), None);
        let prev = crun(None, "UserPromptSubmit", "", T0);
        assert_eq!(crun(prev.clone(), "PreCompact", "", T0 + 1), prev);
        assert!(!chook("PreCompact", "").is_known());
        assert!(chook("Interrupt", "").is_known());
    }

    #[test]
    fn codex_model_is_sticky_and_updated_on_every_event() {
        let s = crun(None, "SessionStart", r#","model":"m1""#, T0);
        let s = crun(s, "UserPromptSubmit", r#","prompt":"p""#, T0 + 1);
        assert_eq!(s.as_ref().unwrap().model.as_deref(), Some("m1"));
        let s = crun(s, "PreToolUse", r#","model":"m2","tool_name":"Bash""#, T0 + 2).unwrap();
        assert_eq!(s.model.as_deref(), Some("m2"));
        let s = crun(Some(s), "Stop", "", T0 + 3).unwrap();
        assert_eq!(s.model.as_deref(), Some("m2"));
    }

    #[test]
    fn codex_stray_post_tool_use_after_stop_is_ignored() {
        let done = crun(crun(None, "UserPromptSubmit", "", T0), "Stop", "", T0 + 100);
        assert_eq!(crun(done.clone(), "PostToolUse", r#","tool_name":"Bash""#, T0 + 500), done);
        assert_eq!(crun(done, "PostToolUse", r#","tool_name":"Bash""#, T0 + 1200).unwrap().state, AgentState::Working);
    }

    #[test]
    fn codex_parse_rejects_garbage_and_tracker_works() {
        assert!(parse_codex_payload("nope").is_none());
        assert!(parse_codex_payload(r#"{"x":1}"#).is_none());
        let mut t = AgentTracker::default();
        assert!(t.on_codex_hook(&chook("UserPromptSubmit", ""), T0));
        assert!(!t.on_codex_hook(&chook("Whatever", ""), T0 + 1));
        // Hooks seen: titles ignored; SessionEnd re-enables them.
        assert!(!t.on_title("\u{2733} x", T0 + 2));
        assert!(t.on_codex_hook(&chook("SessionEnd", ""), T0 + 3));
        assert!(t.status().is_none());
    }

    // ---- Codex subagents ----

    fn cchild(event: &str, id: &str, extra: &str) -> CodexHook {
        chook(event, &format!(r#","agent_id":"{id}"{extra}"#))
    }

    fn crun_h(prev: Option<AgentStatus>, h: &CodexHook, now: u64) -> Option<AgentStatus> {
        apply(prev, AgentInput::Codex(h), now)
    }

    /// A JSON-escaped absolute path string (with quotes) for a `.jsonl` file name.
    fn abs_jsonl(name: &str) -> String {
        serde_json::Value::String(std::env::temp_dir().join(name).display().to_string()).to_string()
    }

    fn working_main() -> Option<AgentStatus> {
        let s = crun(None, "SessionStart", r#","model":"m1""#, T0);
        crun(s, "UserPromptSubmit", r#","prompt":"p""#, T0 + 1)
    }

    #[test]
    fn codex_parse_agent_fields() {
        let h = chook("SubagentStart", r#","agent_id":"  a1  ","agent_type":" explorer ""#);
        assert_eq!(h.agent_id.as_deref(), Some("a1"));
        assert_eq!(h.agent_type.as_deref(), Some("explorer"));
        assert_eq!(h.transcript_path.as_deref(), Some(r"C:\x\r.jsonl"));
        assert!(h.is_known());
        assert!(chook("SubagentStop", "").is_known());
        let h = chook("PreToolUse", r#","agent_id":"   ","agent_type":"""#);
        assert_eq!((h.agent_id, h.agent_type), (None, None));
        let long = "x".repeat(100);
        let h = chook("PreToolUse", &format!(r#","agent_id":"{long}""#));
        assert_eq!(h.agent_id.as_deref().map(str::len), Some(64));
        let h = chook("PreToolUse", r#","agent_id":5"#);
        assert_eq!(h.agent_id, None);
        let h = parse_codex_payload(r#"{"hook_event_name":"Stop"}"#).unwrap();
        assert_eq!(h.transcript_path, None);
    }

    #[test]
    fn codex_child_start_adds_subagent_and_keeps_main() {
        let main = working_main();
        let s = crun_h(main.clone(), &cchild("SubagentStart", "a1", r#","agent_type":"explorer""#), T0 + 5).unwrap();
        let m = main.unwrap();
        assert_eq!((s.state, s.since_unix_ms, &s.model, &s.prompt, &s.detail), (m.state, m.since_unix_ms, &m.model, &m.prompt, &m.detail));
        assert_eq!(s.subagents.len(), 1);
        let sub = &s.subagents[0];
        assert_eq!((sub.id.as_str(), sub.state, sub.agent_type.as_deref()), ("a1", AgentState::Working, Some("explorer")));
        assert_eq!((sub.detail.clone(), sub.description.clone(), sub.since_unix_ms), (None, None, T0 + 5));
    }

    #[test]
    fn codex_child_model_is_clipped_and_sticky_until_the_child_stops() {
        let long = "é".repeat(MAX_DETAIL_CHARS + 5);
        let reported = format!(r#","model":" {long} ""#);
        let s = crun_h(working_main(), &cchild("SubagentStart", "a1", &reported), T0 + 2).unwrap();
        assert_eq!(s.subagents[0].model, Some("é".repeat(MAX_DETAIL_CHARS)));
        let s = crun_h(Some(s), &cchild("PreToolUse", "a1", r#","tool_name":"Bash","model":"  ""#), T0 + 3).unwrap();
        assert_eq!(s.subagents[0].model, Some("é".repeat(MAX_DETAIL_CHARS)));
        let s = crun_h(Some(s), &cchild("PermissionRequest", "a1", r#","model":" new-child-model ""#), T0 + 4).unwrap();
        assert_eq!(s.subagents[0].model.as_deref(), Some("new-child-model"));
        assert_eq!(s.model.as_deref(), Some("m1"));
        let s = crun_h(Some(s), &cchild("SubagentStop", "a1", ""), T0 + 5);
        let reused = crun_h(s, &cchild("SubagentStart", "a1", ""), T0 + 6).unwrap();
        assert_eq!(reused.subagents[0].model, None);
    }

    #[test]
    fn codex_child_events_without_main_status_create_nothing() {
        for ev in ["SubagentStart", "PreToolUse", "PostToolUse", "PermissionRequest", "SubagentStop", "Stop"] {
            assert_eq!(crun_h(None, &cchild(ev, "a1", r#","tool_name":"Bash""#), T0), None, "{ev}");
        }
    }

    #[test]
    fn codex_child_tool_events_are_working_with_label() {
        let main = working_main();
        for ev in ["PreToolUse", "PostToolUse"] {
            let s = crun_h(main.clone(), &cchild(ev, "a1", r#","tool_name":"Bash","tool_input":{"command":"cargo  test"}"#), T0 + 5).unwrap();
            assert_eq!(s.subagents.len(), 1, "{ev}");
            assert_eq!(s.subagents[0].state, AgentState::Working);
            assert_eq!(s.subagents[0].detail.as_deref(), Some("Bash: cargo test"));
            assert_eq!(s.detail, None, "main detail untouched");
            assert_eq!(s.state, AgentState::Working);
        }
        // The same subagent is updated, not duplicated.
        let s = crun_h(main, &cchild("PreToolUse", "a1", r#","tool_name":"Bash""#), T0 + 5);
        let s = crun_h(s, &cchild("PreToolUse", "a1", r#","tool_name":"apply_patch""#), T0 + 6).unwrap();
        assert_eq!(s.subagents.len(), 1);
        assert_eq!(s.subagents[0].detail.as_deref(), Some("apply_patch"));
    }

    #[test]
    fn codex_child_permission_request_waits_without_touching_main() {
        let main = working_main();
        let s = crun_h(main.clone(), &cchild("PermissionRequest", "a1", r#","tool_name":"Bash","tool_input":{"command":"rm x"}"#), T0 + 5).unwrap();
        assert_eq!(s.subagents[0].state, AgentState::Waiting);
        assert_eq!(s.subagents[0].detail.as_deref(), Some("Bash: rm x"));
        assert_eq!(s.state, AgentState::Working);
        let s = crun_h(Some(s), &cchild("PermissionRequest", "a1", r#","tool_name":"Bash","tool_input":{"command":"rm x","description":"Delete  x"}"#), T0 + 6).unwrap();
        assert_eq!(s.subagents[0].detail.as_deref(), Some("Delete x"));
        // Working again after the next tool event.
        let s = crun_h(Some(s), &cchild("PostToolUse", "a1", r#","tool_name":"Bash""#), T0 + 7).unwrap();
        assert_eq!(s.subagents[0].state, AgentState::Working);
    }

    #[test]
    fn codex_child_stop_removes_only_that_subagent_and_keeps_main_message() {
        let main = crun(working_main(), "PreToolUse", r#","tool_name":"Bash""#, T0 + 2);
        let s = crun_h(main, &cchild("SubagentStart", "a1", ""), T0 + 3);
        let s = crun_h(s, &cchild("SubagentStart", "a2", ""), T0 + 4);
        assert_eq!(s.as_ref().unwrap().subagents.len(), 2);
        let before = s.clone().unwrap();
        for ev in ["SubagentStop", "Stop"] {
            let after = crun_h(s.clone(), &cchild(ev, "a1", r#","last_assistant_message":"child done""#), T0 + 9).unwrap();
            assert_eq!(after.subagents.len(), 1, "{ev}");
            assert_eq!(after.subagents[0].id, "a2");
            assert_eq!(after.message, before.message, "main message unchanged");
            assert_eq!((after.state, after.since_unix_ms, &after.detail), (before.state, before.since_unix_ms, &before.detail));
        }
        // Unknown id: nothing changes.
        assert_eq!(crun_h(s.clone(), &cchild("SubagentStop", "zz", ""), T0 + 9), s);
    }

    #[test]
    fn codex_child_other_events_are_ignored() {
        let s = crun_h(working_main(), &cchild("SubagentStart", "a1", ""), T0 + 3);
        for ev in ["SessionStart", "UserPromptSubmit", "Interrupt", "SessionEnd", "PreCompact"] {
            assert_eq!(crun_h(s.clone(), &cchild(ev, "a1", r#","prompt":"x","model":"zz""#), T0 + 9), s, "{ev}");
        }
    }

    #[test]
    fn codex_root_subagent_events_without_agent_id_are_ignored() {
        let s = crun_h(working_main(), &cchild("SubagentStart", "a1", ""), T0 + 3);
        assert_eq!(crun(s.clone(), "SubagentStart", "", T0 + 4), s);
        assert_eq!(crun(s.clone(), "SubagentStop", "", T0 + 4), s);
        assert_eq!(crun(None, "SubagentStart", "", T0), None);
    }

    #[test]
    fn codex_roster_is_cleared_by_root_events() {
        let with_sub = || crun_h(working_main(), &cchild("SubagentStart", "a1", ""), T0 + 3);
        // The roster survives a new prompt and tool events of the root.
        let s = crun(with_sub(), "UserPromptSubmit", r#","prompt":"q""#, T0 + 4).unwrap();
        assert_eq!(s.subagents.len(), 1);
        let s = crun(Some(s), "PreToolUse", r#","tool_name":"Bash""#, T0 + 5).unwrap();
        assert_eq!(s.subagents.len(), 1);
        for ev in ["Stop", "Interrupt", "SessionStart"] {
            let s = crun(with_sub(), ev, "", T0 + 9).unwrap();
            assert!(s.subagents.is_empty(), "{ev}");
        }
        assert_eq!(crun(with_sub(), "SessionEnd", "", T0 + 9), None);
        // Stop with a message still records it.
        let s = crun(with_sub(), "Stop", r#","last_assistant_message":"fin""#, T0 + 9).unwrap();
        assert_eq!((s.state, s.message.as_deref()), (AgentState::Done, Some("fin")));
    }

    #[test]
    fn codex_placeholder_agent_type_is_ignored_and_roster_is_capped() {
        let s = crun_h(working_main(), &cchild("SubagentStart", "a1", r#","agent_type":"default""#), T0 + 3).unwrap();
        assert_eq!(s.subagents[0].agent_type.as_deref(), Some("default"));
        let s = crun_h(Some(s), &cchild("PreToolUse", "a1", r#","agent_type":"Task""#), T0 + 4).unwrap();
        assert_eq!(s.subagents[0].agent_type.as_deref(), Some("default"), "placeholder does not overwrite");
        let mut s = working_main();
        for i in 0..(MAX_SUBAGENTS + 4) {
            s = crun_h(s, &cchild("SubagentStart", &format!("a{i}"), ""), T0 + 3);
        }
        assert_eq!(s.unwrap().subagents.len(), MAX_SUBAGENTS);
    }

    // ---- Codex subagent identity bookkeeping ----

    fn tracker_with_child(path: &str) -> (AgentTracker, CodexHook) {
        let mut t = AgentTracker::default();
        t.on_codex_hook(&chook("UserPromptSubmit", ""), T0);
        let start = cchild("SubagentStart", "a1", &format!(r#","transcript_path":{path}"#));
        assert!(t.on_codex_hook(&start, T0 + 1));
        (t, start)
    }

    fn res(path: &str, len: u64, read: IdentityRead) -> IdentityResult {
        IdentityResult { agent_id: "a1".into(), path: serde_json::from_str::<String>(path).unwrap(), len, read }
    }

    fn tool_ev(path: &str) -> CodexHook {
        cchild("PreToolUse", "a1", &format!(r#","tool_name":"Bash","transcript_path":{path}"#))
    }

    #[test]
    fn identity_plan_requires_child_event_status_and_valid_path() {
        let p = abs_jsonl("sich-id-a.jsonl");
        let (t, start) = tracker_with_child(&p);
        let plan = t.identity_plan(&start, T0 + 2).unwrap();
        assert_eq!((plan.agent_id.as_str(), plan.pending_len), ("a1", None));
        assert_eq!(plan.path, serde_json::from_str::<String>(&p).unwrap());
        // Root events, stop events and unusable paths get no plan.
        assert_eq!(t.identity_plan(&chook("PreToolUse", &format!(r#","transcript_path":{p}"#)), T0 + 2), None);
        assert_eq!(t.identity_plan(&cchild("SubagentStop", "a1", &format!(r#","transcript_path":{p}"#)), T0 + 2), None);
        assert_eq!(t.identity_plan(&cchild("SessionStart", "a1", &format!(r#","transcript_path":{p}"#)), T0 + 2), None);
        for bad in [r#""rel/x.jsonl""#, r#""relative.jsonl""#] {
            assert_eq!(t.identity_plan(&tool_ev(bad), T0 + 2), None, "{bad}");
        }
        let not_jsonl = serde_json::Value::String(std::env::temp_dir().join("x.txt").display().to_string()).to_string();
        assert_eq!(t.identity_plan(&tool_ev(&not_jsonl), T0 + 2), None);
        let no_path = cchild("PreToolUse", "a1", "");
        let no_path = CodexHook { transcript_path: None, ..no_path };
        assert_eq!(t.identity_plan(&no_path, T0 + 2), None);
        // No main status: nothing to attach to.
        assert_eq!(AgentTracker::default().identity_plan(&start, T0), None);
    }

    #[test]
    fn identity_found_sets_description_and_is_never_read_again() {
        let p = abs_jsonl("sich-id-b.jsonl");
        let (mut t, start) = tracker_with_child(&p);
        let changed = t.on_codex_hook_with(&tool_ev(&p), T0 + 2, Some(res(&p, 50, IdentityRead::Found("explorer".into()))));
        assert!(changed);
        assert_eq!(t.status().unwrap().subagents[0].description.as_deref(), Some("explorer"));
        assert_eq!(t.identity_plan(&start, T0 + 100_000), None);
        assert_eq!(t.identity_plan(&tool_ev(&p), T0 + 100_000), None);
        // The label survives later child events.
        t.on_codex_hook(&tool_ev(&p), T0 + 3);
        assert_eq!(t.status().unwrap().subagents[0].description.as_deref(), Some("explorer"));
    }

    #[test]
    fn identity_pending_keeps_the_known_length_for_retry() {
        let p = abs_jsonl("sich-id-c.jsonl");
        let (mut t, start) = tracker_with_child(&p);
        t.on_codex_hook_with(&tool_ev(&p), T0 + 2, Some(res(&p, 10, IdentityRead::Pending)));
        assert_eq!(t.identity_plan(&start, T0 + 3).unwrap().pending_len, Some(10));
        t.on_codex_hook_with(&tool_ev(&p), T0 + 4, Some(res(&p, 25, IdentityRead::Pending)));
        assert_eq!(t.identity_plan(&start, T0 + 5).unwrap().pending_len, Some(25));
        assert_eq!(t.status().unwrap().subagents[0].description, None);
        // A different path starts over.
        let other = abs_jsonl("sich-id-c2.jsonl");
        assert_eq!(t.identity_plan(&tool_ev(&other), T0 + 5).unwrap().pending_len, None);
    }

    #[test]
    fn identity_miss_is_never_retried_for_the_same_path() {
        let p = abs_jsonl("sich-id-d.jsonl");
        let (mut t, start) = tracker_with_child(&p);
        t.on_codex_hook_with(&tool_ev(&p), T0 + 2, Some(res(&p, 30, IdentityRead::Miss)));
        assert!(t.status().unwrap().subagents[0].description.is_none());
        assert_eq!(t.identity_plan(&start, T0 + 999_999), None);
        let other = abs_jsonl("sich-id-d2.jsonl");
        assert!(t.identity_plan(&tool_ev(&other), T0 + 3).is_some());
    }

    #[test]
    fn identity_unreadable_retries_after_five_seconds() {
        let p = abs_jsonl("sich-id-e.jsonl");
        let (mut t, start) = tracker_with_child(&p);
        t.on_codex_hook_with(&tool_ev(&p), T0 + 10, Some(res(&p, 0, IdentityRead::Unreadable)));
        assert_eq!(t.identity_plan(&start, T0 + 10 + 4999), None);
        let plan = t.identity_plan(&start, T0 + 10 + 5000).unwrap();
        assert_eq!(plan.pending_len, None);
    }

    #[test]
    fn identity_bookkeeping_is_dropped_with_the_roster_entry() {
        let p = abs_jsonl("sich-id-f.jsonl");
        // Subagent stop.
        let (mut t, start) = tracker_with_child(&p);
        t.on_codex_hook_with(&tool_ev(&p), T0 + 2, Some(res(&p, 5, IdentityRead::Miss)));
        assert_eq!(t.identities.len(), 1);
        t.on_codex_hook(&cchild("SubagentStop", "a1", ""), T0 + 3);
        assert!(t.identities.is_empty());
        // A new start with the same id is read afresh.
        t.on_codex_hook(&start, T0 + 4);
        assert!(t.identity_plan(&start, T0 + 5).is_some());
        // Root Stop clears the roster and the bookkeeping.
        t.on_codex_hook_with(&tool_ev(&p), T0 + 6, Some(res(&p, 5, IdentityRead::Pending)));
        assert_eq!(t.identities.len(), 1);
        t.on_codex_hook(&chook("Stop", ""), T0 + 7);
        assert!(t.identities.is_empty());
        // Process exit too.
        t.on_codex_hook(&start, T0 + 8);
        t.on_codex_hook_with(&tool_ev(&p), T0 + 9, Some(res(&p, 5, IdentityRead::Miss)));
        t.on_exit(T0 + 10);
        assert!(t.identities.is_empty());
    }

    #[test]
    fn identity_result_for_unknown_subagent_is_ignored() {
        let p = abs_jsonl("sich-id-g.jsonl");
        let (mut t, _) = tracker_with_child(&p);
        let mut r = res(&p, 5, IdentityRead::Found("ghost".into()));
        r.agent_id = "nobody".into();
        assert!(!t.on_codex_hook_with(&chook("UserPromptSubmit", ""), T0 + 2, Some(r)));
        assert!(t.identities.is_empty());
        assert_eq!(t.status().unwrap().subagents[0].description, None);
    }

    // ---- Subagent model reads (Claude meta file, Codex rollout) ----

    fn claude_child(event: &str, id: &str, path: &str) -> ClaudeHook {
        hook(&format!(
            r#"{{"session_id":"s1","hook_event_name":"{event}","agent_id":"{id}","transcript_path":{path}}}"#
        ))
    }

    fn model_res(source: ModelSource, id: &str, session: Option<&str>, model: Option<&str>) -> ModelResult {
        ModelResult {
            agent_id: id.into(),
            session_id: session.map(str::to_string),
            source,
            model: model.map(str::to_string),
        }
    }

    fn claude_tracker() -> AgentTracker {
        let mut t = AgentTracker::default();
        t.on_claude_hook(&hook(r#"{"session_id":"s1","hook_event_name":"SessionStart","model":"main"}"#), T0);
        t
    }

    fn sub_model(t: &AgentTracker, id: &str) -> Option<String> {
        t.status().unwrap().subagents.iter().find(|s| s.id == id).and_then(|s| s.model.clone())
    }

    #[test]
    fn claude_model_comes_from_meta_after_subagent_start() {
        let p = abs_jsonl("sich-cm-a/sess.jsonl");
        let mut t = claude_tracker();
        let start = claude_child("SubagentStart", "a1", &p);
        let plan = t.claude_model_plan(&start, T0 + 1).unwrap();
        assert_eq!((plan.agent_id.as_str(), plan.source, plan.session_id.as_deref()), ("a1", ModelSource::ClaudeMeta, Some("s1")));
        assert!(plan.path.ends_with(std::path::Path::new("sess").join("subagents").join("agent-a1.meta.json")));
        // The hook is applied first (it puts the subagent on the roster), then the result.
        assert!(t.on_claude_hook(&start, T0 + 1));
        let r = model_res(ModelSource::ClaudeMeta, "a1", Some("s1"), Some(" claude-haiku-5-5 "));
        assert!(t.apply_model(r, T0 + 1));
        assert_eq!(sub_model(&t, "a1").as_deref(), Some("claude-haiku-5-5"));
        // Done: no further reads, and a later result never overwrites it.
        let tool = claude_child("PreToolUse", "a1", &p);
        assert_eq!(t.claude_model_plan(&tool, T0 + 100_000), None);
        assert!(!t.apply_model(model_res(ModelSource::ClaudeMeta, "a1", Some("s1"), Some("other")), T0 + 5));
        assert_eq!(sub_model(&t, "a1").as_deref(), Some("claude-haiku-5-5"));
    }

    #[test]
    fn claude_hook_model_is_not_overwritten_by_the_file() {
        let p = abs_jsonl("sich-cm-b/sess.jsonl");
        let mut t = claude_tracker();
        t.on_claude_hook(&claude_child("SubagentStart", "a1", &p), T0 + 1);
        let sw = hook(r#"{"session_id":"s1","hook_event_name":"PostModelSwitch","agent_id":"a1","to_model":"from-hook"}"#);
        t.on_claude_hook(&sw, T0 + 2);
        assert_eq!(sub_model(&t, "a1").as_deref(), Some("from-hook"));
        assert_eq!(t.claude_model_plan(&claude_child("PreToolUse", "a1", &p), T0 + 9), None);
        assert!(!t.apply_model(model_res(ModelSource::ClaudeMeta, "a1", Some("s1"), Some("from-file")), T0 + 9));
        assert_eq!(sub_model(&t, "a1").as_deref(), Some("from-hook"));
    }

    #[test]
    fn claude_model_probe_is_rate_and_count_limited() {
        let p = abs_jsonl("sich-cm-c/sess.jsonl");
        let mut t = claude_tracker();
        let start = claude_child("SubagentStart", "a1", &p);
        t.on_claude_hook(&start, T0);
        let tool = claude_child("PreToolUse", "a1", &p);
        let miss = || model_res(ModelSource::ClaudeMeta, "a1", Some("s1"), None);
        let mut now = T0;
        for _ in 0..MODEL_PROBE_MAX_TRIES {
            assert!(t.claude_model_plan(&tool, now).is_some());
            assert!(!t.apply_model(miss(), now));
            assert_eq!(t.claude_model_plan(&tool, now + MODEL_PROBE_INTERVAL_MS - 1), None, "too soon");
            now += MODEL_PROBE_INTERVAL_MS;
        }
        assert_eq!(t.claude_model_plan(&tool, now + 1_000_000), None, "gave up");
        assert_eq!(sub_model(&t, "a1"), None);
    }

    #[test]
    fn claude_model_plan_needs_a_child_event_a_valid_id_and_path() {
        let p = abs_jsonl("sich-cm-d/sess.jsonl");
        let t = claude_tracker();
        assert!(t.claude_model_plan(&claude_child("PostToolUse", "a1", &p), T0).is_some());
        // Main-level events, stop events, hook agents, unusable ids/paths: nothing to read.
        assert_eq!(t.claude_model_plan(&hook(&payload("PreToolUse", "")), T0), None);
        assert_eq!(t.claude_model_plan(&claude_child("SubagentStop", "a1", &p), T0), None);
        assert_eq!(t.claude_model_plan(&claude_child("PreToolUse", "hook-agent-1", &p), T0), None);
        assert_eq!(t.claude_model_plan(&claude_child("PreToolUse", "a/../b", &p), T0), None);
        assert_eq!(t.claude_model_plan(&claude_child("PreToolUse", "a1", r#""rel/x.jsonl""#), T0), None);
        assert_eq!(t.claude_model_plan(&claude_child("PreToolUse", "a1", &abs_jsonl("x.txt")), T0), None);
        let no_path = hook(r#"{"session_id":"s1","hook_event_name":"PreToolUse","agent_id":"a1"}"#);
        assert_eq!(t.claude_model_plan(&no_path, T0), None);
    }

    #[test]
    fn model_result_for_another_session_or_subagent_is_ignored() {
        let p = abs_jsonl("sich-cm-e/sess.jsonl");
        let mut t = claude_tracker();
        t.on_claude_hook(&claude_child("SubagentStart", "a1", &p), T0);
        assert_eq!(t.status().unwrap().session_id.as_deref(), Some("s1"));
        assert!(!t.apply_model(model_res(ModelSource::ClaudeMeta, "a1", Some("other-session"), Some("m")), T0 + 1));
        assert!(!t.apply_model(model_res(ModelSource::ClaudeMeta, "ghost", Some("s1"), Some("m")), T0 + 1));
        assert!(!t.apply_model(model_res(ModelSource::CodexRollout, "a1", Some("s1"), Some("m")), T0 + 1));
        assert_eq!(sub_model(&t, "a1"), None);
        assert!(t.model_probes.is_empty(), "ignored results leave no bookkeeping");
        // Empty / blank models count as a miss.
        assert!(!t.apply_model(model_res(ModelSource::ClaudeMeta, "a1", Some("s1"), Some("  ")), T0 + 2));
        assert_eq!(sub_model(&t, "a1"), None);
    }

    #[test]
    fn model_probe_bookkeeping_is_dropped_with_the_roster_entry() {
        let p = abs_jsonl("sich-cm-f/sess.jsonl");
        let mut t = claude_tracker();
        t.on_claude_hook(&claude_child("SubagentStart", "a1", &p), T0);
        t.apply_model(model_res(ModelSource::ClaudeMeta, "a1", Some("s1"), None), T0);
        assert_eq!(t.model_probes.len(), 1);
        t.on_claude_hook(&claude_child("SubagentStop", "a1", &p), T0 + 1);
        assert!(t.model_probes.is_empty());
        t.on_claude_hook(&claude_child("SubagentStart", "a1", &p), T0 + 2);
        assert!(t.claude_model_plan(&claude_child("PreToolUse", "a1", &p), T0 + 2).is_some(), "fresh start, fresh budget");
        t.apply_model(model_res(ModelSource::ClaudeMeta, "a1", Some("s1"), None), T0 + 2);
        t.on_exit(T0 + 3);
        assert!(t.model_probes.is_empty());
    }

    #[test]
    fn codex_child_model_comes_from_the_rollout_on_the_right_child() {
        let p = abs_jsonl("sich-cr-a.jsonl");
        let mut t = AgentTracker::default();
        t.on_codex_hook(&chook("UserPromptSubmit", ""), T0);
        let start1 = cchild("SubagentStart", "a1", &format!(r#","transcript_path":{p}"#));
        let start2 = cchild("SubagentStart", "a2", &format!(r#","transcript_path":{p}"#));
        let plan = t.codex_model_plan(&start1, T0 + 1);
        // No Codex roster yet is fine: the plan is made for the hook about to be applied, but only
        // once a status exists.
        assert!(plan.is_some());
        t.on_codex_hook(&start1, T0 + 1);
        t.on_codex_hook(&start2, T0 + 1);
        let plan = t.codex_model_plan(&start1, T0 + 2).unwrap();
        assert_eq!((plan.source, plan.agent_id.as_str()), (ModelSource::CodexRollout, "a1"));
        assert!(t.apply_model(model_res(ModelSource::CodexRollout, "a1", Some("s1"), Some("gpt-6.2-sol")), T0 + 2));
        assert_eq!(sub_model(&t, "a1").as_deref(), Some("gpt-6.2-sol"));
        assert_eq!(sub_model(&t, "a2"), None, "the other child is untouched");
        assert_eq!(t.codex_model_plan(&start1, T0 + 99_999), None, "a1 has its model");
        assert!(t.codex_model_plan(&start2, T0 + 99_999).is_some());
    }

    #[test]
    fn codex_hook_model_wins_over_the_rollout() {
        let p = abs_jsonl("sich-cr-b.jsonl");
        let mut t = AgentTracker::default();
        t.on_codex_hook(&chook("UserPromptSubmit", ""), T0);
        // A hook carrying a model needs no read; the hook value is kept even if a read lands later.
        let with_model = cchild("SubagentStart", "a1", &format!(r#","model":"hook-model","transcript_path":{p}"#));
        assert_eq!(t.codex_model_plan(&with_model, T0 + 1), None);
        t.on_codex_hook(&with_model, T0 + 1);
        assert!(!t.apply_model(model_res(ModelSource::CodexRollout, "a1", Some("s1"), Some("rollout-model")), T0 + 2));
        assert_eq!(sub_model(&t, "a1").as_deref(), Some("hook-model"));
        // Non-child events, stop events and bad paths get no plan.
        let none = |h: CodexHook| assert_eq!(t.codex_model_plan(&h, T0 + 9), None);
        none(chook("PreToolUse", &format!(r#","transcript_path":{p}"#)));
        none(cchild("SubagentStop", "a1", &format!(r#","transcript_path":{p}"#)));
        none(cchild("PreToolUse", "a2", r#","transcript_path":"rel/x.jsonl""#));
        assert_eq!(AgentTracker::default().codex_model_plan(&cchild("SubagentStart", "a2", &format!(r#","transcript_path":{p}"#)), T0), None);
    }

    #[test]
    fn working_to_done_sets_unread_via_hook_flow() {
        let w = run(None, "UserPromptSubmit", "", T0);
        assert!(!w.as_ref().unwrap().unread);
        let d = run(w, "Stop", "", T0 + 5).unwrap();
        assert!(d.unread);
    }

    // ---- subagents ----

    fn srun(
        prev: Option<AgentStatus>,
        pending: &mut VecDeque<SubagentSpawn>,
        event: &str,
        extra: &str,
        now: u64,
    ) -> Option<AgentStatus> {
        let h = hook(&payload(event, extra));
        apply_with(prev, AgentInput::Claude(&h), now, pending)
    }

    fn working() -> Option<AgentStatus> {
        st(AgentState::Working)
    }

    fn start(prev: Option<AgentStatus>, pending: &mut VecDeque<SubagentSpawn>, id: &str, ty: &str, now: u64) -> Option<AgentStatus> {
        srun(prev, pending, "SubagentStart", &format!(r#","agent_id":"{id}","agent_type":"{ty}""#), now)
    }

    #[test]
    fn parse_subagent_fields() {
        let h = hook(&payload("PreToolUse", r#","agent_id":"  agent-1 ","agent_type":" Explore ","tool_name":"Grep""#));
        assert_eq!(h.agent_id.as_deref(), Some("agent-1"));
        assert_eq!(h.agent_type.as_deref(), Some("Explore"));
        assert_eq!(h.spawn, None);
        let long = "x".repeat(200);
        let h = hook(&payload("PreToolUse", &format!(r#","agent_id":"{long}""#)));
        assert_eq!(h.agent_id.unwrap().chars().count(), 64);
        let h = hook(&payload("PreToolUse", r#","agent_id":"  ","agent_type":"""#));
        assert_eq!((h.agent_id, h.agent_type), (None, None));
        let h = hook(&payload("PreToolUse", r#","agent_id":5"#));
        assert_eq!(h.agent_id, None);
    }

    #[test]
    fn parse_spawn_from_agent_and_task_tools() {
        for tool in ["Agent", "Task"] {
            let h = hook(&payload(
                "PreToolUse",
                &format!(r#","tool_name":"{tool}","tool_input":{{"description":" Find  the\nparser ","prompt":"p","subagent_type":" Explore "}}"#),
            ));
            let sp = h.spawn.expect("spawn");
            assert_eq!(sp.agent_type, "Explore");
            assert_eq!(sp.description.as_deref(), Some("Find the parser"));
        }
        // Default type, no description, malformed input.
        let h = hook(&payload("PreToolUse", r#","tool_name":"Agent","tool_input":{}"#));
        assert_eq!(h.spawn, Some(SubagentSpawn { agent_type: "general-purpose".into(), description: None }));
        let h = hook(&payload("PreToolUse", r#","tool_name":"Agent","tool_input":"oops""#));
        assert_eq!(h.spawn.unwrap().agent_type, "general-purpose");
        // Not a spawn: other tool, other event, inside a subagent.
        assert_eq!(hook(&payload("PreToolUse", r#","tool_name":"Bash""#)).spawn, None);
        assert_eq!(hook(&payload("PostToolUse", r#","tool_name":"Agent""#)).spawn, None);
        assert_eq!(hook(&payload("PostToolUse", r#","tool_name":"Agent""#)).spawn_tool_type.as_deref(), Some("general-purpose"));
        assert_eq!(hook(&payload("PreToolUse", r#","agent_id":"a","tool_name":"Agent""#)).spawn, None);
    }

    #[test]
    fn parse_background_tasks_variants() {
        assert_eq!(hook(&payload("Stop", "")).background_tasks, None);
        assert_eq!(hook(&payload("Stop", r#","background_tasks":"x""#)).background_tasks, None);
        assert_eq!(hook(&payload("Stop", r#","background_tasks":{"id":"a"}"#)).background_tasks, None);
        assert_eq!(hook(&payload("Stop", r#","background_tasks":[]"#)).background_tasks, Some(vec![]));
        let h = hook(&payload(
            "Stop",
            r#","background_tasks":[
                {"id":"a1","type":"local_agent","status":"running","agent_type":"Explore","description":"Look  around"},
                {"id":"a2","type":"subagent","status":"Completed"},
                {"id":"a3","type":"local_subagent"},
                {"id":"a4","type":"teammate","status":"running"},
                {"id":"a5","type":"shell","status":"running"},
                {"id":"","type":"subagent"},
                {"type":"subagent"},
                {"id":7,"type":"subagent"},
                "junk", 5, null,
                {"id":"a6","type":"subagent","status":"TIMED_OUT"}
            ]"#,
        ));
        let bg = h.background_tasks.unwrap();
        let ids: Vec<(&str, bool)> = bg.iter().map(|t| (t.id.as_str(), t.running)).collect();
        assert_eq!(ids, [("a1", true), ("a2", false), ("a3", true), ("a6", false)]);
        assert_eq!(bg[0].agent_type.as_deref(), Some("Explore"));
        assert_eq!(bg[0].description.as_deref(), Some("Look around"));
        assert_eq!(bg[1].agent_type, None);
    }

    #[test]
    fn background_task_terminal_statuses_are_not_running() {
        for st in [
            "idle", "done", "success", "succeeded", "complete", "completed", "finished", "failed", "error",
            "terminated", "exited", "aborted", "expired", "skipped", "crashed", "killed", "cancelled",
            "canceled", "timed_out", "DONE",
        ] {
            let h = hook(&payload("Stop", &format!(r#","background_tasks":[{{"id":"a","type":"subagent","status":"{st}"}}]"#)));
            assert!(!h.background_tasks.unwrap()[0].running, "{st}");
        }
        for st in ["running", "pending", "started", ""] {
            let h = hook(&payload("Stop", &format!(r#","background_tasks":[{{"id":"a","type":"subagent","status":"{st}"}}]"#)));
            assert!(h.background_tasks.unwrap()[0].running, "{st}");
        }
    }

    #[test]
    fn subagent_start_without_pending_has_no_description() {
        let mut q = VecDeque::new();
        let s = start(working(), &mut q, "agent-1", "Explore", T0 + 5).unwrap();
        assert_eq!(s.subagents.len(), 1);
        let sub = &s.subagents[0];
        assert_eq!(sub.id, "agent-1");
        assert_eq!(sub.agent_type.as_deref(), Some("Explore"));
        assert_eq!(sub.description, None);
        assert_eq!((sub.state, sub.detail.clone(), sub.since_unix_ms), (AgentState::Working, None, T0 + 5));
        assert_eq!(s.state, AgentState::Working);
    }

    #[test]
    fn subagent_start_takes_matching_pending_description() {
        let mut q = VecDeque::new();
        let s = srun(
            working(),
            &mut q,
            "PreToolUse",
            r#","tool_name":"Agent","tool_input":{"description":"Find parser","subagent_type":"Explore"}"#,
            T0 + 1,
        );
        let s = srun(
            s,
            &mut q,
            "PreToolUse",
            r#","tool_name":"Task","tool_input":{"description":"Review","subagent_type":"reviewer"}"#,
            T0 + 2,
        );
        assert_eq!(q.len(), 2);
        // Main detail is the tool label as usual.
        assert_eq!(s.as_ref().unwrap().detail.as_deref(), Some("Task"));
        // Case-insensitive match picks the second entry, not the first.
        let s = start(s, &mut q, "agent-r", "Reviewer", T0 + 3).unwrap();
        assert_eq!(s.subagents[0].description.as_deref(), Some("Review"));
        assert_eq!(q.len(), 1);
        let s = start(Some(s), &mut q, "agent-e", "Explore", T0 + 4).unwrap();
        assert_eq!(s.subagents[1].description.as_deref(), Some("Find parser"));
        assert!(q.is_empty());
    }

    #[test]
    fn placeholder_labels_are_treated_as_absent() {
        let mut q = VecDeque::new();
        let s = srun(
            working(),
            &mut q,
            "PreToolUse",
            r#","tool_name":"Agent","tool_input":{"description":"Task","subagent_type":"Explore"}"#,
            T0 + 1,
        );
        let s = start(s, &mut q, "a1", "Explore", T0 + 2).unwrap();
        assert_eq!(s.subagents[0].description, None);
        let s = start(Some(s), &mut q, "a2", "Unknown", T0 + 3).unwrap();
        assert_eq!(s.subagents[1].agent_type, None);
        assert!(is_placeholder_label(" Untitled "));
        assert!(is_placeholder_label("SUBAGENT"));
        assert!(!is_placeholder_label("Explore"));
    }

    #[test]
    fn start_less_child_tool_event_creates_subagent_and_keeps_main() {
        let mut q = VecDeque::new();
        let main = srun(None, &mut q, "PreToolUse", r#","tool_name":"Bash","tool_input":{"command":"ls"}"#, T0).unwrap();
        let s = srun(
            Some(main.clone()),
            &mut q,
            "PreToolUse",
            r#","agent_id":"agent-9","agent_type":"Explore","tool_name":"Grep""#,
            T0 + 50,
        )
        .unwrap();
        assert_eq!((s.state, s.detail.clone(), s.since_unix_ms), (main.state, main.detail.clone(), main.since_unix_ms));
        assert_eq!(s.subagents.len(), 1);
        let sub = &s.subagents[0];
        assert_eq!((sub.id.as_str(), sub.since_unix_ms, sub.state), ("agent-9", T0 + 50, AgentState::Working));
        assert_eq!(sub.detail.as_deref(), Some("Grep"));
        assert_eq!(sub.agent_type.as_deref(), Some("Explore"));
        // PostToolUse / PostToolUseFailure in the child update the same entry, no duplicates.
        for ev in ["PostToolUse", "PostToolUseFailure"] {
            let s2 = srun(Some(s.clone()), &mut q, ev, r#","agent_id":"agent-9","tool_name":"Read","tool_input":{"file_path":"/a/b.rs"}"#, T0 + 60).unwrap();
            assert_eq!(s2.subagents.len(), 1);
            assert_eq!(s2.subagents[0].detail.as_deref(), Some("Read: b.rs"));
            assert_eq!(s2.subagents[0].since_unix_ms, T0 + 50);
            assert_eq!((s2.state, s2.detail.clone()), (main.state, main.detail.clone()));
        }
        // Child events without any main status are ignored.
        assert_eq!(srun(None, &mut q, "PreToolUse", r#","agent_id":"a","tool_name":"Grep""#, T0), None);
    }

    #[test]
    fn child_post_tool_use_ignores_straggler_rule() {
        let mut q = VecDeque::new();
        let done = srun(working(), &mut q, "Stop", "", T0 + 100);
        let s = srun(done, &mut q, "PostToolUse", r#","agent_id":"a","tool_name":"Read""#, T0 + 200).unwrap();
        assert_eq!(s.state, AgentState::Done);
        assert_eq!(s.subagents.len(), 1);
    }

    #[test]
    fn waiting_child_events() {
        let mut q = VecDeque::new();
        let s = start(working(), &mut q, "a1", "Explore", T0 + 1);
        for (ev, extra, want) in [
            ("PreToolUse", r#","tool_name":"AskUserQuestion""#, "AskUserQuestion"),
            ("PermissionRequest", r#","tool_name":"Bash","tool_input":{"command":"rm x"}"#, "Bash: rm x"),
            ("Notification", r#","message":"needs input","notification_type":"permission_prompt""#, "needs input"),
            ("Elicitation", r#","message":"pick one""#, "pick one"),
        ] {
            let extra = format!(r#","agent_id":"a1"{extra}"#);
            let w = srun(s.clone(), &mut q, ev, &extra, T0 + 5).unwrap();
            assert_eq!(w.subagents[0].state, AgentState::Waiting, "{ev}");
            assert_eq!(w.subagents[0].detail.as_deref(), Some(want), "{ev}");
            assert_eq!(w.state, AgentState::Working, "{ev}");
            assert_eq!(w.detail, None, "{ev}");
            // The next tool event brings it back to Working.
            let b = srun(Some(w), &mut q, "PreToolUse", r#","agent_id":"a1","tool_name":"Grep""#, T0 + 6).unwrap();
            assert_eq!(b.subagents[0].state, AgentState::Working);
        }
        // idle_prompt inside a child changes nothing.
        let same = srun(s.clone(), &mut q, "Notification", r#","agent_id":"a1","notification_type":"idle_prompt","message":"idle""#, T0 + 7);
        assert_eq!(same, s);
    }

    #[test]
    fn subagent_stop_removes_and_stores_message() {
        let mut q = VecDeque::new();
        let s = start(working(), &mut q, "a1", "Explore", T0 + 1);
        let s = start(s, &mut q, "a2", "Plan", T0 + 2);
        let s = srun(s, &mut q, "SubagentStop", r#","agent_id":"a1","agent_type":"Explore","last_assistant_message":"found it""#, T0 + 3).unwrap();
        assert_eq!(s.subagents.len(), 1);
        assert_eq!(s.subagents[0].id, "a2");
        assert_eq!(s.message.as_deref(), Some("found it"));
        assert_eq!(s.state, AgentState::Working);
        // Unknown id: no-op for the roster. Without status: nothing.
        let s2 = srun(Some(s.clone()), &mut q, "SubagentStop", r#","agent_id":"zzz""#, T0 + 4).unwrap();
        assert_eq!(s2.subagents, s.subagents);
        assert_eq!(srun(None, &mut q, "SubagentStop", r#","agent_id":"a""#, T0), None);
    }

    #[test]
    fn stop_failure_with_agent_id_only_removes_subagent() {
        let mut q = VecDeque::new();
        let s = start(working(), &mut q, "a1", "Explore", T0 + 1).unwrap();
        let f = srun(Some(s.clone()), &mut q, "StopFailure", r#","agent_id":"a1","error":"rate_limit""#, T0 + 2).unwrap();
        assert!(f.subagents.is_empty());
        assert_eq!((f.state, f.unread, f.detail.clone()), (AgentState::Working, false, None));
        // A main StopFailure still fails the main agent.
        let f = srun(Some(s), &mut q, "StopFailure", r#","error":"rate_limit""#, T0 + 3).unwrap();
        assert_eq!(f.state, AgentState::Failed);
    }

    #[test]
    fn reconcile_adds_removes_and_updates() {
        let mut q = VecDeque::new();
        let s = start(working(), &mut q, "a1", "Explore", T0 + 1);
        let s = start(s, &mut q, "a2", "Plan", T0 + 2);
        let s = srun(
            s,
            &mut q,
            "Stop",
            r#","background_tasks":[
                {"id":"a1","type":"local_agent","status":"running","description":"Scan repo","agent_type":"Task"},
                {"id":"a2","type":"local_agent","status":"completed"},
                {"id":"a3","type":"subagent","agent_type":"Reviewer"},
                {"id":"a4","type":"subagent","status":"failed"}
            ]"#,
            T0 + 10,
        )
        .unwrap();
        let ids: Vec<&str> = s.subagents.iter().map(|x| x.id.as_str()).collect();
        assert_eq!(ids, ["a1", "a3"]);
        assert_eq!(s.subagents[0].description.as_deref(), Some("Scan repo"));
        assert_eq!(s.subagents[0].agent_type.as_deref(), Some("Explore"), "placeholder does not overwrite");
        assert_eq!(s.subagents[0].since_unix_ms, T0 + 1);
        assert_eq!(s.subagents[1].agent_type.as_deref(), Some("Reviewer"));
        assert_eq!((s.subagents[1].state, s.subagents[1].since_unix_ms), (AgentState::Working, T0 + 10));
        assert_eq!(s.state, AgentState::Done);
        // Empty inventory removes everything.
        let s = srun(Some(s), &mut q, "Stop", r#","background_tasks":[]"#, T0 + 20).unwrap();
        assert!(s.subagents.is_empty());
    }

    #[test]
    fn absent_inventory_keeps_roster() {
        let mut q = VecDeque::new();
        let s = start(working(), &mut q, "a1", "Explore", T0 + 1);
        let s = srun(s, &mut q, "Stop", "", T0 + 5).unwrap();
        assert_eq!(s.state, AgentState::Done);
        assert_eq!(s.subagents.len(), 1);
        let s = srun(Some(s), &mut q, "SubagentStop", r#","agent_id":"other","background_tasks":"bad""#, T0 + 6).unwrap();
        assert_eq!(s.subagents.len(), 1);
        // SubagentStop reconciles with an inventory too.
        let s = srun(Some(s), &mut q, "SubagentStop", r#","agent_id":"a1","background_tasks":[{"id":"b","type":"subagent"}]"#, T0 + 7).unwrap();
        assert_eq!(s.subagents.len(), 1);
        assert_eq!(s.subagents[0].id, "b");
        // A new prompt keeps background subagents.
        let s = srun(Some(s), &mut q, "UserPromptSubmit", r#","prompt":"x""#, T0 + 8).unwrap();
        assert_eq!(s.subagents.len(), 1);
        assert_eq!(s.state, AgentState::Working);
    }

    #[test]
    fn subagent_stop_is_not_resurrected_by_its_own_inventory_entry() {
        let mut q = VecDeque::new();
        let s = start(working(), &mut q, "a1", "Explore", T0 + 1);
        let s = start(s, &mut q, "b1", "Plan", T0 + 2);
        // Claude lists the stopping agent's task as still running at SubagentStop time.
        let inv = r#","agent_id":"a1","background_tasks":[
            {"id":"a1","type":"local_agent","status":"running","description":"Scan"},
            {"id":"b1","type":"local_agent","status":"running"}]"#;
        let s = srun(s, &mut q, "SubagentStop", inv, T0 + 3).unwrap();
        let ids: Vec<&str> = s.subagents.iter().map(|x| x.id.as_str()).collect();
        assert_eq!(ids, ["b1"]);
        // Only the stopping agent in the inventory: the roster ends up empty.
        let s = start(working(), &mut q, "a1", "Explore", T0 + 1);
        let only = r#","agent_id":"a1","background_tasks":[{"id":"a1","type":"local_agent","status":"running"}]"#;
        let s = srun(s, &mut q, "SubagentStop", only, T0 + 2).unwrap();
        assert!(s.subagents.is_empty());
    }

    #[test]
    fn hook_agent_events_are_ignored() {
        let mut q = VecDeque::new();
        let s = start(working(), &mut q, "a1", "Explore", T0 + 1);
        for ev in ["SubagentStart", "PreToolUse", "PostToolUse", "PermissionRequest", "SubagentStop", "StopFailure"] {
            let r = srun(s.clone(), &mut q, ev, r#","agent_id":"hook-agent-42","agent_type":"x","tool_name":"Bash""#, T0 + 9);
            assert_eq!(r, s, "{ev}");
        }
        assert_eq!(srun(None, &mut q, "SubagentStart", r#","agent_id":"hook-agent-1""#, T0), None);
    }

    #[test]
    fn subagent_roster_is_capped_at_16() {
        let mut q = VecDeque::new();
        let mut s = working();
        for i in 0..20 {
            s = start(s, &mut q, &format!("a{i}"), "Explore", T0 + i);
        }
        let list = s.as_ref().unwrap().subagents.clone();
        assert_eq!(list.len(), MAX_SUBAGENTS);
        assert_eq!(list.last().unwrap().id, "a15");
        // Child tool events for a new id are dropped, known ones still update.
        let s2 = srun(s.clone(), &mut q, "PreToolUse", r#","agent_id":"new","tool_name":"Grep""#, T0 + 99).unwrap();
        assert_eq!(s2.subagents.len(), MAX_SUBAGENTS);
        let s3 = srun(s, &mut q, "PreToolUse", r#","agent_id":"a3","tool_name":"Grep""#, T0 + 99).unwrap();
        assert_eq!(s3.subagents[3].detail.as_deref(), Some("Grep"));
        // Inventory cannot overflow either.
        let many: Vec<String> = (0..30).map(|i| format!(r#"{{"id":"b{i}","type":"subagent"}}"#)).collect();
        let s4 = srun(working(), &mut q, "Stop", &format!(r#","background_tasks":[{}]"#, many.join(",")), T0).unwrap();
        assert_eq!(s4.subagents.len(), MAX_SUBAGENTS);
    }

    #[test]
    fn pending_queue_cap_and_removal_on_main_post_tool_use() {
        let mut q = VecDeque::new();
        let mut s = working();
        for i in 0..20 {
            s = srun(
                s,
                &mut q,
                "PreToolUse",
                &format!(r#","tool_name":"Agent","tool_input":{{"description":"d{i}","subagent_type":"t{i}"}}"#),
                T0 + i,
            );
        }
        assert_eq!(q.len(), MAX_PENDING_SPAWNS);
        assert_eq!(q.front().unwrap().description.as_deref(), Some("d4"), "oldest dropped");
        // Main PostToolUse of the Agent tool drops its still-queued spawn.
        let s = srun(s, &mut q, "PostToolUse", r#","tool_name":"Agent","tool_input":{"subagent_type":"t10"}"#, T0 + 100);
        assert_eq!(q.len(), MAX_PENDING_SPAWNS - 1);
        assert!(!q.iter().any(|p| p.agent_type == "t10"));
        // The same event from inside a subagent does not touch the queue.
        srun(s, &mut q, "PostToolUse", r#","agent_id":"x","tool_name":"Agent","tool_input":{"subagent_type":"t11"}"#, T0 + 101);
        assert!(q.iter().any(|p| p.agent_type == "t11"));
        // Stop clears the queue.
        srun(working(), &mut q, "Stop", "", T0 + 102);
        assert!(q.is_empty());
    }

    #[test]
    fn session_end_start_and_exit_clear_roster() {
        let mut q = VecDeque::new();
        let s = start(working(), &mut q, "a1", "Explore", T0 + 1);
        assert_eq!(srun(s.clone(), &mut q, "SessionEnd", "", T0 + 2), None);
        let fresh = srun(s.clone(), &mut q, "SessionStart", "", T0 + 2).unwrap();
        assert!(fresh.subagents.is_empty());
        assert_eq!(apply(s.clone(), AgentInput::ProcessExit, T0), None);
        // MarkSeen keeps it.
        let seen = apply(s.clone(), AgentInput::MarkSeen, T0 + 3).unwrap();
        assert_eq!(seen.subagents.len(), 1);
        // A different agent kind (title fallback) starts from scratch.
        let gem = apply(s, AgentInput::Title { title: "\u{2726} g", hooks_seen: false }, T0 + 4).unwrap();
        assert_eq!(gem.kind, AgentKind::Gemini);
        assert!(gem.subagents.is_empty());
    }

    #[test]
    fn settle_carries_roster_within_same_kind() {
        let mut q = VecDeque::new();
        let s = start(working(), &mut q, "a1", "Explore", T0 + 1);
        for ev in ["PreToolUse", "PermissionRequest", "Notification", "Stop", "UserPromptSubmit"] {
            let r = srun(s.clone(), &mut q, ev, r#","tool_name":"Bash","message":"m""#, T0 + 9).unwrap();
            assert_eq!(r.subagents.len(), 1, "{ev}");
        }
    }

    #[test]
    fn tracker_keeps_pending_queue_and_clears_it() {
        let mut t = AgentTracker::default();
        let spawn = hook(&payload("PreToolUse", r#","tool_name":"Agent","tool_input":{"description":"D","subagent_type":"Explore"}"#));
        assert!(t.on_claude_hook(&spawn, T0));
        assert_eq!(t.pending_spawns().len(), 1);
        assert!(t.on_claude_hook(&hook(&payload("SubagentStart", r#","agent_id":"a1","agent_type":"Explore""#)), T0 + 1));
        assert!(t.pending_spawns().is_empty());
        let s = t.status().unwrap();
        assert_eq!(s.subagents[0].description.as_deref(), Some("D"));
        t.on_claude_hook(&spawn, T0 + 2);
        t.on_exit(T0 + 3);
        assert!(t.pending_spawns().is_empty());
    }
}
