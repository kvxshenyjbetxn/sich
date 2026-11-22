//! Pure policy and formatting for agent notifications.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use sich_proto::{AgentKind, AgentState, ProjectInfo, TermId, TerminalInfo};

use crate::agent_badge::{BadgeKind, agent_preview, badge_for, should_flash, strip_status_glyphs};
use crate::i18n::{Language, Tr, tr};
use crate::settings::AppSettings;
use crate::sidebar::worktree_label;
use crate::workspace::same_path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotifyKind {
    NeedsInput,
    Done,
    Failed,
}

pub fn notify_kind(prev: Option<BadgeKind>, now: Option<BadgeKind>) -> Option<NotifyKind> {
    match now.filter(|_| should_flash(prev, now))? {
        BadgeKind::Waiting => Some(NotifyKind::NeedsInput),
        BadgeKind::Done => Some(NotifyKind::Done),
        BadgeKind::Failed => Some(NotifyKind::Failed),
        BadgeKind::Working | BadgeKind::SeenDone => None,
    }
}

pub fn should_notify(
    kind: NotifyKind,
    prefs: &AppSettings,
    window_active: bool,
    in_selected_workspace: bool,
) -> bool {
    let enabled = match kind {
        NotifyKind::NeedsInput => prefs.notify_agent_needs_input,
        NotifyKind::Done | NotifyKind::Failed => prefs.notify_agent_done,
    };
    prefs.notifications_enabled && enabled && !(window_active && in_selected_workspace)
}

const COOLDOWN: Duration = Duration::from_millis(5000);

#[derive(Default)]
pub struct Cooldown(HashMap<TermId, Instant>);

impl Cooldown {
    pub fn allow(&mut self, id: TermId, now: Instant) -> bool {
        if self
            .0
            .get(&id)
            .is_some_and(|last| now.saturating_duration_since(*last) < COOLDOWN)
        {
            return false;
        }
        self.0.insert(id, now);
        true
    }

    pub fn retain(&mut self, mut is_alive: impl FnMut(TermId) -> bool) {
        self.0.retain(|&id, _| is_alive(id));
    }
}

pub fn project_label(info: &TerminalInfo, projects: &[ProjectInfo]) -> String {
    if let Some(path) = info.worktree.as_deref()
        && let Some((project, worktree)) = projects.iter().find_map(|project| {
            project
                .worktrees
                .iter()
                .find(|w| same_path(&w.path, path))
                .map(|w| (project, w))
        })
    {
        return if worktree.is_main && !project.name.is_empty() {
            project.name.clone()
        } else {
            worktree_label(worktree)
        };
    }
    info.worktree
        .as_deref()
        .and_then(|path| path.file_name())
        .or_else(|| info.cwd.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn agent_label(kind: Option<AgentKind>, lang: Language) -> &'static str {
    match kind {
        Some(AgentKind::Claude) => "Claude Code",
        Some(AgentKind::Gemini) => "Gemini",
        Some(AgentKind::Codex) => "Codex",
        Some(AgentKind::Omp) => "omp",
        Some(AgentKind::Other) | None => tr(lang, Tr::ToastAgent),
    }
}

pub fn toast_text(
    kind: NotifyKind,
    info: &TerminalInfo,
    project: &str,
    lang: Language,
) -> (String, String) {
    let agent = info.agent.as_ref();
    let label = agent_label(agent.map(|status| status.kind), lang);
    let status_label = tr(
        lang,
        match kind {
            NotifyKind::NeedsInput => Tr::ToastNeedsInput,
            NotifyKind::Done => Tr::ToastDone,
            NotifyKind::Failed => Tr::ToastFailed,
        },
    );
    let title = if project.is_empty() {
        format!("{label} {status_label}")
    } else {
        format!("{project} · {label} {status_label}")
    };
    let preview = agent.and_then(|status| {
        if kind == NotifyKind::NeedsInput && status.state != AgentState::Waiting {
            // The waiting badge can come from a subagent while the main agent is in another state.
            badge_for(status)
                .filter(|badge| badge.kind == BadgeKind::Waiting)
                .and_then(|badge| badge.detail)
        } else {
            agent_preview(status)
        }
    });
    let body = preview
        .as_deref()
        .unwrap_or_else(|| strip_status_glyphs(&info.title));
    let mut body = body.split_whitespace().collect::<Vec<_>>().join(" ");
    const BODY_LIMIT: usize = 180;
    let mut chars = body.char_indices();
    if let Some((end, _)) = chars.nth(BODY_LIMIT - 1)
        && chars.next().is_some()
    {
        body.truncate(end);
        body.push('…');
    }
    (title, body)
}

fn xml_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            '\t'
            | '\n'
            | '\r'
            | '\u{20}'..='\u{d7ff}'
            | '\u{e000}'..='\u{fffd}'
            | '\u{10000}'.. => escaped.push(c),
            _ => {} // XML 1.0 disallows some control characters even as entities.
        }
    }
    escaped
}

pub fn toast_xml(title: &str, body: &str, silent: bool) -> String {
    let audio = if silent {
        r#"<audio silent="true"/>"#
    } else {
        ""
    };
    format!(
        r#"<toast><visual><binding template="ToastGeneric"><text>{}</text><text>{}</text></binding></visual>{audio}</toast>"#,
        xml_escape(title),
        xml_escape(body),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use sich_proto::{AgentStatus, SubagentStatus, WorktreeInfo};

    #[test]
    fn only_transitions_into_attention_states_notify() {
        let cases = [
            (None, None),
            (Some(BadgeKind::Working), None),
            (Some(BadgeKind::Waiting), Some(NotifyKind::NeedsInput)),
            (Some(BadgeKind::Done), Some(NotifyKind::Done)),
            (Some(BadgeKind::Failed), Some(NotifyKind::Failed)),
            (Some(BadgeKind::SeenDone), None),
        ];
        for (prev, _) in cases {
            for (now, expected) in cases {
                assert_eq!(
                    notify_kind(prev, now),
                    if prev == now { None } else { expected },
                    "{prev:?} -> {now:?}"
                );
            }
        }
    }

    #[test]
    fn gates_each_kind_on_settings_and_workspace_visibility() {
        for flags in 0..32 {
            let on = |bit: u8| flags & (1 << bit) != 0;
            let prefs = AppSettings {
                notifications_enabled: on(0),
                notify_agent_needs_input: on(1),
                notify_agent_done: on(2),
                ..Default::default()
            };
            let (active, selected) = (on(3), on(4));
            for (kind, bit) in [
                (NotifyKind::NeedsInput, 1),
                (NotifyKind::Done, 2),
                (NotifyKind::Failed, 2),
            ] {
                let expected = on(0) && on(bit) && (!active || !selected);
                assert_eq!(
                    should_notify(kind, &prefs, active, selected),
                    expected,
                    "{kind:?} {flags:05b}"
                );
            }
        }
    }

    #[test]
    fn cooldown_is_per_terminal_and_measured_since_last_allowed() {
        let mut cooldown = Cooldown::default();
        let now = Instant::now();
        let inside = now + Duration::from_millis(4999);
        assert!(cooldown.allow(1, now));
        assert!(!cooldown.allow(1, inside));
        assert!(cooldown.allow(2, inside));
        assert!(cooldown.allow(1, now + COOLDOWN));
        assert!(!cooldown.allow(2, now + COOLDOWN));
        assert!(!cooldown.allow(1, now + COOLDOWN + Duration::from_millis(1)));
        cooldown.retain(|id| id == 2);
        assert!(cooldown.allow(1, now + COOLDOWN));
        assert!(!cooldown.allow(2, now + COOLDOWN));
    }

    fn terminal(state: AgentState) -> TerminalInfo {
        TerminalInfo {
            id: 1,
            title: "  ⠁✳   terminal\t title\n".into(),
            cwd: "checkout/subdir".into(),
            cols: 80,
            rows: 24,
            exited: None,
            created_unix_ms: 0,
            worktree: None,
            agent: Some(AgentStatus {
                kind: AgentKind::Codex,
                state,
                unread: true,
                since_unix_ms: 0,
                detail: Some(" Needs\n\t a decision ".into()),
                model: None,
                title: None,
                prompt: Some("Last prompt".into()),
                message: Some("  Last\r\n message ".into()),
                subagents: Vec::new(),
                session_id: None,
            }),
        }
    }

    #[test]
    fn toast_text_uses_localized_status_and_the_agent_preview() {
        for (kind, state, statuses, body) in [
            (
                NotifyKind::NeedsInput,
                AgentState::Waiting,
                ["потребує уваги", "требует внимания", "needs input"],
                "Needs a decision",
            ),
            (
                NotifyKind::Done,
                AgentState::Done,
                ["завершив", "завершил", "finished"],
                "Last message",
            ),
            (
                NotifyKind::Failed,
                AgentState::Failed,
                ["помилка", "ошибка", "failed"],
                "Last message",
            ),
        ] {
            for (lang, status) in [Language::Uk, Language::Ru, Language::En]
                .into_iter()
                .zip(statuses)
            {
                assert_eq!(
                    toast_text(kind, &terminal(state), "project", lang),
                    (format!("project · Codex {status}"), body.into())
                );
            }
        }
    }

    #[test]
    fn missing_preview_uses_secondary_text_then_the_terminal_title() {
        for (kind, state, detail, prompt, expected) in [
            (
                NotifyKind::NeedsInput,
                AgentState::Waiting,
                Some(" \t"),
                Some("Input prompt"),
                "Input prompt",
            ),
            (
                NotifyKind::Done,
                AgentState::Done,
                Some("tool"),
                Some("Task prompt"),
                "Task prompt",
            ),
            (
                NotifyKind::Failed,
                AgentState::Failed,
                Some("Error detail"),
                Some("Task prompt"),
                "Error detail",
            ),
            (
                NotifyKind::Done,
                AgentState::Done,
                Some("tool"),
                None,
                "terminal title",
            ),
        ] {
            let mut info = terminal(state);
            let agent = info.agent.as_mut().unwrap();
            agent.message = None;
            agent.detail = detail.map(String::from);
            agent.prompt = prompt.map(String::from);
            assert_eq!(toast_text(kind, &info, "", Language::En).1, expected);
        }
        let mut info = terminal(AgentState::Idle);
        info.agent = None;
        assert_eq!(
            toast_text(NotifyKind::Done, &info, "", Language::En),
            ("Agent finished".into(), "terminal title".into())
        );
    }

    #[test]
    fn subagent_waiting_toast_uses_the_waiting_badge_detail() {
        let mut info = terminal(AgentState::Done);
        info.agent.as_mut().unwrap().subagents.push(SubagentStatus {
            id: "child".into(),
            agent_type: None,
            description: Some("Explore".into()),
            state: AgentState::Waiting,
            detail: Some("needs permission".into()),
            since_unix_ms: 0,
            model: None,
        });
        assert_eq!(
            toast_text(NotifyKind::NeedsInput, &info, "", Language::En).1,
            "Explore: needs permission"
        );
    }

    #[test]
    fn body_limit_preserves_utf8_and_includes_the_ellipsis() {
        let mut info = terminal(AgentState::Done);
        for len in [179, 180, 181, 250] {
            info.agent.as_mut().unwrap().message = Some("🦀".repeat(len));
            let (_, body) = toast_text(NotifyKind::Done, &info, "", Language::En);
            let expected = if len > 180 {
                format!("{}…", "🦀".repeat(179))
            } else {
                "🦀".repeat(len)
            };
            assert_eq!(body, expected);
        }
    }

    #[test]
    fn project_label_uses_sidebar_workspace_name_or_directory_basename() {
        let mut info = terminal(AgentState::Done);
        assert_eq!(project_label(&info, &[]), "subdir");
        info.worktree = Some("other/checkout".into());
        assert_eq!(project_label(&info, &[]), "checkout");
        let mut project = ProjectInfo {
            id: 1,
            name: "project".into(),
            path: "other".into(),
            is_git: true,
            error: None,
            group: None,
            worktrees: vec![WorktreeInfo {
                path: "other/checkout".into(),
                branch: Some("fix/feature".into()),
                head: String::new(),
                is_main: false,
                dirty: false,
            }],
        };
        assert_eq!(
            project_label(&info, std::slice::from_ref(&project)),
            "fix/feature"
        );
        project.worktrees[0].is_main = true;
        assert_eq!(project_label(&info, &[project]), "project");
    }

    #[test]
    fn escapes_xml_metacharacters_and_omits_invalid_xml_controls() {
        assert_eq!(
            xml_escape("&<>\"'\0\u{1b}\u{fffe}🦀"),
            "&amp;&lt;&gt;&quot;&apos;🦀"
        );
    }

    #[test]
    fn toast_xml_escapes_title_and_body_and_sets_audio_only_when_silent() {
        let visual = r#"<visual><binding template="ToastGeneric"><text>A &amp; B</text><text>&lt;C&gt;</text></binding></visual>"#;
        assert_eq!(
            toast_xml("A & B", "<C>", false),
            format!("<toast>{visual}</toast>")
        );
        assert_eq!(
            toast_xml("A & B", "<C>", true),
            format!(r#"<toast>{visual}<audio silent="true"/></toast>"#)
        );
    }
}
