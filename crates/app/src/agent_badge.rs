//! Agent status badge shared by tabs and the sidebar, plus the pure logic around agent status:
//! badge choice, roll-up, "mark seen" and "flash the taskbar" decisions.

use std::time::Duration;

use gpui_kit::assets::IconName as Ic;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Icon, Size, Sizable as _};
use gpui_kit::{
    Animation, AnimationExt as _, AnyElement, App, Hsla, InteractiveElement as _, IntoElement,
    ParentElement as _, StatefulInteractiveElement as _, Styled as _, Transformation, div,
    percentage, px, rgb,
};
use sich_proto::{AgentKind, AgentState, AgentStatus};

use crate::i18n::{self, Language, Tr, tr};
use crate::settings;
use crate::theme;

/// Badge size in pixels.
const BADGE_SIZE: f32 = 14.;
/// Diameter of the "finished and seen" dot.
const SEEN_DOT_SIZE: f32 = 7.;
/// One full turn of the spinner.
const SPIN_PERIOD: Duration = Duration::from_millis(1000);

/// What a badge shows. Declared in roll-up priority order (highest first).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum BadgeKind {
    Waiting,
    Working,
    Failed,
    Done,
    /// Finished and already seen: a small persistent green dot (lowest priority).
    SeenDone,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Badge {
    pub kind: BadgeKind,
    /// Tool name or notification message for the tooltip.
    pub detail: Option<String>,
}

/// The badge of one terminal; `None` = nothing to show (Idle, or Failed already seen).
pub fn badge_for(status: &AgentStatus) -> Option<Badge> {
    let kind = match status.state {
        AgentState::Working => BadgeKind::Working,
        AgentState::Waiting => BadgeKind::Waiting,
        AgentState::Done if status.unread => BadgeKind::Done,
        AgentState::Done => BadgeKind::SeenDone,
        AgentState::Failed if status.unread => BadgeKind::Failed,
        AgentState::Failed | AgentState::Idle => return None,
    };
    Some(Badge { kind, detail: status.detail.clone() })
}

/// The most important badge of a group: Waiting > Working > Failed > Done > SeenDone > none. Ties keep the
/// first one.
pub fn rollup<'a>(statuses: impl IntoIterator<Item = &'a AgentStatus>) -> Option<Badge> {
    let mut best: Option<Badge> = None;
    for badge in statuses.into_iter().filter_map(badge_for) {
        if best.as_ref().is_none_or(|b| badge.kind < b.kind) {
            best = Some(badge);
        }
    }
    best
}

/// Badge kind of a terminal's optional status (for transition tracking).
pub fn kind_of(status: Option<&AgentStatus>) -> Option<BadgeKind> {
    status.and_then(badge_for).map(|b| b.kind)
}

/// Plain text glyph and brand color (`None` = muted) of an agent kind.
pub fn agent_glyph(kind: AgentKind) -> (&'static str, Option<u32>) {
    match kind {
        // U+273B has no emoji presentation (U+2733 renders as a green emoji on Windows).
        AgentKind::Claude => ("\u{273B}", Some(0xD97757)),
        AgentKind::Gemini => ("\u{2726}", Some(0x4796E3)),
        AgentKind::Codex => ("\u{25CE}", Some(0x10A37F)),
        AgentKind::Other => ("\u{2022}", None),
    }
}

/// `claude-opus-5-5` -> `opus-5-5` (a leading `claude-` is dropped; empty stays empty).
pub fn shorten_model(model: &str) -> String {
    let m = model.trim();
    m.strip_prefix("claude-").filter(|s| !s.is_empty()).unwrap_or(m).to_string()
}

fn non_blank(s: &Option<String>) -> Option<String> {
    s.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(String::from)
}

/// The muted second line of an agent row.
pub fn agent_preview(status: &AgentStatus) -> Option<String> {
    let (detail, prompt, message) = (non_blank(&status.detail), non_blank(&status.prompt), non_blank(&status.message));
    match status.state {
        AgentState::Waiting | AgentState::Working => detail.or(prompt),
        AgentState::Done | AgentState::Idle => message.or(prompt),
        AgentState::Failed => message.or(detail),
    }
}

/// True when the user has to be called: the terminal moved INTO Waiting, or into Done/Failed
/// with unread, from anything else.
pub fn should_flash(prev: Option<BadgeKind>, now: Option<BadgeKind>) -> bool {
    matches!(now, Some(BadgeKind::Waiting | BadgeKind::Done | BadgeKind::Failed)) && prev != now
}

/// Whether the UI should tell the daemon that the user has seen this terminal.
pub fn should_mark_seen(is_active_tab: bool, window_active: bool, status: Option<&AgentStatus>) -> bool {
    is_active_tab && window_active && status.is_some_and(|s| s.unread)
}

/// Removes leading agent status glyphs (spinner frames, star, etc.) and the spaces after them.
pub fn strip_status_glyphs(title: &str) -> &str {
    let is_glyph = |c: char| {
        matches!(c, '\u{2800}'..='\u{28FF}' | '\u{25D0}' | '\u{25D3}' | '\u{25D1}' | '\u{25D2}'
            | '\u{2733}' | '\u{2726}' | '\u{23F2}' | '\u{25C7}' | '\u{270B}')
    };
    let trimmed = title.trim_start();
    let mut chars = trimmed.chars();
    match chars.next() {
        Some(c) if is_glyph(c) => {
            // Skip any run of glyphs, then the separating whitespace.
            trimmed.trim_start_matches(is_glyph).trim_start()
        }
        _ => title,
    }
}

fn badge_color(kind: BadgeKind, cx: &App) -> Hsla {
    let p = theme::palette(settings::get(cx).theme);
    match kind {
        BadgeKind::Working => cx.theme().primary,
        BadgeKind::Waiting => rgb(p.agent_waiting).into(),
        BadgeKind::Done | BadgeKind::SeenDone => rgb(p.agent_done).into(),
        BadgeKind::Failed => rgb(p.agent_failed).into(),
    }
}

fn tooltip_text(lang: Language, badge: &Badge) -> String {
    let label = tr(
        lang,
        match badge.kind {
            BadgeKind::Working => Tr::AgentWorking,
            BadgeKind::Waiting => Tr::AgentWaiting,
            BadgeKind::Done | BadgeKind::SeenDone => Tr::AgentDone,
            BadgeKind::Failed => Tr::AgentFailed,
        },
    );
    // "Finished" has nothing useful to add.
    let detail =
        if matches!(badge.kind, BadgeKind::Done | BadgeKind::SeenDone) { None } else { badge.detail.as_deref() };
    i18n::agent_tooltip(label, detail)
}

/// The 14px icon with a tooltip. `id` must be unique among siblings.
pub fn badge_element(
    badge: &Badge,
    lang: Language,
    id: impl Into<gpui_kit::ElementId>,
    cx: &App,
) -> AnyElement {
    let color = badge_color(badge.kind, cx);
    let size = Size::Size(px(BADGE_SIZE));
    let text = tooltip_text(lang, badge);
    let id: gpui_kit::ElementId = id.into();
    let icon = match badge.kind {
        BadgeKind::Working => Icon::new(Ic::LoaderCircle)
            .with_size(size)
            .text_color(color)
            .with_animation(
                "agent-spin",
                Animation::new(SPIN_PERIOD).repeat(),
                |icon, delta| icon.transform(Transformation::rotate(percentage(delta))),
            )
            .into_any_element(),
        BadgeKind::Waiting => Icon::new(Ic::CircleAlert).with_size(size).text_color(color).into_any_element(),
        BadgeKind::Done => Icon::new(Ic::CircleCheck).with_size(size).text_color(color).into_any_element(),
        BadgeKind::Failed => Icon::new(Ic::CircleX).with_size(size).text_color(color).into_any_element(),
        BadgeKind::SeenDone => div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .child(div().size(px(SEEN_DOT_SIZE)).rounded_full().bg(color))
            .into_any_element(),
    };
    div()
        .id(id)
        .flex_shrink_0()
        .size(px(BADGE_SIZE))
        .tooltip(move |window, cx| Tooltip::new(text.clone()).build(window, cx))
        .child(icon)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sich_proto::AgentKind;

    fn st(state: AgentState, unread: bool, detail: Option<&str>) -> AgentStatus {
        AgentStatus {
            kind: AgentKind::Claude,
            state,
            unread,
            since_unix_ms: 1,
            detail: detail.map(String::from),
            model: None,
            title: None,
            prompt: None,
            message: None,
        }
    }

    fn full(state: AgentState, detail: Option<&str>, prompt: Option<&str>, message: Option<&str>) -> AgentStatus {
        AgentStatus {
            prompt: prompt.map(String::from),
            message: message.map(String::from),
            ..st(state, false, detail)
        }
    }

    #[test]
    fn preview_rules_per_state() {
        let p = |s: AgentState, d, pr, m| agent_preview(&full(s, d, pr, m));
        // Waiting / Working: detail, else prompt (never the message).
        for s in [AgentState::Waiting, AgentState::Working] {
            assert_eq!(p(s, Some("Bash: ls"), Some("p"), Some("m")).as_deref(), Some("Bash: ls"));
            assert_eq!(p(s, None, Some("p"), Some("m")).as_deref(), Some("p"));
            assert_eq!(p(s, Some("  "), Some("p"), None).as_deref(), Some("p"));
            assert_eq!(p(s, None, None, Some("m")), None);
        }
        // Done / Idle: message, else prompt (never the detail).
        for s in [AgentState::Done, AgentState::Idle] {
            assert_eq!(p(s, Some("d"), Some("p"), Some("m")).as_deref(), Some("m"));
            assert_eq!(p(s, Some("d"), Some("p"), None).as_deref(), Some("p"));
            assert_eq!(p(s, Some("d"), None, None), None);
        }
        // Failed: message, else detail.
        assert_eq!(p(AgentState::Failed, Some("d"), Some("p"), Some("m")).as_deref(), Some("m"));
        assert_eq!(p(AgentState::Failed, Some("d"), Some("p"), None).as_deref(), Some("d"));
        assert_eq!(p(AgentState::Failed, None, Some("p"), None), None);
    }

    #[test]
    fn shorten_model_strips_claude_prefix() {
        assert_eq!(shorten_model("claude-opus-5-5"), "opus-5-5");
        assert_eq!(shorten_model("claude-sonnet-5-5"), "sonnet-5-5");
        assert_eq!(shorten_model("gemini-2.5-pro"), "gemini-2.5-pro");
        assert_eq!(shorten_model(" claude-haiku "), "haiku");
        assert_eq!(shorten_model("claude-"), "claude-");
        assert_eq!(shorten_model(""), "");
    }

    #[test]
    fn glyph_per_agent_kind() {
        assert_eq!(agent_glyph(AgentKind::Claude), ("\u{273B}", Some(0xD97757)));
        assert_eq!(agent_glyph(AgentKind::Gemini), ("\u{2726}", Some(0x4796E3)));
        assert_eq!(agent_glyph(AgentKind::Codex), ("\u{25CE}", Some(0x10A37F)));
        assert_eq!(agent_glyph(AgentKind::Other).1, None);
    }

    fn kind(s: &AgentStatus) -> Option<BadgeKind> {
        badge_for(s).map(|b| b.kind)
    }

    #[test]
    fn badge_per_state() {
        assert_eq!(kind(&st(AgentState::Working, false, None)), Some(BadgeKind::Working));
        assert_eq!(kind(&st(AgentState::Waiting, false, None)), Some(BadgeKind::Waiting));
        assert_eq!(kind(&st(AgentState::Done, true, None)), Some(BadgeKind::Done));
        assert_eq!(kind(&st(AgentState::Failed, true, None)), Some(BadgeKind::Failed));
    }

    #[test]
    fn read_or_idle_has_no_badge() {
        assert_eq!(kind(&st(AgentState::Idle, false, None)), None);
        assert_eq!(kind(&st(AgentState::Idle, true, None)), None);
        assert_eq!(kind(&st(AgentState::Failed, false, None)), None);
    }

    #[test]
    fn seen_done_is_a_dot() {
        assert_eq!(kind(&st(AgentState::Done, false, None)), Some(BadgeKind::SeenDone));
        assert_eq!(kind(&st(AgentState::Done, true, None)), Some(BadgeKind::Done));
    }

    #[test]
    fn badge_for_every_state_and_unread() {
        use AgentState::*;
        let expect = |s, u| kind(&st(s, u, None));
        assert_eq!(expect(Working, false), Some(BadgeKind::Working));
        assert_eq!(expect(Working, true), Some(BadgeKind::Working));
        assert_eq!(expect(Waiting, false), Some(BadgeKind::Waiting));
        assert_eq!(expect(Waiting, true), Some(BadgeKind::Waiting));
        assert_eq!(expect(Idle, false), None);
        assert_eq!(expect(Idle, true), None);
        assert_eq!(expect(Done, false), Some(BadgeKind::SeenDone));
        assert_eq!(expect(Done, true), Some(BadgeKind::Done));
        assert_eq!(expect(Failed, false), None);
        assert_eq!(expect(Failed, true), Some(BadgeKind::Failed));
    }

    #[test]
    fn seen_done_is_lowest_in_rollup_and_never_flashes() {
        let seen = st(AgentState::Done, false, None);
        let unread = st(AgentState::Done, true, None);
        let idle = st(AgentState::Idle, false, None);
        let k = |v: Vec<&AgentStatus>| rollup(v).map(|b| b.kind);
        assert_eq!(k(vec![&idle, &seen]), Some(BadgeKind::SeenDone));
        assert_eq!(k(vec![&seen, &unread]), Some(BadgeKind::Done));
        assert!(BadgeKind::Done < BadgeKind::SeenDone);
        assert!(!should_flash(Some(BadgeKind::Done), Some(BadgeKind::SeenDone)));
        assert!(!should_flash(None, Some(BadgeKind::SeenDone)));
    }

    #[test]
    fn badge_keeps_detail() {
        let b = badge_for(&st(AgentState::Waiting, false, Some("Allow Bash?"))).unwrap();
        assert_eq!(b.detail.as_deref(), Some("Allow Bash?"));
    }

    #[test]
    fn rollup_priority() {
        let waiting = st(AgentState::Waiting, false, None);
        let working = st(AgentState::Working, false, None);
        let failed = st(AgentState::Failed, true, None);
        let done = st(AgentState::Done, true, None);
        let idle = st(AgentState::Idle, false, None);
        let k = |v: Vec<&AgentStatus>| rollup(v).map(|b| b.kind);
        assert_eq!(k(vec![&done, &failed, &working, &waiting, &idle]), Some(BadgeKind::Waiting));
        assert_eq!(k(vec![&done, &failed, &working, &idle]), Some(BadgeKind::Working));
        assert_eq!(k(vec![&done, &failed, &idle]), Some(BadgeKind::Failed));
        assert_eq!(k(vec![&idle, &done]), Some(BadgeKind::Done));
        assert_eq!(k(vec![&idle]), None);
        assert_eq!(k(vec![]), None);
    }

    #[test]
    fn rollup_ignores_read_results() {
        let read_failed = st(AgentState::Failed, false, None);
        let idle = st(AgentState::Idle, false, None);
        assert_eq!(rollup([&read_failed, &idle]), None);
    }

    #[test]
    fn rollup_ties_keep_first_detail() {
        let a = st(AgentState::Working, false, Some("Bash"));
        let b = st(AgentState::Working, false, Some("Edit"));
        assert_eq!(rollup([&a, &b]).unwrap().detail.as_deref(), Some("Bash"));
    }

    #[test]
    fn mark_seen_decision() {
        let unread = st(AgentState::Done, true, None);
        let read = st(AgentState::Done, false, None);
        assert!(should_mark_seen(true, true, Some(&unread)));
        assert!(!should_mark_seen(false, true, Some(&unread)));
        assert!(!should_mark_seen(true, false, Some(&unread)));
        assert!(!should_mark_seen(true, true, Some(&read)));
        assert!(!should_mark_seen(true, true, None));
    }

    #[test]
    fn flash_only_on_transition_into_attention() {
        use BadgeKind::*;
        assert!(should_flash(None, Some(Waiting)));
        assert!(should_flash(Some(Working), Some(Waiting)));
        assert!(should_flash(Some(Working), Some(Done)));
        assert!(should_flash(Some(Working), Some(Failed)));
        assert!(should_flash(Some(Waiting), Some(Done)));
        // No change, or moving to something that does not need the user.
        assert!(!should_flash(Some(Waiting), Some(Waiting)));
        assert!(!should_flash(Some(Done), Some(Done)));
        assert!(!should_flash(Some(Waiting), Some(Working)));
        assert!(!should_flash(Some(Done), None));
        assert!(!should_flash(None, None));
        assert!(!should_flash(None, Some(Working)));
    }

    #[test]
    fn kind_of_matches_badge() {
        assert_eq!(kind_of(None), None);
        assert_eq!(kind_of(Some(&st(AgentState::Done, true, None))), Some(BadgeKind::Done));
        assert_eq!(kind_of(Some(&st(AgentState::Done, false, None))), Some(BadgeKind::SeenDone));
        assert_eq!(kind_of(Some(&st(AgentState::Idle, false, None))), None);
    }

    #[test]
    fn strips_status_glyphs() {
        assert_eq!(strip_status_glyphs("\u{2810} Fix bug"), "Fix bug");
        assert_eq!(strip_status_glyphs("\u{2733} Claude Code"), "Claude Code");
        assert_eq!(strip_status_glyphs("\u{25D0} task"), "task");
        assert_eq!(strip_status_glyphs("\u{25D3}\u{25D1}  x"), "x");
        assert_eq!(strip_status_glyphs("\u{2726} gemini"), "gemini");
        assert_eq!(strip_status_glyphs("\u{23F2} wait"), "wait");
        assert_eq!(strip_status_glyphs("\u{25C7} ready"), "ready");
        assert_eq!(strip_status_glyphs("\u{270B} action"), "action");
        assert_eq!(strip_status_glyphs("\u{2800}\u{28FF} y"), "y");
    }

    #[test]
    fn plain_titles_are_untouched() {
        assert_eq!(strip_status_glyphs("cargo build"), "cargo build");
        assert_eq!(strip_status_glyphs(""), "");
        assert_eq!(strip_status_glyphs("pwsh \u{2733} x"), "pwsh \u{2733} x");
        assert_eq!(strip_status_glyphs("  spaced"), "  spaced");
    }

    #[test]
    fn glyph_only_title_becomes_empty() {
        assert_eq!(strip_status_glyphs("\u{2733}"), "");
        assert_eq!(strip_status_glyphs("\u{2733} "), "");
    }

    #[test]
    fn tooltips_are_translated_with_detail() {
        let working = Badge { kind: BadgeKind::Working, detail: Some("Bash".into()) };
        assert_eq!(tooltip_text(Language::En, &working), "Working: Bash");
        let waiting = Badge { kind: BadgeKind::Waiting, detail: Some("Allow?".into()) };
        assert_eq!(tooltip_text(Language::Uk, &waiting), "Чекає на вас: Allow?");
        let done = Badge { kind: BadgeKind::Done, detail: Some("x".into()) };
        assert_eq!(tooltip_text(Language::En, &done), "Finished");
        let failed = Badge { kind: BadgeKind::Failed, detail: None };
        assert_eq!(tooltip_text(Language::Ru, &failed), "Ошибка");
    }
}
