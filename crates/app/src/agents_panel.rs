//! The "Agents" tab of the right panel: the history of the agents' sessions found on disk
//! (`agent_history`), with search, scope and agent filters, and per-session actions (resume, jump
//! to the live terminal, hand off to another agent, open the transcript...).
//!
//! The panel only READS the agents' files (on the background executor, bounded and cached, see
//! `agent_history`). The pure decisions live at the top and are unit-tested; the view below is
//! plumbing.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::assets::IconName as Ic;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, Selectable as _, Sizable as _, Theme, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, AppContext as _, ClickEvent, ClipboardItem, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, WeakEntity,
    Window, div, px, uniform_list,
};
use sich_proto::{AgentKind, AgentState, ProjectInfo, TerminalInfo};

use crate::agent_badge::{agent_icon_element, badge_element, badge_for};
use crate::agent_history::{
    self, DEFAULT_LIMIT, Filter, LiveSession, Role, Scanner, Scope, SessionMeta, Turn, filter_sessions, live_sessions,
};
use crate::app_view::AppView;
use crate::handoff::{self, FolderChoice, HandoffOrigin, Resolved, TranscriptDirs};
use crate::i18n::{self, Language, Tr, relative_time, tr};
use crate::workspace::WorkspaceKey;
use crate::{handoff_dialog, settings};

/// The list is re-read at most this often while the tab is visible.
const TICK: Duration = Duration::from_secs(30);
/// Window activation refreshes at most this often.
const ACTIVATION_MIN: Duration = Duration::from_secs(10);
const ROW_H: f32 = 44.;
const DETAILS_MAX_H: f32 = 200.;
const DETAIL_TURNS: usize = 8;

// ---------------------------------------------------------------------------------------------
// Pure logic
// ---------------------------------------------------------------------------------------------

/// What "Resume" does: the official command and where the terminal opens.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ResumePlan {
    pub command: String,
    pub resolved: Resolved,
}

/// `None` when the session cannot be resumed: its id is not a valid provider id, or its folder is
/// unknown or gone (`cwd_exists`). The terminal opens in the session's own folder (the agents look
/// sessions up by folder), inside the known worktree that contains it, else in a Home terminal.
pub(crate) fn resume_plan(meta: &SessionMeta, projects: &[ProjectInfo], home: Option<&Path>, cwd_exists: bool) -> Option<ResumePlan> {
    let command = crate::session::resume_command(meta.kind, &meta.id)?;
    let cwd = meta.cwd.as_ref().filter(|_| cwd_exists)?;
    let mut resolved = handoff::resolve_target(&FolderChoice::Path(cwd.clone()), projects, home);
    if resolved.workspace == WorkspaceKey::Home
        && let Some(worktree) = handoff::containing_worktree(projects, cwd)
    {
        resolved.workspace = WorkspaceKey::Worktree(worktree);
    }
    Some(ResumePlan { command, resolved })
}

/// Whether the Resume button is enabled (as of the last scan).
pub(crate) fn resume_available(meta: &SessionMeta) -> bool {
    meta.cwd_exists && crate::session::resume_command(meta.kind, &meta.id).is_some()
}

/// Second line of a row: "<last prompt> · <branch>".
pub(crate) fn row_subtitle(meta: &SessionMeta) -> String {
    let branch = meta.branch.as_deref().filter(|b| !b.is_empty());
    [Some(meta.last_prompt.as_str()).filter(|p| !p.is_empty()), branch].into_iter().flatten().collect::<Vec<_>>().join(" \u{b7} ")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MenuAction {
    OpenTranscript,
    Reveal,
    CopyId,
    CopyPath,
    CopyResume,
}

/// The "..." menu in order, with labels.
pub(crate) const MENU: [(MenuAction, Tr); 5] = [
    (MenuAction::OpenTranscript, Tr::AgentsOpenTranscript),
    (MenuAction::Reveal, Tr::AgentsReveal),
    (MenuAction::CopyId, Tr::AgentsCopyId),
    (MenuAction::CopyPath, Tr::AgentsCopyPath),
    (MenuAction::CopyResume, Tr::AgentsCopyResume),
];

/// What a copy action puts on the clipboard (`None` for the other actions, or when there is nothing to copy).
pub(crate) fn copy_text(action: MenuAction, meta: &SessionMeta) -> Option<String> {
    match action {
        MenuAction::CopyId => Some(meta.id.clone()),
        MenuAction::CopyPath => Some(meta.path.display().to_string()),
        MenuAction::CopyResume => crate::session::resume_command(meta.kind, &meta.id),
        MenuAction::OpenTranscript | MenuAction::Reveal => None,
    }
}

/// Window activation refreshes the list unless it did so very recently.
pub(crate) fn activation_due(last: Option<Instant>, now: Instant) -> bool {
    last.is_none_or(|last| now.saturating_duration_since(last) >= ACTIVATION_MIN)
}

/// What the app remembers about a terminal's agent to notice when the history should be re-read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SeenAgent {
    pub session_id: Option<String>,
    /// Idle or Done: the agent finished what it was doing.
    pub settled: bool,
}

pub(crate) fn seen_agent(info: &TerminalInfo) -> Option<SeenAgent> {
    let agent = info.agent.as_ref()?;
    Some(SeenAgent { session_id: agent.session_id.clone(), settled: matches!(agent.state, AgentState::Idle | AgentState::Done) })
}

/// A terminal's agent went to Done/Idle or got another session id since `prev` (the first sight of
/// a terminal only seeds `prev`).
pub(crate) fn history_changed(prev: Option<&SeenAgent>, now: Option<&SeenAgent>) -> bool {
    let (Some(prev), Some(now)) = (prev, now) else { return false };
    (now.settled && !prev.settled) || (now.session_id.is_some() && now.session_id != prev.session_id)
}

/// Rows to show: indices into the session list plus the live terminal of each, if any.
pub(crate) fn visible_rows(
    sessions: &[SessionMeta],
    live: &std::collections::HashMap<String, LiveSession>,
    filter: &Filter<'_>,
) -> Vec<(usize, Option<LiveSession>)> {
    filter_sessions(sessions, filter)
        .into_iter()
        .map(|index| {
            let live = live.get(&sessions[index].id).filter(|l| l.status.kind == sessions[index].kind).cloned();
            (index, live)
        })
        .collect()
}

fn scope_label(scope: Scope) -> Tr {
    match scope {
        Scope::Worktree => Tr::AgentsScopeWorktree,
        Scope::Project => Tr::AgentsScopeProject,
        Scope::All => Tr::AgentsScopeAll,
    }
}

// ---------------------------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------------------------

struct Expanded {
    path: PathBuf,
    /// The file's mtime the turns were read at.
    updated_ms: u64,
    /// `None` while loading.
    turns: Option<Vec<Turn>>,
}

pub(crate) struct AgentsPanel {
    app: WeakEntity<AppView>,
    dirs: TranscriptDirs,
    /// Taken while a scan runs in the background (one scan at a time).
    scanner: Option<Scanner>,
    sessions: Arc<Vec<SessionMeta>>,
    total_files: usize,
    limit: usize,
    loaded: bool,
    rescan: bool,
    error: Option<String>,
    visible: bool,
    /// Something happened while the tab was hidden: read again when it is shown.
    stale: bool,
    last_activation: Option<Instant>,
    scope: Scope,
    claude: bool,
    codex: bool,
    search: Option<Entity<InputState>>,
    _search_events: Option<Subscription>,
    expanded: Option<Expanded>,
}

impl AgentsPanel {
    pub(crate) fn new(app: WeakEntity<AppView>, cx: &mut Context<Self>) -> Self {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(TICK).await;
                let Ok(()) = this.update(cx, |panel, cx| {
                    if panel.visible {
                        panel.refresh(cx);
                    }
                }) else {
                    break;
                };
            }
        })
        .detach();
        Self {
            app,
            dirs: TranscriptDirs::from_system(),
            scanner: Some(Scanner::default()),
            sessions: Arc::new(Vec::new()),
            total_files: 0,
            limit: DEFAULT_LIMIT,
            loaded: false,
            rescan: false,
            error: None,
            visible: false,
            stale: false,
            last_activation: None,
            scope: Scope::default(),
            claude: true,
            codex: true,
            search: None,
            _search_events: None,
            expanded: None,
        }
    }

    /// The tab is on screen (or not). Showing it reads the history again.
    pub(crate) fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.visible == visible {
            return;
        }
        self.visible = visible;
        if visible {
            self.stale = false;
            self.refresh(cx);
        }
    }

    pub(crate) fn on_activated(&mut self, cx: &mut Context<Self>) {
        let now = Instant::now();
        if self.visible && activation_due(self.last_activation, now) {
            self.last_activation = Some(now);
            self.refresh(cx);
        }
    }

    /// An agent finished or changed session: re-read now when visible, else when shown.
    pub(crate) fn agent_activity(&mut self, cx: &mut Context<Self>) {
        if self.visible {
            self.refresh(cx);
        } else {
            self.stale = true;
        }
    }

    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(mut scanner) = self.scanner.take() else {
            self.rescan = true;
            return;
        };
        let (dirs, limit) = (self.dirs.clone(), self.limit);
        cx.spawn(async move |this, cx| {
            let (scanner, result) = cx
                .background_executor()
                .spawn(async move {
                    let result = scanner.scan(&dirs, limit);
                    (scanner, result)
                })
                .await;
            let _ = this.update(cx, |panel, cx| panel.finish_scan(scanner, result, cx));
        })
        .detach();
        cx.notify();
    }

    fn finish_scan(&mut self, scanner: Scanner, result: agent_history::ScanResult, cx: &mut Context<Self>) {
        self.scanner = Some(scanner);
        self.loaded = true;
        self.error = result.error;
        self.total_files = result.total_files;
        if *self.sessions != result.sessions {
            self.sessions = Arc::new(result.sessions);
        }
        let reload = self.expanded.as_ref().and_then(|open| {
            let meta = self.sessions.iter().find(|m| m.path == open.path)?;
            (meta.updated_ms != open.updated_ms).then(|| (meta.path.clone(), meta.updated_ms))
        });
        if let Some((path, updated_ms)) = reload {
            self.load_turns(path, updated_ms, cx);
        }
        if std::mem::take(&mut self.rescan) {
            self.refresh(cx);
        }
        cx.notify();
    }

    fn show_more(&mut self, cx: &mut Context<Self>) {
        self.limit += DEFAULT_LIMIT;
        self.refresh(cx);
    }

    fn toggle_expand(&mut self, meta: &SessionMeta, cx: &mut Context<Self>) {
        if self.expanded.as_ref().is_some_and(|open| open.path == meta.path) {
            self.expanded = None;
            cx.notify();
        } else {
            self.load_turns(meta.path.clone(), meta.updated_ms, cx);
        }
    }

    fn load_turns(&mut self, path: PathBuf, updated_ms: u64, cx: &mut Context<Self>) {
        let keep = self.expanded.take().filter(|open| open.path == path).and_then(|open| open.turns);
        self.expanded = Some(Expanded { path: path.clone(), updated_ms, turns: keep });
        cx.spawn(async move |this, cx| {
            let read = path.clone();
            let turns = cx.background_executor().spawn(async move { agent_history::session_turns(&read, DETAIL_TURNS) }).await;
            let _ = this.update(cx, |panel, cx| {
                if let Some(open) = panel.expanded.as_mut().filter(|open| open.path == path && open.updated_ms == updated_ms) {
                    open.turns = Some(turns);
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn ensure_search(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<InputState> {
        if let Some(search) = &self.search {
            return search.clone();
        }
        let lang = settings::language(cx);
        let state = cx.new(|cx| InputState::new(window, cx).placeholder(tr(lang, Tr::AgentsSearch)));
        self._search_events = Some(cx.subscribe(&state, |_, _, event, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        }));
        self.search = Some(state.clone());
        state
    }
}

// ---------------------------------------------------------------------------------------------
// Actions of a row (they run outside the panel, on the app)
// ---------------------------------------------------------------------------------------------

fn run_menu_action(action: MenuAction, meta: &SessionMeta, app: &WeakEntity<AppView>, window: &mut Window, cx: &mut App) {
    match action {
        MenuAction::OpenTranscript => {
            let path = meta.path.clone();
            let _ = app.update(cx, |app, cx| app.open_file_tab(path, true, window, cx));
        }
        MenuAction::Reveal => crate::os::reveal_in_file_manager(&meta.path),
        MenuAction::CopyId | MenuAction::CopyPath | MenuAction::CopyResume => {
            if let Some(text) = copy_text(action, meta) {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
            }
        }
    }
}

fn open_handoff(meta: &SessionMeta, app: &WeakEntity<AppView>, window: &mut Window, cx: &mut App) {
    let Some(entity) = app.upgrade() else { return };
    let origin = HandoffOrigin::from_session(meta, &entity.read(cx).projects);
    handoff_dialog::open_origin(entity, origin, window, cx);
}

// ---------------------------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------------------------

fn centered(text: String, theme: &Theme) -> AnyElement {
    div().flex_1().w_full().p_4().text_size(px(12.5)).text_color(theme.muted_foreground).child(text).into_any_element()
}

struct RowEnv {
    sessions: Arc<Vec<SessionMeta>>,
    app: WeakEntity<AppView>,
    panel: WeakEntity<AgentsPanel>,
    lang: Language,
    now_ms: u64,
    theme: Theme,
    expanded: Option<PathBuf>,
}

fn render_row(index: usize, live: Option<&LiveSession>, env: &RowEnv, cx: &mut App) -> AnyElement {
    let meta = &env.sessions[index];
    let (theme, lang, muted) = (&env.theme, env.lang, env.theme.muted_foreground);
    let group = SharedString::from(format!("agent-row-{index}"));
    let open = env.expanded.as_ref().is_some_and(|path| *path == meta.path);

    let badge = live.and_then(|l| badge_for(&l.status));
    let glyph = match badge {
        Some(badge) => badge_element(&badge, false, lang, ("agent-badge", index), cx),
        None => agent_icon_element(meta.kind, 14., theme.foreground, muted),
    };

    let mut actions = h_flex().gap_0p5();
    match live {
        Some(live) => {
            let (app, term, worktree) = (env.app.clone(), live.term, live.worktree.clone());
            actions = actions.child(
                Button::new(("agent-goto", index))
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(Ic::SquareTerminal))
                    .tooltip(tr(lang, Tr::AgentsGoToTerminal))
                    .on_click(move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        cx.stop_propagation();
                        let worktree = worktree.clone();
                        let _ = app.update(cx, |app, cx| {
                            app.activate_terminal_in(WorkspaceKey::of(worktree.as_deref()), term, window, cx)
                        });
                    }),
            );
        }
        None => {
            let (app, sessions) = (env.app.clone(), env.sessions.clone());
            let available = resume_available(meta);
            actions = actions.child(
                Button::new(("agent-resume", index))
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(Ic::Play))
                    .disabled(!available)
                    .tooltip(tr(lang, if meta.cwd_exists { Tr::AgentsResume } else { Tr::AgentsResumeNoDir }))
                    .on_click(move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        cx.stop_propagation();
                        let meta = sessions[index].clone();
                        let _ = app.update(cx, |app, cx| app.resume_history_session(&meta, window, cx));
                    }),
            );
        }
    }
    let (app, sessions) = (env.app.clone(), env.sessions.clone());
    actions = actions.child(
        Button::new(("agent-handoff", index))
            .ghost()
            .xsmall()
            .icon(Icon::new(Ic::Send))
            .tooltip(tr(lang, Tr::AgentsHandoff))
            .on_click(move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                cx.stop_propagation();
                open_handoff(&sessions[index], &app, window, cx);
            }),
    );
    let (app, sessions) = (env.app.clone(), env.sessions.clone());
    actions = actions.child(
        Button::new(("agent-more", index))
            .ghost()
            .xsmall()
            .icon(Icon::new(Ic::Ellipsis))
            .tooltip(tr(lang, Tr::MoreActions))
            .dropdown_menu(move |menu, _, _| {
                let mut menu = menu;
                for (action, label) in MENU {
                    let (app, sessions) = (app.clone(), sessions.clone());
                    menu = menu.item(PopupMenuItem::new(tr(lang, label)).on_click(
                        move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                            run_menu_action(action, &sessions[index], &app, window, cx);
                        },
                    ));
                }
                menu
            }),
    );

    let title_line = h_flex()
        .w_full()
        .gap_2()
        .items_center()
        .child(div().flex_1().min_w_0().truncate().text_size(px(12.5)).text_color(theme.foreground).child(meta.display_title().to_owned()))
        .child(div().flex_shrink_0().text_size(px(11.)).text_color(muted).child(relative_time(lang, env.now_ms, meta.updated_ms)));
    let text = v_flex()
        .flex_1()
        .min_w_0()
        .gap(px(2.))
        .child(title_line)
        .child(div().w_full().truncate().text_size(px(11.)).text_color(muted).child(row_subtitle(meta)));

    let (panel, sessions) = (env.panel.clone(), env.sessions.clone());
    let row = h_flex()
        .id(("agent-row", index))
        .group(group.clone())
        .relative()
        .w_full()
        .h(px(ROW_H))
        .px_2()
        .gap_2()
        .items_center()
        .rounded_md()
        .cursor_pointer()
        .child(div().flex_shrink_0().size(px(14.)).flex().items_center().justify_center().child(glyph))
        .child(text)
        .child(
            div()
                .absolute()
                .top(px(6.))
                .right(px(4.))
                .invisible()
                .group_hover(group, |style| style.visible())
                .rounded_md()
                .bg(theme.sidebar)
                .child(actions),
        )
        .on_click(move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
            let meta = sessions[index].clone();
            let _ = panel.update(cx, |panel, cx| panel.toggle_expand(&meta, cx));
        });
    if open {
        row.bg(theme.accent.opacity(0.6)).into_any_element()
    } else {
        row.hover(|style| style.bg(theme.accent.opacity(0.35))).into_any_element()
    }
}

fn render_turn(turn: &Turn, lang: Language, theme: &Theme) -> AnyElement {
    let label = tr(lang, if turn.role == Role::User { Tr::AgentsYou } else { Tr::AgentsAgent });
    v_flex()
        .w_full()
        .child(div().text_size(px(10.5)).text_color(theme.muted_foreground).child(label))
        .child(div().text_size(px(11.5)).text_color(theme.foreground.opacity(0.75)).child(turn.text.clone()))
        .into_any_element()
}

impl AgentsPanel {
    fn render_header(&self, search: &Entity<InputState>, lang: Language, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let mut scopes = h_flex().gap_0p5();
        for (index, scope) in Scope::ALL.into_iter().enumerate() {
            scopes = scopes.child(
                Button::new(("agents-scope", index))
                    .xsmall()
                    .label(tr(lang, scope_label(scope)))
                    .selected(self.scope == scope)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.scope = scope;
                        cx.notify();
                    })),
            );
        }
        let toggle = |id: &'static str, kind: AgentKind, on: bool, cx: &mut Context<Self>| {
            Button::new(id)
                .ghost()
                .xsmall()
                .selected(on)
                .tooltip(handoff::kind_name(kind))
                .child(agent_icon_element(kind, 14., theme.foreground, theme.muted_foreground))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    match kind {
                        AgentKind::Claude => this.claude = !this.claude,
                        _ => this.codex = !this.codex,
                    }
                    cx.notify();
                }))
        };
        let claude = toggle("agents-claude", AgentKind::Claude, self.claude, cx);
        let codex = toggle("agents-codex", AgentKind::Codex, self.codex, cx);
        v_flex()
            .flex_shrink_0()
            .w_full()
            .px_2()
            .pt_2()
            .pb_1()
            .gap_2()
            .child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .items_center()
                    .child(div().flex_1().min_w_0().child(Input::new(search).small().aria_label(tr(lang, Tr::AgentsSearch))))
                    .child(
                        Button::new("agents-refresh")
                            .ghost()
                            .xsmall()
                            .icon(Icon::new(Ic::RefreshCw))
                            .tooltip(tr(lang, Tr::AgentsRefresh))
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.refresh(cx))),
                    ),
            )
            .child(h_flex().w_full().gap_2().items_center().justify_between().child(scopes).child(h_flex().gap_0p5().child(claude).child(codex)))
            .into_any_element()
    }

    fn render_details(&self, lang: Language, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let open = self.expanded.as_ref()?;
        let meta = self.sessions.iter().find(|m| m.path == open.path)?;
        let body = match &open.turns {
            None => div().text_size(px(11.5)).text_color(theme.muted_foreground).child(tr(lang, Tr::AgentsLoadingTurns)).into_any_element(),
            Some(turns) if turns.is_empty() => {
                div().text_size(px(11.5)).text_color(theme.muted_foreground).child(tr(lang, Tr::AgentsNoTurns)).into_any_element()
            }
            Some(turns) => v_flex().w_full().gap_1p5().children(turns.iter().map(|turn| render_turn(turn, lang, theme))).into_any_element(),
        };
        Some(
            v_flex()
                .flex_shrink_0()
                .w_full()
                .px_2()
                .py_1()
                .gap_1()
                .border_t_1()
                .border_color(theme.border)
                .child(
                    h_flex()
                        .w_full()
                        .gap_1()
                        .items_center()
                        .child(div().flex_1().min_w_0().truncate().text_size(px(11.5)).text_color(theme.foreground).child(meta.display_title().to_owned()))
                        .child(
                            Button::new("agents-details-close")
                                .ghost()
                                .xsmall()
                                .icon(Icon::new(Ic::Close))
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.expanded = None;
                                    cx.notify();
                                })),
                        ),
                )
                .child(div().id("agents-turns").w_full().max_h(px(DETAILS_MAX_H)).overflow_y_scroll().child(body))
                .into_any_element(),
        )
    }
}

impl Render for AgentsPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let search = self.ensure_search(window, cx);
        let lang = settings::language(cx);
        let theme = cx.theme().clone();
        let query = search.read(cx).value().to_string();
        let Some(app) = self.app.upgrade() else { return div().into_any_element() };
        let rows = {
            let view = app.read(cx);
            let snap = view.client.snapshot();
            let live = live_sessions(snap.terminals.iter().map(|t| &t.info));
            let filter = Filter {
                scope: self.scope,
                query: &query,
                claude: self.claude,
                codex: self.codex,
                selected: view.ws.selected.path(),
                projects: &view.projects,
            };
            visible_rows(&self.sessions, &live, &filter)
        };

        let count = rows.len();
        let header = self.render_header(&search, lang, &theme, cx);
        let body = if !self.loaded {
            centered(tr(lang, Tr::AgentsScanning).to_owned(), &theme)
        } else if rows.is_empty() {
            centered(tr(lang, Tr::AgentsEmpty).to_owned(), &theme)
        } else {
            let env = RowEnv {
                sessions: self.sessions.clone(),
                app: self.app.clone(),
                panel: cx.weak_entity(),
                lang,
                now_ms: crate::claude_ctx::now_ms(),
                theme: theme.clone(),
                expanded: self.expanded.as_ref().map(|open| open.path.clone()),
            };
            uniform_list("agents-list", count, move |range, _, cx| {
                range.map(|i| render_row(rows[i].0, rows[i].1.as_ref(), &env, cx)).collect::<Vec<_>>()
            })
            .flex_1()
            .min_h_0()
            .w_full()
            .px_1()
            .into_any_element()
        };
        let error = self.error.as_ref().map(|err| {
            div()
                .flex_shrink_0()
                .w_full()
                .px_2()
                .text_size(px(11.))
                .text_color(theme.danger)
                .child(format!("{}: {err}", tr(lang, Tr::AgentsScanError)))
        });
        let more = (self.loaded && self.total_files > self.limit).then(|| {
            Button::new("agents-more")
                .ghost()
                .xsmall()
                .label(tr(lang, Tr::AgentsMore))
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.show_more(cx)))
        });
        let footer = h_flex()
            .flex_shrink_0()
            .w_full()
            .h(px(26.))
            .px_2()
            .items_center()
            .justify_between()
            .border_t_1()
            .border_color(theme.border)
            .text_size(px(11.))
            .text_color(theme.muted_foreground)
            .child(i18n::agents_count(lang, count))
            .children(more);
        v_flex()
            .id("agents-panel")
            .size_full()
            .min_h_0()
            .child(header)
            .children(error)
            .child(body)
            .children(self.render_details(lang, &theme, cx))
            .child(footer)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sich_proto::{AgentStatus, WorktreeInfo};

    fn meta(kind: AgentKind, id: &str, cwd: Option<&str>, exists: bool) -> SessionMeta {
        SessionMeta {
            kind,
            id: id.into(),
            path: PathBuf::from(format!("/sessions/{id}.jsonl")),
            cwd: cwd.map(PathBuf::from),
            cwd_exists: exists,
            branch: Some("feat/x".into()),
            title: Some("A title".into()),
            first_prompt: "first".into(),
            last_prompt: "the last prompt".into(),
            last_assistant: None,
            model: None,
            created_ms: None,
            updated_ms: 5,
        }
    }

    fn project(path: &str, worktrees: &[&str]) -> ProjectInfo {
        ProjectInfo {
            id: Default::default(),
            name: "proj".into(),
            path: PathBuf::from(path),
            is_git: true,
            worktrees: worktrees
                .iter()
                .map(|p| WorktreeInfo { path: PathBuf::from(p), branch: None, head: String::new(), is_main: false, dirty: false })
                .collect(),
            error: None,
            group: None,
        }
    }

    const ID: &str = "11111111-2222-3333-4444-555555555555";

    #[test]
    fn resume_plan_types_the_official_command_in_the_session_folder() {
        let projects = vec![project("/w/app", &["/w/app", "/w/wt/feat"])];
        let home = Path::new("/home/u");
        let known = resume_plan(&meta(AgentKind::Claude, ID, Some("/w/wt/feat"), true), &projects, Some(home), true).unwrap();
        assert_eq!(known.command, format!("claude --resume {ID}"));
        assert_eq!(known.resolved.workspace, WorkspaceKey::Worktree(PathBuf::from("/w/wt/feat")));
        assert_eq!(known.resolved.cwd, Some(PathBuf::from("/w/wt/feat")));
        let codex = resume_plan(&meta(AgentKind::Codex, ID, Some("/w/app"), true), &projects, Some(home), true).unwrap();
        assert_eq!(codex.command, format!("codex resume {ID}"));
    }

    #[test]
    fn resume_in_a_subfolder_opens_in_the_containing_worktree_with_that_folder() {
        let projects = vec![project("/w/app", &["/w/app"])];
        let plan = resume_plan(&meta(AgentKind::Claude, ID, Some("/w/app/pkg/api"), true), &projects, None, true).unwrap();
        assert_eq!(plan.resolved.workspace, WorkspaceKey::Worktree(PathBuf::from("/w/app")));
        assert_eq!(plan.resolved.cwd, Some(PathBuf::from("/w/app/pkg/api")));
    }

    #[test]
    fn resume_in_an_unknown_folder_opens_a_home_terminal_there() {
        let plan = resume_plan(&meta(AgentKind::Claude, ID, Some("/elsewhere/dir"), true), &[], None, true).unwrap();
        assert_eq!(plan.resolved, Resolved { workspace: WorkspaceKey::Home, cwd: Some(PathBuf::from("/elsewhere/dir")) });
    }

    #[test]
    fn resume_is_refused_without_a_folder_or_with_an_unsafe_id() {
        let projects = vec![];
        assert!(resume_plan(&meta(AgentKind::Claude, ID, Some("/gone"), false), &projects, None, false).is_none());
        assert!(resume_plan(&meta(AgentKind::Claude, ID, None, false), &projects, None, true).is_none());
        for bad in ["../x", "-abcdefgh", "short", "has space in it", ""] {
            assert!(resume_plan(&meta(AgentKind::Claude, bad, Some("/w"), true), &projects, None, true).is_none(), "{bad}");
        }
        assert!(resume_plan(&meta(AgentKind::Gemini, ID, Some("/w"), true), &projects, None, true).is_none());
    }

    #[test]
    fn resume_button_state_follows_the_scan() {
        assert!(resume_available(&meta(AgentKind::Claude, ID, Some("/w"), true)));
        assert!(!resume_available(&meta(AgentKind::Claude, ID, Some("/w"), false)));
        assert!(!resume_available(&meta(AgentKind::Claude, "../x", Some("/w"), true)));
    }

    #[test]
    fn subtitle_is_the_last_prompt_and_the_branch() {
        let m = meta(AgentKind::Claude, ID, None, false);
        assert_eq!(row_subtitle(&m), "the last prompt \u{b7} feat/x");
        assert_eq!(row_subtitle(&SessionMeta { branch: None, ..m.clone() }), "the last prompt");
        assert_eq!(row_subtitle(&SessionMeta { branch: Some(String::new()), last_prompt: String::new(), ..m }), "");
    }

    #[test]
    fn copy_actions_produce_the_right_text() {
        let m = meta(AgentKind::Codex, ID, Some("/w"), true);
        assert_eq!(copy_text(MenuAction::CopyId, &m), Some(ID.to_owned()));
        assert_eq!(copy_text(MenuAction::CopyPath, &m), Some(m.path.display().to_string()));
        assert_eq!(copy_text(MenuAction::CopyResume, &m), Some(format!("codex resume {ID}")));
        assert_eq!(copy_text(MenuAction::OpenTranscript, &m), None);
        assert_eq!(copy_text(MenuAction::Reveal, &m), None);
        assert_eq!(copy_text(MenuAction::CopyResume, &meta(AgentKind::Claude, "../x", None, false)), None);
        let order: Vec<MenuAction> = MENU.iter().map(|(a, _)| *a).collect();
        assert_eq!(
            order,
            [MenuAction::OpenTranscript, MenuAction::Reveal, MenuAction::CopyId, MenuAction::CopyPath, MenuAction::CopyResume]
        );
    }

    #[test]
    fn activation_refreshes_at_most_every_ten_seconds() {
        let t0 = Instant::now();
        assert!(activation_due(None, t0));
        assert!(!activation_due(Some(t0), t0 + Duration::from_secs(9)));
        assert!(activation_due(Some(t0), t0 + Duration::from_secs(10)));
        assert!(!activation_due(Some(t0 + Duration::from_secs(5)), t0));
    }

    fn seen(session: Option<&str>, settled: bool) -> SeenAgent {
        SeenAgent { session_id: session.map(str::to_owned), settled }
    }

    #[test]
    fn history_is_re_read_when_an_agent_settles_or_changes_session() {
        let working = seen(Some("a"), false);
        let done = seen(Some("a"), true);
        assert!(history_changed(Some(&working), Some(&done)));
        assert!(!history_changed(Some(&done), Some(&done)));
        assert!(!history_changed(Some(&done), Some(&working)));
        assert!(history_changed(Some(&seen(Some("a"), false)), Some(&seen(Some("b"), false))));
        assert!(history_changed(Some(&seen(None, false)), Some(&seen(Some("b"), false))));
        assert!(!history_changed(Some(&seen(Some("a"), false)), Some(&seen(None, false))), "a lost id is not news");
        assert!(!history_changed(None, Some(&done)), "the first sight only seeds");
        assert!(!history_changed(Some(&working), None));
    }

    #[test]
    fn terminals_are_seen_by_their_agent_state() {
        let mut info = TerminalInfo {
            id: 1,
            title: String::new(),
            cwd: PathBuf::new(),
            cols: 80,
            rows: 24,
            exited: None,
            created_unix_ms: 0,
            worktree: None,
            agent: None,
        };
        assert_eq!(seen_agent(&info), None);
        let status = |state, session: Option<&str>| AgentStatus {
            kind: AgentKind::Claude,
            state,
            unread: false,
            since_unix_ms: 0,
            detail: None,
            model: None,
            title: None,
            prompt: None,
            message: None,
            subagents: Vec::new(),
            session_id: session.map(str::to_owned),
        };
        info.agent = Some(status(AgentState::Working, Some("s")));
        assert_eq!(seen_agent(&info), Some(seen(Some("s"), false)));
        for (state, settled) in [(AgentState::Idle, true), (AgentState::Done, true), (AgentState::Waiting, false), (AgentState::Failed, false)] {
            info.agent = Some(status(state, None));
            assert_eq!(seen_agent(&info), Some(seen(None, settled)), "{state:?}");
        }
    }

    #[test]
    fn rows_pair_sessions_with_their_live_terminal_of_the_same_agent() {
        let sessions = vec![meta(AgentKind::Claude, "c1", Some("/w"), true), meta(AgentKind::Codex, "x1", Some("/w"), true)];
        let status = |kind| AgentStatus {
            kind,
            state: AgentState::Working,
            unread: false,
            since_unix_ms: 0,
            detail: None,
            model: None,
            title: None,
            prompt: None,
            message: None,
            subagents: Vec::new(),
            session_id: None,
        };
        let live = std::collections::HashMap::from([
            ("c1".to_owned(), LiveSession { term: 7, worktree: None, status: status(AgentKind::Claude) }),
            ("x1".to_owned(), LiveSession { term: 8, worktree: None, status: status(AgentKind::Claude) }),
        ]);
        let filter = Filter { scope: Scope::All, query: "", claude: true, codex: true, selected: None, projects: &[] };
        let rows = visible_rows(&sessions, &live, &filter);
        assert_eq!(rows.len(), 2);
        assert_eq!((rows[0].0, rows[0].1.as_ref().map(|l| l.term)), (0, Some(7)));
        assert_eq!((rows[1].0, rows[1].1.as_ref().map(|l| l.term)), (1, None), "a terminal of another agent is not this session");
        let only_codex = Filter { claude: false, ..filter };
        assert_eq!(visible_rows(&sessions, &live, &only_codex).iter().map(|r| r.0).collect::<Vec<_>>(), vec![1]);
    }

    #[test]
    fn session_count_uses_the_right_plural() {
        let en = |n| i18n::agents_count(Language::En, n);
        assert_eq!((en(0), en(1), en(2)), ("0 sessions".to_owned(), "1 session".to_owned(), "2 sessions".to_owned()));
        let uk = |n| i18n::agents_count(Language::Uk, n);
        assert_eq!(uk(1), "1 сесія");
        assert_eq!(uk(3), "3 сесії");
        assert_eq!(uk(5), "5 сесій");
        assert_eq!(uk(11), "11 сесій");
        assert_eq!(uk(12), "12 сесій");
        assert_eq!(uk(21), "21 сесія");
        assert_eq!(uk(0), "0 сесій");
        let ru = |n| i18n::agents_count(Language::Ru, n);
        assert_eq!((ru(1), ru(2), ru(5), ru(14), ru(22)), ("1 сессия".to_owned(), "2 сессии".to_owned(), "5 сессий".to_owned(), "14 сессий".to_owned(), "22 сессии".to_owned()));
    }

    #[test]
    fn scope_labels_are_distinct_and_translated() {
        let labels: Vec<Tr> = Scope::ALL.into_iter().map(scope_label).collect();
        assert_eq!(labels, [Tr::AgentsScopeWorktree, Tr::AgentsScopeProject, Tr::AgentsScopeAll]);
        assert_eq!(tr(Language::En, Tr::AgentsTab), "Agents");
        assert_eq!(tr(Language::Uk, Tr::AgentsTab), "Агенти");
        assert_eq!(tr(Language::Ru, Tr::AgentsTab), "Агенты");
        assert_eq!(Scope::default(), Scope::Worktree);
    }

    #[test]
    fn row_height_is_constant() {
        assert_eq!(ROW_H, 44.);
    }
}
