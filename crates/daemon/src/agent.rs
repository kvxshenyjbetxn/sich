//! AI agent status state machine. Pure logic: no IPC, no clocks (the caller passes `now`).
//!
//! Two sources feed it:
//! - Claude Code hook events (`SICH_TERM_ID` + `sich hook claude`), which are authoritative;
//! - terminal title glyphs (Claude's spinner, Gemini's markers), used only while the terminal
//!   has not received any hook event during the current agent session.

use sich_proto::{AgentKind, AgentState, AgentStatus};

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
];

/// The fields we use from a Claude hook payload.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaudeHook {
    pub event: String,
    pub tool_name: Option<String>,
    pub notification_type: Option<String>,
    pub message: Option<String>,
    pub error: Option<String>,
}

impl ClaudeHook {
    pub fn is_known(&self) -> bool {
        KNOWN_CLAUDE_EVENTS.contains(&self.event.as_str())
    }
}

/// Parses the hook stdin JSON. `None` if it is not a JSON object with `hook_event_name`.
pub fn parse_claude_payload(json: &str) -> Option<ClaudeHook> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let obj = v.as_object()?;
    let get = |k: &str| obj.get(k).and_then(|x| x.as_str()).map(str::to_string);
    Some(ClaudeHook {
        event: get("hook_event_name")?,
        tool_name: get("tool_name"),
        notification_type: get("notification_type"),
        message: get("message"),
        error: get("error"),
    })
}

pub enum AgentInput<'a> {
    Claude(&'a ClaudeHook),
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
    Some(AgentStatus { kind, state, unread, since_unix_ms: since, detail })
}

/// All transitions. Returns the new status; equal to `prev` when nothing changed.
pub fn apply(prev: Option<AgentStatus>, input: AgentInput<'_>, now: u64) -> Option<AgentStatus> {
    match input {
        AgentInput::ProcessExit => None,
        AgentInput::MarkSeen => match prev {
            Some(mut p) => {
                if p.unread {
                    p.unread = false;
                }
                if p.state == AgentState::Done {
                    p.state = AgentState::Idle;
                    p.since_unix_ms = now;
                }
                Some(p)
            }
            None => None,
        },
        AgentInput::Claude(h) => apply_claude(prev, h, now),
        AgentInput::Title { title, hooks_seen } => {
            if hooks_seen {
                prev
            } else {
                apply_title(prev, title, now)
            }
        }
    }
}

fn apply_claude(prev: Option<AgentStatus>, h: &ClaudeHook, now: u64) -> Option<AgentStatus> {
    let kind = AgentKind::Claude;
    let tool = non_empty(&h.tool_name);
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
    settle(prev, kind, state, unread, detail, now)
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
        // A plain title: while working it may be transient; otherwise the agent is gone.
        return match prev {
            Some(p) if p.state == AgentState::Working => Some(p),
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
}

impl AgentTracker {
    pub fn status(&self) -> Option<&AgentStatus> {
        self.status.as_ref()
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
        self.update(|prev| apply(prev, AgentInput::Claude(h), now))
    }

    pub fn on_title(&mut self, title: &str, now: u64) -> bool {
        let hooks_seen = self.hooks_seen;
        self.update(|prev| apply(prev, AgentInput::Title { title, hooks_seen }, now))
    }

    pub fn on_exit(&mut self) -> bool {
        self.hooks_seen = false;
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
        })
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
    fn mark_seen_done_becomes_idle_read() {
        let done = run(st(AgentState::Working), "Stop", "", T0 + 100);
        let s = apply(done, AgentInput::MarkSeen, T0 + 900).unwrap();
        assert_eq!(s.state, AgentState::Idle);
        assert!(!s.unread);
        assert_eq!(s.since_unix_ms, T0 + 900);
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
        assert!(t.on_exit());
        assert!(!t.on_exit());
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
        assert_eq!((s.state, s.unread), (AgentState::Idle, false));
        assert!(!t.mark_seen(T0 + 4));
    }

    #[test]
    fn working_to_done_sets_unread_via_hook_flow() {
        let w = run(None, "UserPromptSubmit", "", T0);
        assert!(!w.as_ref().unwrap().unread);
        let d = run(w, "Stop", "", T0 + 5).unwrap();
        assert!(d.unread);
    }
}
