//! Main window: projects, terminal tabs, right panel, and status line.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::assets::IconName as Ic;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::tooltip::Tooltip;
use futures::{FutureExt as _, StreamExt as _};
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::{
    AppContext as _, ClickEvent, Context, Entity, ExternalPaths, InteractiveElement as _, IntoElement,
    CursorStyle, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement as _, Render,
    PlatformInput, ScrollHandle, StatefulInteractiveElement as _, Styled as _, Window, WindowBounds,
    div, px,
};
use sich_git::Repo;
use sich_git::compare::CompareKey;
use sich_orch::auto_close::{AutoClose, countdown_seconds, due_for_close};
use sich_proto::{GroupId, GroupInfo, PROTO_VERSION, ProjectId, ProjectInfo, SubagentActivityKey, TermId, TerminalInfo};

use crate::agent_badge::{BadgeKind, kind_of, should_flash, should_mark_seen};
use crate::daemon_client::{ConnStatus, DEFAULT_COLS, DEFAULT_ROWS, DaemonPool, RestartResult, RestoreData, Snapshot};
use crate::diff_view::DiffView;
use crate::git_panel::{DEFAULT_RIGHT_W, GitPanel, GitPanelEvent, MIN_RIGHT_W, clamp_right_w};
use crate::i18n::{self, Tr, tr};
use crate::notifications;
use crate::notify::{self, Cooldown, NotifyKind};
use crate::quick_commands::{self, TemporaryCommand};
use crate::orch_watch;
use crate::sidebar::{DEFAULT_SIDEBAR_W, clamp_sidebar_w};
use crate::session::{self, RightPanelState, RightTab, SavedWindow, Session, SessionInput};
use crate::settings;
use crate::settings_dialog;
use crate::subagent_activity::{ActivityTarget, ActivityView};
use crate::tray::{self, Tray, UiEvent};
use crate::update_dialog::{self, UpdateFlow};
use crate::update_status::{self, CheckStatus};
use crate::updater;
use crate::view::TerminalView;
use crate::terminal_layout::{self, Axis, Edge, Layout, Rect};
use crate::split_drag::{DragGhost, SeparatorDrag, TerminalDrag};
use crate::workspace::{
    WorkspaceKey, Workspaces, cycle_id, restore_selection, terminals_in, workspace_exists,
};
use crate::workspace_tabs::{WorkspaceTab, WorkspaceTabs};
use crate::win_flash;
use crate::win_show;
use crate::win_toast;
use crate::worktree_dialogs;

/// How often the sidebar is repainted while agents are present, to refresh relative times.
const AGE_REFRESH: Duration = Duration::from_secs(30);

/// Upper bound for creations waiting to be matched with a `TerminalCreated`.
const MAX_PENDING_CREATES: usize = 8;

/// Delay before a changed session is written to disk.
const SESSION_SAVE_DEBOUNCE: Duration = Duration::from_millis(500);

/// How long a session restore may take before it stops blocking session saves.
const RESTORE_TIMEOUT: Duration = Duration::from_secs(15);

fn countdown_changed(state: Option<AutoClose>, previous_ms: i64, now_ms: i64) -> bool {
    matches!(state, Some(AutoClose::ClosesAt(deadline))
        if countdown_seconds(deadline.saturating_sub(previous_ms))
            != countdown_seconds(deadline.saturating_sub(now_ms)))
}

/// Terminals of one workspace being recreated from the saved session.
struct RestoreTarget {
    key: WorkspaceKey,
    tokens: Vec<u64>,
    layout: Option<Layout<session::RestoreLeaf>>,
    active: Option<usize>,
    attached_active: Option<TermId>,
}

#[derive(Clone, Copy)]
enum TerminalOpenKind {
    Split { target: TermId, edge: Edge },
    QuickTab,
}

struct PendingTerminalOpen {
    token: u64,
    workspace: WorkspaceKey,
    kind: TerminalOpenKind,
    started: Instant,
}

#[derive(PartialEq, Eq)]
pub(crate) enum TabKey {
    Diff(CompareKey),
    Activity(ActivityTarget),
}

pub(crate) enum TabView {
    Diff(Entity<DiffView>),
    Activity(Entity<ActivityView>),
}

pub struct AppView {
    pub(crate) client: DaemonPool,
    views: HashMap<TermId, Entity<TerminalView>>,
    terminal_layouts: HashMap<WorkspaceKey, Layout<TermId>>,
    dragged_terminal: Option<TermId>,
    workspace_tabs: HashMap<WorkspaceKey, WorkspaceTabs<TabKey, TabView>>,
    next_tab_id: u64,
    focused_tab: Option<u64>,
    activity_rt: tokio::runtime::Handle,
    pub(crate) tab_scroll: ScrollHandle,
    pub(crate) ws: Workspaces,
    /// Saved workspace to restore when the first `Projects` update arrives.
    restore: Option<Option<PathBuf>>,
    pub(crate) projects: Vec<ProjectInfo>,
    pub(crate) worktree_visibility: crate::worktree_visibility::Store,
    pub(crate) groups: Vec<GroupInfo>,
    /// Whether the Home terminal list is collapsed.
    pub(crate) home_collapsed: bool,
    /// Groups whose project list is collapsed (default: expanded).
    pub(crate) collapsed_groups: HashSet<GroupId>,
    /// Projects whose tree is collapsed (default: expanded).
    pub(crate) collapsed: HashSet<ProjectId>,
    /// Workspaces this window asked a terminal for; the matching new terminal gets activated.
    pending_creates: VecDeque<WorkspaceKey>,
    pending_terminal_opens: Vec<PendingTerminalOpen>,
    pending_terminal_restarts: HashMap<TermId, u64>,
    pub(crate) quick_terminals: HashMap<TermId, TemporaryCommand>,
    quick_return: HashMap<WorkspaceKey, TermId>,
    seen_projects_rev: u64,
    shown_error: Option<Instant>,
    /// Terminal that last received keyboard focus.
    focused: Option<TermId>,
    pub(crate) usage_bar: crate::usage_bar::UsageBar,
    /// Whether the window has focus (kept by the activation observer and `sync`).
    window_active: bool,
    /// Native window handle for the taskbar flash (Windows).
    hwnd: Option<isize>,
    /// Last badge kind seen per terminal, to detect transitions into "needs the user".
    attention: HashMap<TermId, Option<BadgeKind>>,
    toast_cooldown: Cooldown,
    /// Toasts to clear when a terminal becomes visible.
    unseen_toasts: HashSet<TermId>,
    /// `since_unix_ms` of the status for which `mark_seen` was already sent, per terminal.
    seen_sent: HashMap<TermId, u64>,
    events_tx: UnboundedSender<UiEvent>,
    events_rx: Option<UnboundedReceiver<UiEvent>>,
    tray: Option<Tray>,
    tray_lang: i18n::Language,
    /// Session loaded at startup, consumed by the restore.
    saved: Option<Session>,
    /// Whether the terminal restore decision was made (once per run).
    restored: bool,
    restore_targets: Vec<RestoreTarget>,
    next_restore_token: u64,
    restore_started: Option<Instant>,
    window_state: Option<SavedWindow>,
    last_session: Option<Session>,
    pending_save: Option<Session>,
    save_scheduled: bool,
    restarting_daemon: bool,
    restart_dialog_shown: bool,
    /// Orchestration data for the sidebar and banner, refreshed by the watcher.
    pub(crate) orch: sich_orch::OrchSnapshot,
    orch_now_ms: i64,
    /// Sidebar width in logical pixels (drag the handle on its right edge).
    pub(crate) sidebar_w: f32,
    /// The sidebar handle is being dragged.
    sidebar_dragging: bool,
    git_panel: Entity<GitPanel>,
    pub(crate) right_panel: RightPanelState,
    right_dragging: bool,
    hidden_to_tray: bool,
    /// Newer release found by an update check; retained if the toast is dismissed.
    update_available: Option<updater::Release>,
    update_flow: Option<Arc<UpdateFlow>>,
}

impl AppView {
    pub fn new(
        rt: tokio::runtime::Handle,
        socket: String,
        events: (UnboundedSender<UiEvent>, UnboundedReceiver<UiEvent>),
        saved: Option<Session>,
        cx: &mut Context<Self>,
    ) -> Self {
        let sidebar_w = saved
            .as_ref()
            .and_then(|s| s.sidebar_w)
            .map_or(DEFAULT_SIDEBAR_W, |w| clamp_sidebar_w(w, f32::INFINITY));
        let mut right_panel = saved.as_ref().map_or_else(RightPanelState::default, |s| s.right_panel);
        right_panel.width = right_panel.width.map(|width| clamp_right_w(width, f32::INFINITY, 0.));
        let git_panel = cx.new(|_| GitPanel::new(right_panel.history_collapsed, right_panel.history_height));
        // Coalescing wake path: bounded(1), try_send never blocks.
        let (wake_tx, wake_rx) = async_channel::bounded::<()>(1);
        let client = DaemonPool::start(
            &rt,
            socket.clone(),
            Arc::new(move || {
                let _ = wake_tx.try_send(());
            }),
        );
        cx.spawn(async move |this, cx| {
            while wake_rx.recv().await.is_ok() {
                let alive = this.update(cx, |this, cx| this.on_wake(cx));
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();

        // Relative times in the sidebar ("5m") tick even when nothing else changes.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(AGE_REFRESH).await;
                let alive = this.update(cx, |this, cx| {
                    let snap = this.client.snapshot();
                    if snap.terminals.iter().any(|t| t.info.agent.is_some()) {
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();

        // The watcher delivers mail, raises escalations, closes workers and cleans up merged worktrees.
        let orch_socket = socket;
        let worktree_socket = client.primary_socket().to_owned();
        let orch_rt = rt.clone();
        cx.spawn(async move |this, cx| {
            let mut watcher = Some(sich_orch::watch::Watcher::new(sich_orch::paths::db_path()));
            let mut closing: Option<gpui_kit::Task<()>> = None;
            let mut cleanup: Option<gpui_kit::Task<()>> = None;
            let mut last_cleanup: Option<Instant> = None;
            loop {
                cx.background_executor().timer(orch_watch::TICK).await;
                let Ok((data, auto_close_workers)) = this.update(cx, |this, cx| {
                    (this.orch_tick_data(), settings::get(cx).orch_auto_close_workers)
                }) else { break };
                let Some(mut w) = watcher.take() else { break };
                let legacy_socket = orch_socket.clone();
                let (w, mut out) = cx
                    .background_executor()
                    .spawn(async move {
                        let input = sich_orch::watch::TickInput {
                            terms: &data.terms,
                            synced: data.synced,
                            auto_close_workers,
                            now_ms: data.now_ms,
                            exe: &data.exe,
                        };
                        let routes = sich_orch::watch::TerminalRoutes { terminals: &data.routes, legacy_socket: &legacy_socket };
                        let out = w.tick_routed(&input, Some(&routes));
                        (w, out)
                    })
                    .await;
                watcher = Some(w);
                let due = std::mem::take(&mut out.due);
                let Ok((should_close, cleanup_terminals)) = this.update(cx, |this, cx| {
                    this.apply_orch(out, cx);
                    let synced = this.client.terminals_synced();
                    let settings = settings::get(cx);
                    let cleanup = (synced && settings.orch_cleanup_worktrees && cleanup.is_none()
                        && last_cleanup.is_none_or(|t| t.elapsed() >= orch_watch::WORKTREE_CLEANUP_INTERVAL))
                        .then(|| this.client.snapshot().terminals.into_iter().map(|t| t.info).collect::<Vec<_>>());
                    (synced && settings.orch_auto_close_workers, cleanup)
                }) else { break };
                if closing.as_mut().is_some_and(|close| close.now_or_never().is_some()) {
                    closing = None;
                }
                if should_close && !due.is_empty() && closing.is_none() {
                    let socket = orch_socket.clone();
                    let rt = orch_rt.clone();
                    closing = Some(cx.background_executor().spawn(async move {
                        orch_watch::release_due(&due, &socket, &rt);
                    }));
                }
                if cleanup.as_mut().is_some_and(|task| task.now_or_never().is_some()) {
                    cleanup = None;
                }
                if let Some(terminals) = cleanup_terminals {
                    last_cleanup = Some(Instant::now());
                    let socket = worktree_socket.clone();
                    let rt = orch_rt.clone();
                    cleanup = Some(cx.background_executor().spawn(async move {
                        orch_watch::cleanup_worktrees(&socket, &terminals, &rt);
                    }));
                }
            }
        })
        .detach();

        Self {
            client,
            views: HashMap::new(),
            terminal_layouts: HashMap::new(),
            dragged_terminal: None,
            workspace_tabs: HashMap::new(),
            next_tab_id: 0,
            focused_tab: None,
            activity_rt: rt,
            tab_scroll: ScrollHandle::new(),
            ws: Workspaces::default(),
            // The session's selection wins over the older `last_workspace` setting.
            restore: Some(match &saved {
                Some(s) => s.selected.clone(),
                None => settings::get(cx).last_workspace,
            }),
            projects: Vec::new(),
            worktree_visibility: Default::default(),
            groups: Vec::new(),
            home_collapsed: saved.as_ref().is_some_and(|s| s.home_collapsed),
            collapsed_groups: HashSet::new(),
            collapsed: HashSet::new(),
            pending_creates: VecDeque::new(),
            pending_terminal_opens: Vec::new(),
            pending_terminal_restarts: HashMap::new(),
            quick_terminals: HashMap::new(),
            quick_return: HashMap::new(),
            seen_projects_rev: 0,
            shown_error: None,
            focused: None,
            usage_bar: crate::usage_bar::UsageBar::default(),
            window_active: false,
            hwnd: None,
            attention: HashMap::new(),
            toast_cooldown: Cooldown::default(),
            unseen_toasts: HashSet::new(),
            seen_sent: HashMap::new(),
            events_tx: events.0,
            events_rx: Some(events.1),
            tray: None,
            tray_lang: settings::language(cx),
            saved,
            restored: false,
            restore_targets: Vec::new(),
            next_restore_token: 0,
            restore_started: None,
            window_state: None,
            last_session: None,
            pending_save: None,
            save_scheduled: false,
            restarting_daemon: false,
            restart_dialog_shown: false,
            orch: sich_orch::OrchSnapshot::default(),
            orch_now_ms: sich_orch::paths::now_ms(),
            sidebar_w,
            sidebar_dragging: false,
            git_panel,
            right_panel,
            right_dragging: false,
            hidden_to_tray: false,
            update_available: None,
            update_flow: None,
        }
    }

    fn orch_tick_data(&self) -> orch_watch::TickData {
        let snap = self.client.snapshot();
        let infos: Vec<TerminalInfo> = snap.terminals.iter().map(|t| t.info.clone()).collect();
        let exe = crate::daemon_host::cli_exe().map_or_else(|| "sich-cli".to_string(), |p| p.to_string_lossy().into_owned());
        let mut data = orch_watch::tick_data(&infos, self.client.terminals_synced(), sich_orch::paths::now_ms(), exe, Instant::now());
        for info in &infos {
            if let Some(owner) = self.client.owner(info.id) {
                data.routes.entry(owner.socket).or_default().insert(owner.id, info.id);
            }
        }
        data
    }

    /// Applies one watcher result: sidebar data, and wake-up pointers typed like a user would
    /// (a bracketed paste, then Enter a moment later).
    fn apply_orch(&mut self, out: sich_orch::watch::TickOutput, cx: &mut Context<Self>) {
        let now_ms = sich_orch::paths::now_ms();
        let enabled = settings::get(cx).orch_auto_close_workers;
        self.tick_activity_tabs(enabled, now_ms, cx);
        let quick_countdown_changed = !self.hidden_to_tray && self.active_tab().is_none()
            && self.quick_terminals.values().any(|command| {
                countdown_changed(command.auto_close_state(enabled), self.orch_now_ms, now_ms)
            });
        if !self.quick_terminals.is_empty() {
            self.tick_quick_terminals(&self.client.snapshot(), enabled, now_ms);
        }
        let worker_state = if !self.hidden_to_tray && self.active_tab().is_none() {
            self.focused
                .and_then(|id| self.orch.workers.get(&id))
                .and_then(|worker| worker.auto_close_state(enabled))
        } else {
            None
        };
        let worker_countdown_changed = countdown_changed(worker_state, self.orch_now_ms, now_ms);
        self.orch_now_ms = now_ms;
        let snapshot_changed = out.snapshot.is_some();
        if let Some(snapshot) = out.snapshot {
            self.orch = snapshot;
        }
        if snapshot_changed || worker_countdown_changed || quick_countdown_changed {
            cx.notify();
        }
        for pointer in out.pointers {
            self.client.send_input(pointer.term, orch_watch::bracketed_paste(&pointer.text));
            let (client, term) = (self.client.clone(), pointer.term);
            cx.spawn(async move |_, cx| {
                cx.background_executor().timer(orch_watch::ENTER_DELAY).await;
                client.send_input(term, b"\r".to_vec());
            })
            .detach();
        }
    }

    fn tick_quick_terminals(&mut self, snap: &Snapshot, enabled: bool, now_ms: i64) {
        if self.quick_terminals.is_empty() || !self.client.terminals_synced() {
            return;
        }
        self.quick_terminals.retain(|&id, command| {
            let Some(entry) = snap.terminals.iter().find(|entry| entry.info.id == id) else { return false };
            command.observe(entry.info.exited.is_some(), now_ms);
            if due_for_close(command.auto_close_state(enabled), now_ms) {
                self.client.kill_terminal(id);
                false
            } else {
                true
            }
        });
    }

    fn tick_activity_tabs(&mut self, enabled: bool, now_ms: i64, cx: &mut Context<Self>) {
        if !enabled {
            return;
        }
        let mut selected_changed = false;
        let mut active_changed = false;
        for (workspace, tabs) in &mut self.workspace_tabs {
            let previous = tabs.active;
            let removed = tabs.close_where(|tab| match &tab.view {
                TabView::Activity(view) => due_for_close(view.read(cx).auto_close_state(enabled), now_ms),
                TabView::Diff(_) => false,
            });
            if removed && workspace == &self.ws.selected {
                selected_changed = true;
                active_changed = tabs.active != previous;
            }
        }
        if active_changed {
            self.sync_diff_highlight(cx);
        }
        if selected_changed {
            cx.notify();
        }
        if !self.hidden_to_tray
            && let Some(TabView::Activity(view)) = self.active_tab().map(|tab| &tab.view)
            && countdown_changed(view.read(cx).auto_close_state(enabled), self.orch_now_ms, now_ms)
        {
            view.update(cx, |_, cx| cx.notify());
        }
    }

    fn on_wake(&mut self, cx: &mut Context<Self>) {
        self.finish_daemon_restart();
        for view in self.views.values() {
            view.update(cx, |v, cx| v.refresh(cx));
        }
        let snap = self.client.snapshot();
        self.finish_restore_targets(&snap);
        let prefs = settings::get(cx);
        self.track_agents(&snap, &prefs);
        // The window may be hidden in the tray (no renders), so keep these going from here too.
        let infos: Vec<TerminalInfo> = snap.terminals.iter().map(|t| t.info.clone()).collect();
        self.tick_quick_terminals(&snap, prefs.orch_auto_close_workers, sich_orch::paths::now_ms());
        self.update_tray(&infos, cx);
        self.touch_session(&infos, cx);
        cx.notify();
    }

    /// Hooks the window up: focus tracking (mark seen on focus, flash only in the background),
    /// the native handle (taskbar flash, hide to tray), close interception, the tray icon and
    /// window-bounds tracking for the session.
    pub fn watch_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.usage_bar.watch(window, cx);
        Self::start_update_checks(window, cx);
        cx.observe(&self.git_panel, |this, panel, cx| {
            let panel = panel.read(cx);
            let collapsed = panel.history_collapsed();
            let height = panel.history_height();
            if this.right_panel.history_collapsed != collapsed || this.right_panel.history_height != height {
                this.right_panel.history_collapsed = collapsed;
                this.right_panel.history_height = height;
                this.sidebar_changed(cx);
            }
        }).detach();
        cx.subscribe_in(&self.git_panel, window, |this, _, event, window, cx| {
            this.on_git_panel_event(event, window, cx);
        }).detach();
        self.window_active = window.is_window_active();
        self.hwnd = win_flash::hwnd_of(window);
        self.window_state = saved_window_of(window, cx);

        let lang = settings::language(cx);
        match Tray::new(lang, &tray::tray_tooltip(lang, 0, 0), self.events_tx.clone()) {
            Ok(t) => self.tray = Some(t),
            Err(e) => tracing::warn!("no tray icon: {e:#}"),
        }
        self.tray_lang = lang;

        let this = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| {
            if this.upgrade().is_none() {
                return true;
            }
            // Decide outside the platform callback; the window never closes by itself.
            let this = this.clone();
            window.defer(cx, move |window, cx| {
                let _ = this.update(cx, |this, cx| this.on_close_requested(window, cx));
            });
            false
        });
        cx.observe_window_bounds(window, |this, window, cx| {
            this.window_state = saved_window_of(window, cx).or(this.window_state);
            let infos: Vec<TerminalInfo> =
                this.client.snapshot().terminals.iter().map(|t| t.info.clone()).collect();
            this.touch_session(&infos, cx);
        })
        .detach();

        if let Some(mut rx) = self.events_rx.take() {
            cx.spawn_in(window, async move |this, cx| {
                while let Some(ev) = rx.next().await {
                    let alive = this.update_in(cx, |this, window, cx| this.on_ui_event(ev, window, cx));
                    if alive.is_err() {
                        break;
                    }
                }
            })
            .detach();
        }
        cx.observe_window_activation(window, |this, window, cx| {
            let active = window.is_window_active();
            if active {
                this.git_panel.update(cx, GitPanel::refresh);
            }
            this.window_active = active;
            let snap = this.client.snapshot();
            this.track_agents(&snap, &settings::get(cx));
            cx.notify();
        })
        .detach();
    }

    /// Flash and notify on attention transitions, then mark the visible terminal seen.
    fn track_agents(&mut self, snap: &Snapshot, prefs: &settings::AppSettings) {
        let infos: Vec<TerminalInfo> = snap.terminals.iter().map(|t| t.info.clone()).collect();
        let ws_ids = self.workspace_ids(&infos);
        let active = if self.active_tab().is_none() { self.ws.active_in(&self.ws.selected, &ws_ids) } else { None };
        let window_active = self.window_active && !self.hidden_to_tray;
        let mut flash = false;
        let instant = Instant::now();
        for t in &snap.terminals {
            let id = t.info.id;
            let status = t.info.agent.as_ref();
            let now = kind_of(status);
            // The first snapshot of a terminal only seeds its state.
            if let Some(prev) = self.attention.insert(id, now) {
                flash |= !self.window_active && should_flash(prev, now);
                if cfg!(windows)
                    && let Some(kind) = notify::notify_kind(prev, now)
                    && notify::should_notify(kind, prefs, window_active, ws_ids.contains(&id))
                    && self.toast_cooldown.allow(id, instant)
                {
                    self.notify_agent(kind, &t.info, prefs);
                    self.unseen_toasts.insert(id);
                }
            }
            let visible = window_active && active == Some(id);
            let mut clear_toast = visible && self.unseen_toasts.remove(&id);
            if should_mark_seen(active == Some(id), window_active, status) {
                let since = status.map_or(0, |s| s.since_unix_ms);
                if self.seen_sent.get(&id) != Some(&since) {
                    self.seen_sent.insert(id, since);
                    self.client.mark_seen(id);
                    clear_toast = true;
                }
            }
            if clear_toast {
                win_toast::remove(id);
            }
        }
        let alive: HashSet<TermId> = infos.iter().map(|t| t.id).collect();
        self.attention.retain(|id, _| alive.contains(id));
        self.seen_sent.retain(|id, _| alive.contains(id));
        self.toast_cooldown.retain(|id| alive.contains(&id));
        self.unseen_toasts.retain(|id| alive.contains(id));
        if flash {
            if let Some(hwnd) = self.hwnd {
                win_flash::flash(hwnd);
            }
        }
    }

    fn notify_agent(&self, kind: NotifyKind, info: &TerminalInfo, prefs: &settings::AppSettings) {
        let project = notify::project_label(info, &self.projects);
        let (title, body) = notify::toast_text(kind, info, &project, prefs.language);
        let xml = notify::toast_xml(&title, &body, !prefs.notify_sound);
        win_toast::show(&xml, Some(info.id), self.events_tx.clone());
    }

    pub(crate) fn test_notification(&self, prefs: &settings::AppSettings) {
        let xml = notify::toast_xml(
            tr(prefs.language, Tr::TestNotification),
            tr(prefs.language, Tr::TestNotificationBody),
            !prefs.notify_sound,
        );
        win_toast::show(&xml, None, self.events_tx.clone());
    }

    // ---- tray, quit ------------------------------------------------------------------------

    fn on_ui_event(&mut self, ev: UiEvent, window: &mut Window, cx: &mut Context<Self>) {
        match ev {
            UiEvent::Show => self.show_window(cx),
            #[cfg(windows)]
            UiEvent::FocusTerminal(id) => {
                let snap = self.client.snapshot();
                if let Some(entry) = snap.terminals.iter().find(|entry| entry.info.id == id) {
                    let key = WorkspaceKey::of(entry.info.worktree.as_deref());
                    self.show_window(cx);
                    self.activate_terminal_in(key, id, window, cx);
                }
            }
            UiEvent::Takeover if !self.restarting_daemon => {
                match self.save_current_session() {
                    Ok(()) => self.quit_ui_only(cx),
                    Err(err) => notifications::show_error(format!("{err:#}"), window, cx),
                }
            }
            UiEvent::Restart if !self.restarting_daemon => {
                if let Err(err) = self.restart_ui() {
                    self.show_window(cx);
                    notifications::show_error(format!("{err:#}"), window, cx);
                }
            }
            UiEvent::Takeover | UiEvent::Restart => {}
            UiEvent::Quit => self.request_quit(window, cx),
        }
    }

    /// Saves the session and starts a replacement UI (it takes over this one).
    pub(crate) fn restart_ui(&mut self) -> anyhow::Result<()> {
        self.save_current_session().and_then(|()| crate::cli::spawn_replacement_ui())
    }

    // ---- updates -----------------------------------------------------------------------------

    /// Periodic release check (official builds only). Errors are logged and ignored.
    fn start_update_checks(window: &mut Window, cx: &mut Context<Self>) {
        if !updater::OFFICIAL {
            return;
        }
        if let Some(dir) = updater::install_dir() {
            cx.background_executor().spawn(async move { updater::cleanup_old(&dir) }).detach();
        }
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(updater::FIRST_CHECK_DELAY).await;
            loop {
                let wanted = this
                    .update(cx, |this, cx| {
                        settings::get(cx).check_updates && this.update_flow.is_none()
                    })
                    .unwrap_or(false);
                if wanted {
                    let found = cx.background_executor().spawn(fetch_latest_now()).await;
                    if let Err(err) = &found {
                        tracing::debug!("update check failed: {err:#}");
                    }
                    if this.update_in(cx, |this, window, cx| {
                        if this.apply_check(found, window, cx)
                            && let Some(release) = &this.update_available
                        {
                            notifications::show_update(cx.weak_entity(), &release.version, window, cx);
                        }
                    }).is_err() {
                        break;
                    }
                }
                cx.background_executor().timer(updater::CHECK_INTERVAL).await;
            }
        })
        .detach();
    }

    /// Keep the last known release after a failed check; an unchanged version does not re-toast.
    fn apply_check(
        &mut self,
        result: anyhow::Result<Option<updater::Release>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let status = match &result {
            Ok(Some(r)) => CheckStatus::Available(r.version.to_string()),
            Ok(None) => CheckStatus::UpToDate,
            Err(err) => CheckStatus::Failed(format!("{err:#}")),
        };
        update_status::set(status, cx);
        let Ok(release) = result else { return false };
        if self.update_flow.is_some() {
            return false;
        }
        if release.is_none() {
            notifications::clear_update(window, cx);
        }
        record_release(&mut self.update_available, release)
    }

    /// Manual "check now" from settings. Works regardless of the auto-check toggle.
    pub(crate) fn check_updates_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !updater::OFFICIAL {
            return;
        }
        if self.update_flow.is_some() {
            window.close_dialog(cx);
            // Deferred: `open` updates AppView, which is already leased here (re-entrancy panic).
            let app = cx.entity();
            window.defer(cx, move |window, cx| update_dialog::open(app, window, cx));
            return;
        }
        if update_status::get(cx) == CheckStatus::Checking {
            return;
        }
        update_status::set(CheckStatus::Checking, cx);
        cx.spawn_in(window, async move |this, cx| {
            let found = cx.background_executor().spawn(fetch_latest_now()).await;
            let _ = this.update_in(cx, |this, window, cx| {
                let has_release = matches!(&found, Ok(Some(_)));
                this.apply_check(found, window, cx);
                if has_release {
                    // Dialogs can't coexist: close settings, then show the update dialog.
                    window.close_dialog(cx);
                    let app = cx.entity();
                    window.defer(cx, move |window, cx| update_dialog::open(app, window, cx));
                }
            });
        })
        .detach();
    }

    /// The flow shown by the update dialog; created from the pending release on first use.
    pub(crate) fn update_flow_for_dialog(&mut self) -> Option<Arc<UpdateFlow>> {
        if self.update_flow.is_none() {
            self.update_flow = self.update_available.clone().map(UpdateFlow::new);
        }
        self.update_flow.clone()
    }

    pub(crate) fn finish_update_flow(&mut self) {
        self.update_flow = None;
    }

    pub(crate) fn start_update(&mut self, flow: Arc<UpdateFlow>, window: &mut Window, cx: &mut Context<Self>) {
        update_dialog::run(cx.entity(), flow, window, cx);
    }

    fn show_window(&mut self, cx: &mut Context<Self>) {
        if let Some(hwnd) = self.hwnd {
            win_show::show(hwnd);
        }
        self.hidden_to_tray = false;
        self.sync_right_worktree(cx);
        cx.activate(true);
        cx.notify();
    }

    /// The window's close button / Alt+F4: hide to the tray, or quit.
    fn on_close_requested(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.client.snapshot().status, ConnStatus::Incompatible { .. }) {
            self.quit_ui_only(cx);
            return;
        }
        match self.hwnd {
            Some(hwnd) if self.tray.is_some() && settings::get(cx).close_to_tray => {
                self.hidden_to_tray = true;
                self.sync_right_worktree(cx);
                win_show::hide(hwnd);
            }
            _ => self.request_quit(window, cx),
        }
    }

    /// Quits, after a confirmation when agents are still busy.
    fn request_quit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.restarting_daemon {
            return;
        }
        let infos: Vec<TerminalInfo> =
            self.client.snapshot().terminals.iter().map(|t| t.info.clone()).collect();
        if !tray::needs_quit_confirm(&infos) {
            self.quit_now(cx);
            return;
        }
        self.show_window(cx);
        if window.has_active_dialog(cx) {
            window.close_dialog(cx);
        }
        let lang = settings::language(cx);
        let app = cx.entity();
        worktree_dialogs::open_confirm(
            window,
            cx,
            tr(lang, Tr::QuitConfirmTitle).to_string(),
            tr(lang, Tr::QuitConfirmText).to_string(),
            None,
            tr(lang, Tr::Quit),
            true,
            Rc::new(move |_, cx| {
                app.update(cx, |this, cx| this.quit_now(cx));
            }),
        );
    }

    fn save_current_session(&mut self) -> anyhow::Result<()> {
        if self.session_ready() {
            let infos: Vec<_> = self.client.snapshot().terminals.into_iter().map(|t| t.info).collect();
            let session = self.build_session(&infos);
            session.save()?;
            self.pending_save = None;
        }
        Ok(())
    }

    /// Saves the session, stops the daemon (and with it every terminal) and exits.
    fn quit_now(&mut self, cx: &mut Context<Self>) {
        if let Err(err) = self.save_current_session() {
            tracing::error!("failed to save session: {err:#}");
        }
        self.client.shutdown_all();
        self.quit_ui_only(cx);
    }

    pub(crate) fn quit_ui_only(&mut self, cx: &mut Context<Self>) {
        self.tray = None;
        cx.quit();
    }

    pub(crate) fn request_daemon_restart(&mut self, cx: &mut Context<Self>) {
        if self.restarting_daemon {
            return;
        }
        let snap = self.client.snapshot();
        if !matches!(snap.status, ConnStatus::Incompatible { .. }) {
            return;
        }
        self.restarting_daemon = true;
        self.pending_save = None;
        self.client.restart_daemon(crate::daemon_restart::Request { expected: snap.status, session: None });
        cx.notify();
    }

    fn finish_daemon_restart(&mut self) {
        let Some(result) = self.client.take_restart_result() else { return };
        self.restarting_daemon = false;
        self.restart_dialog_shown = false;
        self.last_session = None;
        if let RestartResult::Stopped(session) = result {
            self.restore = Some(session.selected.clone());
            self.saved = Some(session);
            self.restored = false;
            self.restore_targets.clear();
            self.restore_started = None;
            self.pending_creates.clear();
            self.pending_terminal_opens.clear();
            self.quick_terminals.clear();
            self.quick_return.clear();
            self.views.clear();
            self.terminal_layouts.clear();
            self.ws.prune(&[]);
            self.attention.clear();
            self.seen_sent.clear();
            self.focused = None;
        }
    }

    // ---- session ---------------------------------------------------------------------------

    /// True once the saved state has been applied; before that the live state is incomplete and
    /// must not overwrite the file on disk.
    fn session_ready(&self) -> bool {
        !self.restarting_daemon && self.restored && self.restore.is_none() && self.restore_targets.is_empty()
    }

    fn build_session(&self, infos: &[TerminalInfo]) -> Session {
        let mut session = session::build_session(&SessionInput {
            terminals: infos,
            ws: &self.ws,
            layouts: &self.terminal_layouts,
            projects: &self.projects,
            groups: &self.groups,
            home_collapsed: self.home_collapsed,
            collapsed: &self.collapsed,
            collapsed_groups: &self.collapsed_groups,
            window: self.window_state,
            sidebar_w: Some(self.sidebar_w),
            right_panel: self.right_panel,
        });
        session.daemon_pid = self.client.daemon_pid();
        session.record_owners(|key| self.client.owner(key));
        session
    }

    /// Schedules a debounced save when the session differs from the last one seen.
    fn touch_session(&mut self, infos: &[TerminalInfo], cx: &mut Context<Self>) {
        if !self.session_ready() {
            return;
        }
        let session = self.build_session(infos);
        if self.last_session.as_ref() == Some(&session) {
            return;
        }
        self.last_session = Some(session.clone());
        self.pending_save = Some(session);
        if !self.save_scheduled {
            self.save_scheduled = true;
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(SESSION_SAVE_DEBOUNCE).await;
                let _ = this.update(cx, |this, _| this.flush_session());
            })
            .detach();
        }
    }

    fn flush_session(&mut self) {
        self.save_scheduled = false;
        if let Some(session) = self.pending_save.take() {
            if let Err(e) = session.save() {
                tracing::error!("failed to save session: {e:#}");
            }
        }
    }

    /// Recreates the saved terminals once, after the first terminal list and the projects have
    /// arrived and only when the daemon started out empty.
    fn maybe_restore_terminals(&mut self, infos: &[TerminalInfo], cx: &Context<Self>) {
        if self.restored || self.restore.is_some() {
            return;
        }
        let Some(count) = self.client.first_list_count() else { return };
        self.restored = true;
        let attached: Vec<_> = infos.iter().filter_map(|info| Some((info.id, self.client.owner(info.id)?))).collect();
        let peers = self.client.connected_peers();
        let mut plan = Vec::new();
        let mut layouts = HashMap::new();
        let mut attached_active = HashMap::new();
        if let Some(saved) = self.saved.take() {
            let (missing, active, saved_layouts) = session::reattach(saved, &attached, &peers, count);
            for (key, id) in active {
                self.ws.set_active(&key, id);
                attached_active.insert(key, id);
            }
            layouts = saved_layouts;
            plan = session::restore_plan(&missing, 0, &self.projects);
        }
        for (key, layout) in &layouts {
            if !plan.iter().any(|(pending, _, _)| pending == key) {
                if let Some(mapped) = layout.remap(|leaf| match leaf {
                    session::RestoreLeaf::Live(id) => Some(id),
                    session::RestoreLeaf::Missing(_) => None,
                }) {
                    self.terminal_layouts.insert(key.clone(), mapped);
                }
            }
        }
        if plan.is_empty() && count == 0 {
            self.client.create_terminal_in(DEFAULT_COLS, DEFAULT_ROWS, None);
        }
        let lang = settings::language(cx);
        let resume_agents = settings::get(cx).resume_agents_on_restore;
        for (key, terminals, active) in plan {
            let worktree = key.path().map(PathBuf::from);
            let mut tokens = Vec::with_capacity(terminals.len());
            for term in terminals {
                self.next_restore_token += 1;
                let token = self.next_restore_token;
                tokens.push(token);
                let resume = term.agent.filter(|_| resume_agents)
                    .and_then(|a| session::resume_command(a.kind, &a.session_id));
                let data = RestoreData {
                    token,
                    scrollback: term.scrollback,
                    separator: tr(lang, Tr::RestoredScrollback),
                    interruption: term.interrupted.then(|| tr(lang, Tr::AgentInterrupted)),
                    resume,
                    protect_resume: false,
                    report_open_failure: false,
                    migration: None,
                };
                let cwd = (!term.cwd.as_os_str().is_empty()).then_some(term.cwd);
                self.client.restore_terminal(worktree.clone(), cwd, data);
            }
            self.restore_targets.push(RestoreTarget {
                attached_active: attached_active.get(&key).copied(),
                layout: layouts.remove(&key),
                key, tokens, active,
            });
        }
        self.restore_started = Some(Instant::now());
        if !self.restore_targets.is_empty() {
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(RESTORE_TIMEOUT + Duration::from_millis(1)).await;
                let _ = this.update(cx, |this, cx| {
                    let snap = this.client.snapshot();
                    this.finish_restore_targets(&snap);
                    let infos: Vec<_> = snap.terminals.iter().map(|entry| entry.info.clone()).collect();
                    this.touch_session(&infos, cx);
                    cx.notify();
                });
            }).detach();
        }
    }

    /// Resolve restored identities only by their request token and workspace.
    fn finish_restore_targets(&mut self, snap: &Snapshot) {
        let timed_out = self.restore_started.is_some_and(|t| t.elapsed() > RESTORE_TIMEOUT);
        self.restore_targets.retain(|target| {
            let ids: Vec<_> = target.tokens.iter().map(|token| {
                snap.terminals.iter().find(|entry| {
                    entry.restore_token == Some(*token)
                        && target.key.contains(entry.info.worktree.as_deref())
                }).map(|entry| entry.info.id)
            }).collect();
            if ids.iter().any(Option::is_none) && !timed_out {
                return true;
            }
            let mapped = target.layout.as_ref().and_then(|layout| layout.remap(|leaf| match leaf {
                session::RestoreLeaf::Live(id) => snap.terminals.iter()
                    .any(|entry| entry.info.id == id && target.key.contains(entry.info.worktree.as_deref()))
                    .then_some(id),
                session::RestoreLeaf::Missing(index) => ids.get(index).copied().flatten(),
            }));
            let selected = target.active.and_then(|index| ids.get(index).copied().flatten())
                .or(target.attached_active)
                .or_else(|| mapped.as_ref().map(Layout::first_leaf));
            if let Some(layout) = mapped {
                self.terminal_layouts.insert(target.key.clone(), layout);
            }
            if let Some(id) = selected {
                self.ws.set_active(&target.key, id);
            }
            false
        });
    }

    fn update_tray(&mut self, infos: &[TerminalInfo], cx: &mut Context<Self>) {
        let Some(tray) = self.tray.as_mut() else { return };
        let lang = settings::language(cx);
        if lang != self.tray_lang {
            tray.set_language(lang);
            self.tray_lang = lang;
        }
        let (waiting, working) = tray::agent_counts(infos);
        tray.set_tooltip(&tray::tray_tooltip(lang, waiting, working));
    }

    // ---- selection -------------------------------------------------------------------------

    /// Selects `key` and persists the choice.
    fn set_selected(&mut self, key: WorkspaceKey, cx: &mut Context<Self>) {
        if self.ws.selected != key {
            // Make sure the selected worktree is visible: expand its project.
            crate::groups::expand_to(&key, &self.projects, &mut self.collapsed, &mut self.collapsed_groups);
            settings::set_last_workspace(key.path().map(PathBuf::from), cx);
            self.ws.selected = key;
            self.dragged_terminal = None;
            self.sync_right_worktree(cx);
            self.sync_diff_highlight(cx);
        }
        cx.notify();
    }

    pub(crate) fn select_workspace(&mut self, key: WorkspaceKey, _window: &mut Window, cx: &mut Context<Self>) {
        self.set_selected(key, cx);
    }

    pub(crate) fn activate_terminal(&mut self, id: TermId, window: &mut Window, cx: &mut Context<Self>) {
        self.select_terminal(id, !self.standalone_quick_tab(id), window, cx);
    }

    fn select_terminal(&mut self, id: TermId, put_in_layout: bool, window: &mut Window, cx: &mut Context<Self>) {
        let key = self.ws.selected.clone();
        if let Some(layout) = self.terminal_layouts.get_mut(&key) {
            let ids: Vec<_> = self.client.snapshot().terminals.iter()
                .filter(|entry| key.contains(entry.info.worktree.as_deref()))
                .map(|entry| entry.info.id).collect();
            let previous = self.ws.active_in(&key, &ids);
            if put_in_layout {
                let previous_is_standalone = previous.is_some_and(|id| {
                    is_standalone_quick_tab(Some(layout), id, self.quick_terminals.contains_key(&id))
                });
                let remembered = self.quick_return.remove(&key);
                let pane = pane_to_replace(layout, previous, previous_is_standalone, remembered);
                layout.replace(pane, id);
            } else if let Some(pane) = previous.filter(|&id| layout.contains(id)) {
                self.quick_return.insert(key.clone(), pane);
            }
        } else if put_in_layout {
            self.terminal_layouts.insert(key.clone(), Layout::Leaf(id));
        }
        for target in &mut self.restore_targets {
            if target.key == key {
                target.active = None;
                target.attached_active = Some(id);
                if put_in_layout {
                    target.layout = None;
                }
            }
        }
        self.ws.set_active(&key, id);
        self.deactivate_tab(&key, cx);
        self.focused = Some(id);
        if let Some(view) = self.views.get(&id) {
            view.update(cx, |v, cx| v.focus(window, cx));
        }
        cx.notify();
    }

    /// Selects workspace `key` and activates terminal `id` in it (sidebar terminal rows).
    pub(crate) fn activate_terminal_in(
        &mut self,
        key: WorkspaceKey,
        id: TermId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_selected(key, cx);
        self.activate_terminal(id, window, cx);
    }

    pub(crate) fn workspace_tabs(&self) -> Option<&WorkspaceTabs<TabKey, TabView>> {
        self.workspace_tabs.get(&self.ws.selected)
    }

    fn active_tab(&self) -> Option<&WorkspaceTab<TabKey, TabView>> {
        self.workspace_tabs()?.active()
    }

    fn sync_diff_highlight(&self, cx: &mut Context<Self>) {
        let key = self.active_tab().and_then(|tab| match &tab.key {
            TabKey::Diff(key) => Some(key.clone()),
            TabKey::Activity(_) => None,
        });
        self.git_panel.update(cx, |panel, cx| panel.set_active_diff(key, cx));
    }

    fn deactivate_tab(&mut self, key: &WorkspaceKey, cx: &mut Context<Self>) {
        if let Some(tabs) = self.workspace_tabs.get_mut(key) {
            tabs.active = None;
        }
        self.focused_tab = None;
        self.sync_diff_highlight(cx);
    }

    fn on_git_panel_event(
        &mut self,
        event: &GitPanelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            GitPanelEvent::OpenDiff { worktree, repo, key, original } => {
                if self.ws.selected.contains(Some(worktree)) {
                    self.open_diff(worktree.clone(), repo.clone(), key.clone(), original.clone(), window, cx);
                }
            }
            GitPanelEvent::Refreshed(worktree) | GitPanelEvent::Mutated { worktree, .. } => {
                if let GitPanelEvent::Mutated { branch_switched: true, .. } = event {
                    self.client.refresh_projects();
                }
                if let Some(tabs) = self.workspace_tabs.get(&WorkspaceKey::of(Some(worktree))) {
                    for tab in &tabs.tabs {
                        let (TabKey::Diff(key), TabView::Diff(view)) = (&tab.key, &tab.view) else { continue };
                        if let GitPanelEvent::Mutated { diff_tab: Some(id), error, .. } = event {
                            if tab.id == *id {
                                view.update(cx, |view, cx| view.mutation_finished(error.clone(), cx));
                            }
                        }
                        if key.source.is_live() {
                            let rename = self.git_panel.read(cx).diff_rename(worktree, key);
                            view.update(cx, |view, cx| view.refresh_live(rename, cx));
                        }
                    }
                }
            }
        }
    }

    fn open_diff(
        &mut self,
        worktree: PathBuf,
        repo: Repo,
        key: CompareKey,
        original: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = self.next_tab_id;
        let panel = self.git_panel.clone();
        let tabs = self.workspace_tabs.entry(self.ws.selected.clone()).or_default();
        let tab = tabs.open(id, TabKey::Diff(key.clone()), || {
            TabView::Diff(cx.new(|cx| DiffView::new(id, worktree, repo, key, original, panel, cx)))
        });
        let opened = tab.id;
        if opened == id {
            self.next_tab_id += 1;
            if let TabView::Diff(view) = &tab.view {
                view.update(cx, DiffView::reload);
            }
        }
        self.activate_tab(opened, window, cx);
    }

    pub(crate) fn open_subagent_activity(
        &mut self,
        workspace: WorkspaceKey,
        term: TermId,
        key: SubagentActivityKey,
        label: gpui_kit::SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(owner) = self.client.owner(term) else {
            notifications::show_error(tr(settings::language(cx), Tr::ActivityUnavailable).into(), window, cx);
            return;
        };
        let target = ActivityTarget { owner, key };
        let key = TabKey::Activity(target.clone());
        // A terminal can be moved to another workspace while its original activity tab stays open.
        if let Some((workspace, id)) = self.workspace_tabs.iter().find_map(|(workspace, tabs)| {
            tabs.tabs.iter().find(|tab| tab.key == key).map(|tab| (workspace.clone(), tab.id))
        }) {
            self.set_selected(workspace, cx);
            self.activate_tab(id, window, cx);
            return;
        }
        self.set_selected(workspace.clone(), cx);
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        let tabs = self.workspace_tabs.entry(workspace).or_default();
        tabs.open(id, key, || {
            TabView::Activity(cx.new(|cx| ActivityView::new(target, label, &self.activity_rt, cx)))
        });
        self.activate_tab(id, window, cx);
    }

    pub(crate) fn activate_tab(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tabs) = self.workspace_tabs.get_mut(&self.ws.selected) {
            tabs.activate(id);
        }
        self.focus_tab(window, cx);
        self.sync_diff_highlight(cx);
        cx.notify();
    }

    pub(crate) fn close_tab(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(tabs) = self.workspace_tabs.get_mut(&self.ws.selected) {
            tabs.close(id);
        }
        self.sync_diff_highlight(cx);
        cx.notify();
    }

    fn focus_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.active_tab() else { return };
        let id = tab.id;
        match &tab.view {
            TabView::Diff(view) => view.update(cx, |view, cx| view.focus(window, cx)),
            TabView::Activity(view) => view.update(cx, |view, cx| view.focus(window, cx)),
        }
        // Terminal tabs are rendered before the other workspace tabs.
        let terminals = self.client.snapshot().terminals.iter()
            .filter(|term| self.ws.selected.contains(term.info.worktree.as_deref())).count();
        let index = self.workspace_tabs()
            .and_then(|tabs| tabs.tabs.iter().position(|tab| tab.id == id)).unwrap_or(0);
        self.tab_scroll.scroll_to_item(terminals + index);
        self.focused_tab = Some(id);
        self.focused = None;
    }

    fn workspace_ids(&self, infos: &[TerminalInfo]) -> Vec<TermId> {
        terminals_in(&self.ws.selected, infos)
    }

    // ---- commands --------------------------------------------------------------------------

    pub(crate) fn terminal_restarting(&self, id: TermId) -> bool {
        self.pending_terminal_restarts.contains_key(&id)
    }

    pub(crate) fn restart_legacy_terminal(&mut self, id: TermId, window: &mut Window, cx: &mut Context<Self>) {
        self.restart_terminals(Some(id), window, cx);
    }

    fn restart_all_legacy_terminals(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.pending_terminal_restarts.is_empty() {
            return;
        }
        self.restart_terminals(None, window, cx);
    }

    fn restart_terminals(
        &mut self,
        single: Option<TermId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let snap = self.client.snapshot();
        if !can_restart_terminals(self.restarting_daemon, snap.status) {
            return;
        }
        let ids = match single {
            Some(id) => vec![id],
            None => legacy_restart_ids(snap.terminals.iter().map(|entry| (entry.info.id, entry.legacy))).collect(),
        };
        let batch = single.is_none();
        let mut targets = Vec::new();
        for id in ids {
            if self.terminal_restarting(id) {
                continue;
            }
            self.next_restore_token += 1;
            let token = self.next_restore_token;
            self.pending_terminal_restarts.insert(id, token);
            targets.push((id, token));
        }
        if targets.is_empty() {
            return;
        }
        let client = self.client.clone();
        let rt = self.activity_rt.clone();
        let lang = settings::language(cx);
        let resume = settings::get(cx).resume_agents_on_restore;
        cx.spawn_in(window, async move |this, cx| {
            let mut failed = false;
            for (id, token) in targets {
                let client = client.clone();
                let result = rt.spawn(async move {
                    client.migrate_terminal(id, token, resume, tr(lang, Tr::RestoredScrollback), tr(lang, Tr::AgentInterrupted)).await
                }).await.map_err(anyhow::Error::from).and_then(|result| result);
                let Ok(completed) = this.update_in(cx, |this, _, cx| {
                    let snap = this.client.snapshot();
                    let completed = if result.is_err() {
                        this.recover_terminal_restart(&snap, id, token)
                    } else {
                        this.map_restarted_terminals(&snap);
                        true
                    };
                    this.pending_terminal_restarts.remove(&id);
                    cx.notify();
                    completed
                }) else {
                    return;
                };
                if let Err(err) = result && !completed {
                    failed = true;
                    tracing::warn!(id, "could not complete terminal migration: {err:#}");
                }
            }
            if failed {
                let _ = this.update_in(cx, |_, window, cx| {
                    let key = if batch { Tr::BatchTerminalRestartFailed } else { Tr::TerminalRestartFailed };
                    notifications::show_error(tr(settings::language(cx), key).into(), window, cx);
                });
            }
        }).detach();
        cx.notify();
    }

    /// Restore the old pane before removing the pending entry so the next sync leaves it in place.
    fn recover_terminal_restart(&mut self, snap: &Snapshot, old: TermId, token: u64) -> bool {
        let replacement = snap.terminals.iter().find(|entry| !entry.legacy && entry.restore_token == Some(token));
        let old_exists = snap.terminals.iter().any(|entry| entry.legacy && entry.info.id == old);
        match migration_recovery(old_exists, replacement.map(|entry| entry.info.id)) {
            MigrationRecovery::KeepReplacement => {
                self.map_restarted_terminals(snap);
                true
            }
            MigrationRecovery::Revert(new) => {
                let key = WorkspaceKey::of(replacement.unwrap().info.worktree.as_deref());
                if let Some(layout) = self.terminal_layouts.get_mut(&key) {
                    layout.replace(new, old);
                }
                self.ws.replace_active(&key, new, old);
                self.client.kill_terminal(new);
                false
            }
            MigrationRecovery::Nothing => false,
        }
    }

    /// Adopt only the replacement returned for this request, not a concurrent new terminal.
    fn map_restarted_terminals(&mut self, snap: &Snapshot) {
        for (&old, &token) in &self.pending_terminal_restarts {
            let Some(entry) = snap.terminals.iter().find(|entry| !entry.legacy && entry.restore_token == Some(token)) else {
                continue;
            };
            let key = WorkspaceKey::of(entry.info.worktree.as_deref());
            let new = entry.info.id;
            if let Some(layout) = self.terminal_layouts.get_mut(&key) {
                layout.replace(old, new);
            }
            self.ws.replace_active(&key, old, new);
        }
    }

    pub(crate) fn new_terminal_in(&mut self, key: WorkspaceKey, cx: &mut Context<Self>) {
        let snap = self.client.snapshot();
        let infos: Vec<TerminalInfo> = snap.terminals.iter().map(|t| t.info.clone()).collect();
        let size = self
            .ws
            .active_in(&self.ws.selected, &terminals_in(&self.ws.selected, &infos))
            .and_then(|a| snap.terminals.iter().find(|t| t.info.id == a))
            .map(|t| t.session.size())
            .unwrap_or((DEFAULT_COLS, DEFAULT_ROWS));
        self.pending_creates.push_back(key.clone());
        while self.pending_creates.len() > MAX_PENDING_CREATES {
            self.pending_creates.pop_front();
        }
        self.client.create_terminal_in(size.0, size.1, key.path().map(PathBuf::from));
        cx.notify();
    }

    pub(crate) fn new_terminal_here(&mut self, cx: &mut Context<Self>) {
        self.new_terminal_in(self.ws.selected.clone(), cx);
    }

    pub(crate) fn create_worktree(
        &mut self,
        project_id: ProjectId,
        name: String,
        base: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.client.create_worktree(project_id, name, base);
        cx.notify();
    }

    fn cycle(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let infos: Vec<TerminalInfo> =
            self.client.snapshot().terminals.iter().map(|t| t.info.clone()).collect();
        let ids = self.workspace_ids(&infos);
        let cur = self.ws.active_in(&self.ws.selected, &ids);
        if let Some(next) = cycle_id(&ids, cur, delta) {
            self.activate_terminal(next, window, cx);
        }
    }

    fn on_key_capture(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        if ks.key == "escape" && self.dragged_terminal.take().is_some() && cx.stop_active_drag(window) {
            cx.notify();
            cx.stop_propagation();
            return;
        }
        let m = ks.modifiers;
        if !m.control || m.alt || m.platform {
            return;
        }
        match ks.key.as_str() {
            "t" if m.shift => self.new_terminal_here(cx),
            "g" if m.shift => self.toggle_right_panel(window, cx),
            "," if !m.shift => settings_dialog::open(cx.entity(), window, cx),
            "tab" => self.cycle(if m.shift { -1 } else { 1 }, window, cx),
            _ => return,
        }
        cx.stop_propagation();
    }

    // ---- per-frame sync --------------------------------------------------------------------

    /// Makes views, selection and pending actions match the daemon state.
    fn sync(&mut self, snap: &Snapshot, infos: &[TerminalInfo], window: &mut Window, cx: &mut Context<Self>) {
        if !cx.has_active_drag() { self.dragged_terminal = None; }
        self.map_restarted_terminals(snap);
        for entry in &snap.terminals {
            let id = entry.info.id;
            if let Some(view) = self.views.get(&id) {
                let agent = entry.info.agent.as_ref().map(|a| a.kind);
                view.update(cx, |v, _| v.set_agent(agent));
                continue;
            }
            let session = entry.session.clone();
            let view = cx.new(|cx| TerminalView::new(session, id, window, cx));
            self.views.insert(id, view);
            // A terminal this window asked for: show its workspace and activate it.
            let key = WorkspaceKey::of(entry.info.worktree.as_deref());
            if entry.restore_token.is_none()
                && self.client.owner(id).is_some_and(|owner| owner.socket == self.client.primary_socket())
                && let Some(pos) = self.pending_creates.iter().position(|k| *k == key)
            {
                self.pending_creates.remove(pos);
                self.set_selected(key, cx);
                self.activate_terminal(id, window, cx);
            }
        }
        let ids: Vec<TermId> = infos.iter().map(|t| t.id).collect();
        self.views.retain(|id, _| ids.contains(id));
        self.ws.prune(&ids);
        self.window_active = window.is_window_active();

        if let Some(update) = self.client.projects_since(self.seen_projects_rev) {
            self.seen_projects_rev = update.revision;
            self.projects = update.projects;
            self.groups = update.groups;
            self.on_projects_changed(update.created_worktrees, cx);
        }

        self.maybe_restore_terminals(infos, cx);
        self.finish_restore_targets(snap);
        self.terminal_layouts.retain(|key, layout| {
            layout.retain_where(|id| infos.iter().any(|info| info.id == id && key.contains(info.worktree.as_deref())))
        });
        self.quick_return.retain(|key, id| {
            self.terminal_layouts.get(key).is_some_and(|layout| layout.contains(*id))
        });
        self.finish_pending_terminal_opens(snap, window, cx);
        let key = self.ws.selected.clone();
        let ids = self.workspace_ids(infos);
        if let Some(active) = self.ws.active_in(&key, &ids)
            && !self.standalone_quick_tab(active)
        {
            let layout = self.terminal_layouts.entry(key.clone()).or_insert(Layout::Leaf(active));
            if !layout.contains(active) {
                self.ws.set_active(&key, layout.first_leaf());
            }
        }

        if let Some(id) = self.active_tab().map(|tab| tab.id) {
            if self.focused_tab != Some(id) {
                self.focus_tab(window, cx);
            }
        } else {
            let had_nonterminal_focus = self.focused_tab.take().is_some();
            let ws_ids = self.workspace_ids(infos);
            let active = self.ws.active_in(&self.ws.selected, &ws_ids);
            if active != self.focused {
                self.focused = active;
                if let Some(view) = active.and_then(|a| self.views.get(&a)) {
                    view.update(cx, |v, cx| v.focus(window, cx));
                }
            }
            if active.is_none() && had_nonterminal_focus {
                window.blur(cx);
            }
        }
        self.track_agents(snap, &settings::get(cx));
        self.update_tray(infos, cx);
        self.touch_session(infos, cx);
    }

    fn on_projects_changed(&mut self, created: Vec<(PathBuf, PathBuf)>, cx: &mut Context<Self>) {
        self.workspace_tabs.retain(|key, _| workspace_exists(key, &self.projects));
        if let Some(saved) = self.restore.take() {
            if let Some(session) = &self.saved {
                let (projects, groups) = session::resolve_collapsed(session, &self.projects, &self.groups);
                self.collapsed = projects;
                self.collapsed_groups = groups;
            }
            let key = restore_selection(saved.as_deref(), &self.projects);
            self.set_selected(key, cx);
        } else if !workspace_exists(&self.ws.selected, &self.projects) {
            self.set_selected(WorkspaceKey::Home, cx);
        }
        for (root, path) in created {
            if let Err(error) = crate::worktree_visibility::record_created(&root, &path) {
                tracing::warn!("could not record created worktree: {error:#}");
            }
            let key = WorkspaceKey::Worktree(path);
            if workspace_exists(&key, &self.projects) {
                self.set_selected(key.clone(), cx);
                self.new_terminal_in(key, cx);
            }
        }
        match crate::worktree_visibility::reconcile(&self.projects) {
            Ok(store) => self.worktree_visibility = store,
            Err(error) => tracing::warn!("could not load worktree preferences: {error:#}"),
        }
    }

    // ---- rendering -------------------------------------------------------------------------

    pub(crate) fn terminal_is_split(&self, id: TermId) -> bool {
        self.terminal_layouts.get(&self.ws.selected)
            .is_some_and(|layout| matches!(layout, Layout::Split { .. }) && layout.contains(id))
    }

    pub(crate) fn dragging_split_pane(&self) -> bool {
        self.dragged_terminal.is_some_and(|id| self.terminal_is_split(id))
    }

    fn standalone_quick_tab(&self, id: TermId) -> bool {
        is_standalone_quick_tab(
            self.terminal_layouts.get(&self.ws.selected), id, self.quick_terminals.contains_key(&id),
        )
    }

    pub(crate) fn run_quick_command(
        &mut self,
        source: TermId,
        command: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let snap = self.client.snapshot();
        let Some(entry) = snap.terminals.iter().find(|entry| {
            entry.info.id == source && self.ws.selected.contains(entry.info.worktree.as_deref())
        }) else { return };
        let workspace = self.ws.selected.clone();
        let cwd = entry.info.cwd.is_dir().then(|| entry.info.cwd.clone());
        self.next_restore_token += 1;
        let token = self.next_restore_token;
        if !self.client.open_quick_tab(workspace.path().map(PathBuf::from), cwd, entry.session.size(), token, command) {
            let message = tr(settings::language(cx), Tr::QuickCommandOpenFailed);
            notifications::show_error(message.to_owned(), window, cx);
            return;
        }
        self.track_terminal_open(token, workspace, TerminalOpenKind::QuickTab, cx);
    }

    pub(crate) fn request_terminal_split(&mut self, target: &TerminalDrag, edge: Edge, window: &mut Window, cx: &mut Context<Self>) {
        if target.workspace != self.ws.selected { return; }
        let snap = self.client.snapshot();
        let Some(entry) = snap.terminals.iter().find(|entry| {
            entry.info.id == target.id && target.workspace.contains(entry.info.worktree.as_deref())
        }) else { return };
        let cwd = entry.info.cwd.is_dir().then(|| entry.info.cwd.clone());
        let size = entry.session.size();
        self.next_restore_token += 1;
        let token = self.next_restore_token;
        let worktree = target.workspace.path().map(PathBuf::from);
        if !self.client.open_split(worktree, cwd, size, token) {
            let message = tr(settings::language(cx), Tr::TerminalSplitDisconnected);
            notifications::show_error(message.to_owned(), window, cx);
            return;
        }
        self.select_terminal(target.id, true, window, cx);
        self.track_terminal_open(token, target.workspace.clone(), TerminalOpenKind::Split { target: target.id, edge }, cx);
    }

    fn track_terminal_open(&mut self, token: u64, workspace: WorkspaceKey, kind: TerminalOpenKind, cx: &mut Context<Self>) {
        self.pending_terminal_opens.push(PendingTerminalOpen { token, workspace, kind, started: Instant::now() });
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(RESTORE_TIMEOUT + Duration::from_millis(1)).await;
            let _ = this.update(cx, |this, cx| {
                this.pending_terminal_opens.retain(|pending| pending.started.elapsed() <= RESTORE_TIMEOUT);
                cx.notify();
            });
        }).detach();
    }

    fn finish_pending_terminal_opens(&mut self, snap: &Snapshot, window: &mut Window, cx: &mut Context<Self>) {
        let failures = self.client.take_failed_terminal_opens();
        let (mut failed_split, mut failed_quick) = (false, false);
        self.pending_terminal_opens.retain(|request| {
            if failures.contains(&request.token) {
                match request.kind {
                    TerminalOpenKind::Split { .. } => failed_split = true,
                    TerminalOpenKind::QuickTab => failed_quick = true,
                }
                return false;
            }
            request.started.elapsed() <= RESTORE_TIMEOUT && match request.kind {
                TerminalOpenKind::Split { target, .. } => self.terminal_layouts
                    .get(&request.workspace).is_some_and(|layout| layout.contains(target)),
                TerminalOpenKind::QuickTab => true,
            }
        });
        for (failed, key) in [(failed_split, Tr::TerminalSplitFailed), (failed_quick, Tr::QuickCommandOpenFailed)] {
            if failed {
                let message = tr(settings::language(cx), key);
                cx.defer_in(window, move |_, window, cx| {
                    notifications::show_error(message.to_owned(), window, cx);
                });
            }
        }
        let mut index = 0;
        while let Some(request) = self.pending_terminal_opens.get(index) {
            let Some(entry) = snap.terminals.iter().find(|entry| entry.restore_token == Some(request.token)) else {
                index += 1;
                continue;
            };
            let request = self.pending_terminal_opens.remove(index);
            let key = &request.workspace;
            if !key.contains(entry.info.worktree.as_deref()) {
                continue;
            }
            match request.kind {
                TerminalOpenKind::Split { target, edge } => {
                    let layout = self.terminal_layouts.remove(key).unwrap();
                    self.terminal_layouts.insert(key.clone(), layout.split(entry.info.id, target, edge));
                }
                TerminalOpenKind::QuickTab => {
                    let mut command = TemporaryCommand::default();
                    command.observe(entry.info.exited.is_some(), sich_orch::paths::now_ms());
                    self.quick_terminals.insert(entry.info.id, command);
                }
            }
            if self.ws.selected == *key && self.active_tab().is_none() {
                self.activate_terminal(entry.info.id, window, cx);
            }
        }
    }

    pub(crate) fn begin_terminal_drag(&mut self, drag: &TerminalDrag, cx: &mut Context<Self>) {
        self.dragged_terminal = Some(drag.id);
        cx.notify();
    }

    pub(crate) fn detach_terminal(&mut self, drag: &TerminalDrag, window: &mut Window, cx: &mut Context<Self>) {
        let key = &drag.workspace;
        if key != &self.ws.selected
            || !self.terminal_layouts.get(key).is_some_and(|layout| {
                matches!(layout, Layout::Split { .. }) && layout.contains(drag.id)
            })
        {
            return;
        }
        let layout = self.terminal_layouts.remove(key).unwrap();
        let active = self.focused.unwrap_or_else(|| layout.first_leaf());
        let remaining = layout.remove(drag.id).expect("another pane remains");
        let active = visible_active(&remaining, active);
        self.terminal_layouts.insert(key.clone(), remaining);
        self.activate_terminal(active, window, cx);
    }

    fn split_terminal(&mut self, drag: &TerminalDrag, target: TermId, edge: Edge, window: &mut Window, cx: &mut Context<Self>) {
        let key = &drag.workspace;
        if key != &self.ws.selected || drag.id == target
            || !self.terminal_layouts.get(key).is_some_and(|layout| layout.contains(target))
            || !self.client.snapshot().terminals.iter().any(|entry| {
                entry.info.id == drag.id && key.contains(entry.info.worktree.as_deref())
            })
        {
            return;
        }
        let layout = self.terminal_layouts.remove(key).unwrap();
        self.terminal_layouts.insert(key.clone(), layout.split(drag.id, target, edge));
        self.activate_terminal(drag.id, window, cx);
    }

    fn render_split_node(&self, node: &Layout<TermId>, path: Vec<bool>, snap: &Snapshot,
        show_header: bool, lang: i18n::Language, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        use gpui_kit::{relative, SharedString};
        match node {
            Layout::Leaf(id) => {
                let id = *id;
                let Some(view) = self.views.get(&id) else { return div().size_full().into_any_element() };
                let Some(entry) = snap.terminals.iter().find(|entry| entry.info.id == id) else {
                    return div().size_full().into_any_element();
                };
                let header = show_header.then(|| self.render_pane_header(entry, self.focused == Some(id), lang, cx));
                let banner = self.quick_terminals.get(&id)
                    .and_then(|command| quick_commands::banner(id, command, self.orch_now_ms, lang, cx))
                    .or_else(|| orch_watch::worker_for_terminal(&self.orch, &entry.info)
                        .and_then(|worker| crate::orch_banner::render(worker, self.orch_now_ms, lang, cx)));
                let standalone = self.standalone_quick_tab(id);
                let tint = self.dragged_terminal.filter(|&source| !standalone && source != id && cx.has_active_drag())
                    .map(|_| cx.theme().primary.opacity(0.25));
                let feedback = tint.map(|tint| gpui_kit::canvas(
                    |_, _, _| (),
                    move |bounds, _, window, _| {
                        let rect = pane_rect(bounds);
                        let p = window.mouse_position();
                        if let Some(edge) = rect.edge_at(p.x.into(), p.y.into()) {
                            let half = rect.half(edge);
                            window.paint_quad(gpui_kit::fill(
                                gpui_kit::Bounds::new(gpui_kit::point(px(half.x), px(half.y)),
                                    gpui_kit::size(px(half.width), px(half.height))),
                                tint,
                            ));
                        }
                    },
                ).absolute().left_0().top_0().size_full());
                let body = div().id(("split-body", id)).relative().flex_1().min_h_0().w_full().overflow_hidden()
                    .capture_any_mouse_down(cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                        if matches!(event.button, MouseButton::Left | MouseButton::Middle | MouseButton::Right)
                            && this.focused != Some(id)
                        {
                            this.activate_terminal(id, window, cx);
                        }
                    }));
                let body = if standalone { body } else {
                    body.on_drop(cx.listener(move |this, drag: &TerminalDrag, window, cx| {
                        // TerminalElement measures the same pane in prepaint, including
                        // resizes that occurred while the pointer was stationary.
                        let rect = this.views.get(&id).and_then(|view| view.read(cx).grid_bounds()).map(pane_rect);
                        let p = window.mouse_position();
                        if let Some(edge) = rect.and_then(|rect| rect.edge_at(p.x.into(), p.y.into())) {
                            this.split_terminal(drag, id, edge, window, cx);
                        }
                    }))
                };
                let body = body
                    .on_drop(cx.listener(move |this, paths: &ExternalPaths, window, cx| {
                        if this.views.get(&id).is_some_and(|view| {
                            view.update(cx, |view, cx| view.paste_dropped_paths(paths.paths(), cx))
                        }) {
                            this.activate_terminal(id, window, cx);
                        }
                    }))
                    .child(view.clone()).children(feedback);
                // The lifetime banner sits above the body (not over it): the terminal gets the rest.
                v_flex().size_full().min_w_0().min_h_0().overflow_hidden()
                    .children(header).children(banner).child(body).into_any_element()
            }
            Layout::Split { axis, ratio, first, second } => {
                let axis = *axis;
                let ratio = terminal_layout::clamp_ratio(*ratio);
                let mut first_path = path.clone(); first_path.push(false);
                let mut second_path = path.clone(); second_path.push(true);
                let first = self.render_split_node(first, first_path, snap, true, lang, cx);
                let second = self.render_split_node(second, second_path, snap, true, lang, cx);
                let drag = SeparatorDrag { workspace: self.ws.selected.clone(), path: path.clone() };
                const DIVIDER: f32 = 6.;
                let handle = div().id(SharedString::from(format!("split-handle-{path:?}")))
                    .group("split-handle").flex().items_center().justify_center()
                    .flex_shrink_0().occlude()
                    .on_drag(drag, |_, _, _, cx| cx.new(|_| DragGhost));
                let line = div().bg(cx.theme().muted_foreground.opacity(0.55))
                    .group_hover("split-handle", |style| style.bg(cx.theme().primary.opacity(0.7)))
                    .group_drag_over::<SeparatorDrag>("split-handle", |style| style.bg(cx.theme().primary.opacity(0.7)));
                let (one, two, handle) = match axis {
                    Axis::Horizontal => (
                        div().w(relative(ratio)).h_full().min_w_0().child(first),
                        div().w(relative(1. - ratio)).h_full().min_w_0().child(second),
                        handle.w(px(DIVIDER)).h_full().cursor(CursorStyle::ResizeLeftRight)
                            .child(line.w(px(1.)).h_full()),
                    ),
                    Axis::Vertical => (
                        div().h(relative(ratio)).w_full().min_h_0().child(first),
                        div().h(relative(1. - ratio)).w_full().min_h_0().child(second),
                        handle.h(px(DIVIDER)).w_full().cursor(CursorStyle::ResizeUpDown)
                            .child(line.h(px(1.)).w_full()),
                    ),
                };
                let container = div().id(SharedString::from(format!("split-node-{path:?}")))
                    .relative().size_full().min_w_0().min_h_0().flex();
                let container = match axis { Axis::Horizontal => container.flex_row(), Axis::Vertical => container.flex_col() };
                container.on_drag_move::<SeparatorDrag>(cx.listener(move |this, event: &gpui_kit::DragMoveEvent<SeparatorDrag>, _, cx| {
                    let drag = event.dragged_item().downcast_ref::<SeparatorDrag>().unwrap();
                    if drag.workspace != this.ws.selected || drag.path != path { return; }
                    let rect = pane_rect(event.bounds);
                    let p = event.event.position;
                    if let Some(ratio) = rect.ratio_at(axis, p.x.into(), p.y.into(), DIVIDER)
                        && this.terminal_layouts.get_mut(&drag.workspace).is_some_and(|layout| layout.resize(&drag.path, ratio))
                    {
                        for target in &mut this.restore_targets {
                            if target.key == drag.workspace { target.layout = None; target.active = None; }
                        }
                        cx.notify();
                    }
                })).child(one).child(handle).child(two).into_any_element()
            }
        }
    }

    fn render_main(
        &self,
        snap: &Snapshot,
        infos: &[TerminalInfo],
        lang: i18n::Language,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let ids = self.workspace_ids(infos);
        let tab = self.active_tab();
        let active = if tab.is_none() { self.ws.active_in(&self.ws.selected, &ids) } else { None };
        let tab_bar = self.render_tab_bar(snap, &ids, active, lang, cx);
        let quick_layout = active.filter(|&id| self.standalone_quick_tab(id)).map(Layout::Leaf);
        let layout = quick_layout.as_ref().or_else(|| self.terminal_layouts.get(&self.ws.selected));
        let content = match (tab.map(|tab| &tab.view), layout) {
            (Some(TabView::Diff(view)), _) => div().flex_1().min_h_0().w_full().child(view.clone()),
            (Some(TabView::Activity(view)), _) => div().flex_1().min_h_0().w_full().child(view.clone()),
            (None, Some(layout)) => {
                div().relative().flex_1().min_h_0().w_full()
                    .child(self.render_split_node(layout, Vec::new(), snap, false, lang, cx))
            }
            (None, None) => div()
                .flex_1()
                .min_h_0()
                .w_full()
                .flex()
                .flex_col()
                .gap_3()
                .items_center()
                .justify_center()
                .text_color(theme.muted_foreground)
                .child(tr(lang, Tr::NoTerminals))
                .child(
                    Button::new("empty-new-terminal")
                        .small()
                        .label(tr(lang, Tr::NewTerminal))
                        .on_click(cx.listener(|this, _: &ClickEvent, _w, cx| {
                            this.new_terminal_here(cx)
                        })),
                ),
        };
        v_flex().flex_1().min_w_0().h_full().child(tab_bar).child(content)
    }

    fn render_right_panel(&self, width: f32, lang: i18n::Language, cx: &Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let pane = match self.right_panel.tab {
            RightTab::Git => self.git_panel.clone().into_any_element(),
        };
        let tabs = RightTab::ALL.iter().map(|&tab| {
            let (id, icon, label) = match tab {
                RightTab::Git => ("right-tab-git", Ic::GitBranch, Tr::Git),
            };
            let active = self.right_panel.tab == tab;
            h_flex().id(id).h_full().px_3().gap_1().cursor_pointer().border_b_2()
                .border_color(if active { theme.primary } else { theme.primary.opacity(0.) })
                .bg(if active { theme.sidebar } else { theme.tab_bar })
                .text_size(px(12.5))
                .child(Icon::new(icon).size(px(14.)))
                .child(tr(lang, label))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if this.right_panel.tab != tab {
                        this.right_panel.tab = tab;
                        this.sync_right_worktree(cx);
                        this.sidebar_changed(cx);
                    }
                }))
        });
        v_flex().w(px(width)).h_full().flex_shrink_0().min_h_0().overflow_hidden()
            .border_l_1().border_color(theme.border).bg(theme.sidebar)
            .child(
                h_flex().h(px(32.)).flex_shrink_0().bg(theme.tab_bar).border_b_1().border_color(theme.border)
                    .children(tabs),
            )
            .child(div().flex_1().min_h_0().w_full().child(pane))
    }

    fn render_status_bar(
        &self,
        snap: &Snapshot,
        lang: i18n::Language,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let status = status_line(lang, &snap.status, self.restarting_daemon, snap.legacy_terminals);
        let indicator = match status {
            StatusLine::Dot { tooltip, show_restart } => {
                let busy = !self.pending_terminal_restarts.is_empty();
                let restart = show_restart.then(|| {
                    let tip = tr(lang, if busy { Tr::TerminalRestarting } else { Tr::RestartAllLegacyTerminals });
                    let button = h_flex()
                        .id("restart-all-legacy-terminals")
                        .size(px(18.)).items_center().justify_center()
                        .tooltip(move |window, cx| Tooltip::new(tip).build(window, cx))
                        .child(Icon::new(Ic::CircleAlert).size(px(12.)).text_color(theme.warning.opacity(if busy { 0.45 } else { 0.9 })));
                    if busy { button } else {
                        button.cursor_pointer().on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.restart_all_legacy_terminals(window, cx);
                        }))
                    }
                });
                let dot = div()
                    .id("daemon-connected")
                    .size(px(16.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                    .child(div().size(px(8.)).rounded_full().bg(theme.success));
                h_flex().items_center().gap_1().child(dot).children(restart).into_any_element()
            }
            StatusLine::Text(text) => text.into_any_element(),
        };
        h_flex()
            .h(px(24.))
            .w_full()
            .flex_shrink_0()
            .px_3()
            .gap_2()
            .items_center()
            .bg(theme.status_bar)
            .border_t_1()
            .border_color(theme.status_bar_border)
            .text_size(px(12.))
            .text_color(theme.muted_foreground)
            .child(div().flex_1().min_w_0().truncate().child(indicator))
            .children(self.usage_bar.render(lang, cx))
            .child(
                Button::new("open-settings")
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(IconName::Settings))
                    .tooltip(tr(lang, Tr::Settings))
                    .on_click(cx.listener(|_this, _: &ClickEvent, window, cx| {
                        settings_dialog::open(cx.entity(), window, cx)
                    })),
            )
    }
}

/// Replace the cached release and report a change worth announcing.
fn record_release(available: &mut Option<updater::Release>, found: Option<updater::Release>) -> bool {
    let changed = found.as_ref().is_some_and(|latest| {
        available.as_ref().is_none_or(|previous| latest.version != previous.version)
    });
    *available = found;
    changed
}

#[derive(Debug, PartialEq, Eq)]
enum StatusLine {
    Dot { tooltip: String, show_restart: bool },
    Text(String),
}

fn status_line(
    lang: i18n::Language,
    status: &ConnStatus,
    restarting: bool,
    legacy_terminals: bool,
) -> StatusLine {
    let text = if restarting {
        tr(lang, Tr::DaemonRestarting).to_string()
    } else {
        match *status {
            ConnStatus::Connected { pid } => {
                return StatusLine::Dot {
                    tooltip: i18n::connected(lang, pid),
                    show_restart: legacy_terminals,
                };
            }
            ConnStatus::Reconnecting => tr(lang, Tr::Reconnecting).to_string(),
            ConnStatus::Incompatible { version, .. } => {
                i18n::daemon_incompatible(lang, version, PROTO_VERSION)
            }
        }
    };
    StatusLine::Text(text)
}

fn legacy_restart_ids(terminals: impl IntoIterator<Item = (TermId, bool)>) -> impl Iterator<Item = TermId> {
    terminals.into_iter().filter_map(|(id, legacy)| legacy.then_some(id))
}

fn can_restart_terminals(restarting_daemon: bool, status: ConnStatus) -> bool {
    !restarting_daemon && matches!(status, ConnStatus::Connected { .. })
}

#[derive(Debug, PartialEq, Eq)]
enum MigrationRecovery {
    KeepReplacement,
    Revert(TermId),
    Nothing,
}

fn migration_recovery(old_exists: bool, replacement: Option<TermId>) -> MigrationRecovery {
    match (old_exists, replacement) {
        (true, Some(new)) => MigrationRecovery::Revert(new),
        (false, Some(_)) => MigrationRecovery::KeepReplacement,
        (_, None) => MigrationRecovery::Nothing,
    }
}

/// Blocking release lookup; run it on the background executor.
async fn fetch_latest_now() -> anyhow::Result<Option<updater::Release>> {
    updater::fetch_latest(&updater::current_version())
}

/// Window bounds for the session (logical pixels); `None` when they look bogus.
fn saved_window_of(window: &Window, cx: &gpui_kit::gpui::App) -> Option<SavedWindow> {
    let (bounds, maximized) = match window.window_bounds() {
        WindowBounds::Windowed(b) => (b, false),
        WindowBounds::Maximized(b) => (b, true),
        WindowBounds::Fullscreen(b) => (b, false),
    };
    SavedWindow::from_bounds(
        f32::from(bounds.origin.x),
        f32::from(bounds.origin.y),
        f32::from(bounds.size.width),
        f32::from(bounds.size.height),
        maximized,
    )
    .map(|w| SavedWindow {
        display: window.display(cx).and_then(|d| d.uuid().ok()).map(|u| session::display_key(u.as_u128())),
        ..w
    })
}

/// Width of the invisible sidebar resize hit area.
const SIDEBAR_HANDLE_W: f32 = 6.;

impl AppView {
    fn on_sidebar_handle_down(&mut self, ev: &MouseDownEvent, _w: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        if ev.click_count >= 2 {
            self.sidebar_dragging = false;
            self.sidebar_w = DEFAULT_SIDEBAR_W;
            self.sidebar_changed(cx);
        } else {
            self.sidebar_dragging = true;
            cx.notify();
        }
    }

    fn on_sidebar_drag_move(&mut self, ev: &MouseMoveEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.sidebar_dragging {
            return;
        }
        let window_w = f32::from(window.viewport_size().width);
        self.sidebar_w = clamp_sidebar_w(f32::from(ev.position.x), self.available_left_width(window_w));
        cx.notify();
    }

    fn on_sidebar_drag_end(&mut self, _: &MouseUpEvent, _w: &mut Window, cx: &mut Context<Self>) {
        if self.sidebar_dragging {
            self.sidebar_dragging = false;
            self.sidebar_changed(cx);
        }
    }

    fn sync_right_worktree(&self, cx: &mut Context<Self>) {
        let visible = self.right_panel.open && self.right_panel.tab == RightTab::Git && !self.hidden_to_tray;
        let path = if visible { self.ws.selected.path() } else { None };
        self.git_panel.update(cx, |panel, cx| panel.select(path, cx));
    }

    pub(crate) fn toggle_right_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.right_panel.open = !self.right_panel.open;
        self.right_dragging = false;
        self.sync_right_worktree(cx);
        if !self.right_panel.open {
            if let Some(view) = self.focused.and_then(|id| self.views.get(&id)) {
                view.update(cx, |view, cx| view.focus(window, cx));
            }
        }
        self.sidebar_changed(cx);
    }

    fn available_left_width(&self, window_w: f32) -> f32 {
        if self.right_panel.open { window_w - MIN_RIGHT_W } else { window_w }
    }

    fn on_right_handle_down(&mut self, event: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        cx.stop_propagation();
        if event.click_count >= 2 {
            self.right_dragging = false;
            self.right_panel.width = Some(DEFAULT_RIGHT_W);
            self.sidebar_changed(cx);
        } else {
            self.right_dragging = true;
            cx.notify();
        }
    }

    fn on_right_drag_move(&mut self, event: &MouseMoveEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.right_dragging {
            let width = f32::from(window.viewport_size().width);
            let left = clamp_sidebar_w(self.sidebar_w, self.available_left_width(width));
            self.right_panel.width = Some(clamp_right_w(width - f32::from(event.position.x), width, left).max(MIN_RIGHT_W));
            cx.notify();
        }
    }

    fn on_right_drag_end(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.right_dragging {
            self.right_dragging = false;
            self.sidebar_changed(cx);
        }
    }

    /// Persists the new width through the debounced session save.
    fn sidebar_changed(&mut self, cx: &mut Context<Self>) {
        let infos: Vec<TerminalInfo> =
            self.client.snapshot().terminals.iter().map(|t| t.info.clone()).collect();
        self.touch_session(&infos, cx);
        cx.notify();
    }
}

impl Render for AppView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.finish_daemon_restart();
        let snap = self.client.snapshot();
        let infos: Vec<TerminalInfo> = snap.terminals.iter().map(|t| t.info.clone()).collect();
        self.sync(&snap, &infos, window, cx);
        let theme = cx.theme().clone();
        let lang = settings::language(cx);

        if let Some(msg) = notifications::take_new_error(self.client.last_error(), &mut self.shown_error) {
            cx.defer_in(window, move |_, window, cx| notifications::show_error(msg, window, cx));
        }

        let window_w = f32::from(window.viewport_size().width);
        let sidebar_w = clamp_sidebar_w(self.sidebar_w, self.available_left_width(window_w));
        let right_w = clamp_right_w(self.right_panel.width.unwrap_or(DEFAULT_RIGHT_W), window_w, sidebar_w);
        let sidebar = self.render_sidebar(&snap, &infos, lang, sidebar_w, cx);
        let main = self.render_main(&snap, &infos, lang, cx);
        let status_bar = self.render_status_bar(&snap, lang, cx);
        if matches!(snap.status, ConnStatus::Incompatible { .. })
            && !self.restarting_daemon && !self.restart_dialog_shown
        {
            self.restart_dialog_shown = true;
            let app = cx.entity();
            cx.defer_in(window, move |_, window, cx| worktree_dialogs::open_incompatible_daemon(app, window, cx));
        }

        // Thin hit area straddling the sidebar's right border.
        let handle = div()
            .id("sidebar-resize")
            .group("sidebar-resize")
            .absolute()
            .top_0()
            .bottom_0()
            .left(px(sidebar_w - SIDEBAR_HANDLE_W / 2.))
            .w(px(SIDEBAR_HANDLE_W))
            .cursor(CursorStyle::ResizeLeftRight)
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_sidebar_handle_down))
            .child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(px(SIDEBAR_HANDLE_W / 2. - 1.))
                    .w(px(2.))
                    .group_hover("sidebar-resize", |s| s.bg(theme.primary.opacity(0.6))),
            );

        let right_handle = self.right_panel.open.then(|| {
            div()
                .id("right-panel-resize")
                .group("right-panel-resize")
                .absolute()
                .top_0()
                .bottom_0()
                .right(px(right_w - SIDEBAR_HANDLE_W / 2.))
                .w(px(SIDEBAR_HANDLE_W))
                .cursor(CursorStyle::ResizeLeftRight)
                .on_mouse_down(MouseButton::Left, cx.listener(Self::on_right_handle_down))
                .child(
                    div().absolute().top_0().bottom_0().left(px(SIDEBAR_HANDLE_W / 2. - 1.))
                        .w(px(2.)).group_hover("right-panel-resize", |s| s.bg(theme.primary.opacity(0.6))),
                )
        });
        let right = self.right_panel.open.then(|| self.render_right_panel(right_w, lang, cx));

        let mut root = v_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .capture_key_down(cx.listener(Self::on_key_capture))
            .on_drag_move::<ExternalPaths>(|_, window, cx| {
                // GPUI 0.3.12 translates file drags to mouse events but leaves the window
                // in keyboard modality after typing, which suppresses drop hit-testing.
                if window.last_input_was_keyboard() {
                    window.defer(cx, |window, cx| {
                        if cx.has_active_drag() && window.last_input_was_keyboard() {
                            window.dispatch_event(PlatformInput::MouseMove(MouseMoveEvent {
                                position: window.mouse_position(),
                                modifiers: window.modifiers(),
                                pressed_button: None,
                            }), cx);
                        }
                    });
                }
            })
            .child(
                h_flex().relative().flex_1().min_h_0().child(sidebar).child(main)
                    .children(right).child(handle).children(right_handle),
            )
            .child(status_bar);
        if self.sidebar_dragging {
            // Covers the window while dragging so terminals never see the mouse.
            root = root.child(
                div()
                    .absolute()
                    .size_full()
                    .cursor(CursorStyle::ResizeLeftRight)
                    .on_mouse_move(cx.listener(Self::on_sidebar_drag_move))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::on_sidebar_drag_end))
                    .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_sidebar_drag_end)),
            );
        }
        if self.right_dragging {
            root = root.child(
                div()
                    .absolute()
                    .size_full()
                    .cursor(CursorStyle::ResizeLeftRight)
                    .on_mouse_move(cx.listener(Self::on_right_drag_move))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::on_right_drag_end))
                    .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_right_drag_end)),
            );
        }
        root
    }
}

fn pane_rect(bounds: gpui_kit::Bounds<gpui_kit::Pixels>) -> Rect {
    Rect {
        x: bounds.origin.x.into(),
        y: bounds.origin.y.into(),
        width: bounds.size.width.into(),
        height: bounds.size.height.into(),
    }
}

/// Keep the active identity inside the visible tree after a close or detach.
fn visible_active(layout: &Layout<TermId>, active: TermId) -> TermId {
    if layout.contains(active) { active } else { layout.first_leaf() }
}

fn is_standalone_quick_tab(layout: Option<&Layout<TermId>>, id: TermId, is_quick: bool) -> bool {
    is_quick && !layout.is_some_and(|layout| layout.contains(id))
}

fn pane_to_replace(
    layout: &Layout<TermId>,
    active: Option<TermId>,
    active_is_standalone: bool,
    remembered: Option<TermId>,
) -> TermId {
    let pane = if active_is_standalone { remembered } else { active };
    pane.map_or_else(|| layout.first_leaf(), |id| visible_active(layout, id))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn removed_or_detached_active_falls_back_to_a_visible_leaf() {
        let layout = Layout::Leaf(1).split(2, 1, Edge::Right).split(3, 2, Edge::Bottom);
        assert_eq!(visible_active(&layout, 2), 2);
        let layout = layout.remove(2).unwrap();
        assert_eq!(visible_active(&layout, 2), 1);
        assert_eq!(visible_active(&layout, 3), 3);
    }

    #[test]
    fn quick_tab_is_standalone_until_it_is_explicitly_split() {
        let layout = Layout::Leaf(1).split(2, 1, Edge::Right);
        assert!(is_standalone_quick_tab(Some(&layout), 3, true));
        assert!(!is_standalone_quick_tab(Some(&layout), 3, false));
        let layout = layout.split(3, 2, Edge::Bottom);
        assert!(!is_standalone_quick_tab(Some(&layout), 3, true));
    }

    #[test]
    fn selecting_from_a_quick_tab_replaces_the_previously_focused_pane() {
        let layout = Layout::Leaf(1).split(2, 1, Edge::Right).split(3, 2, Edge::Bottom);
        assert_eq!(pane_to_replace(&layout, Some(4), true, Some(3)), 3);
        assert_eq!(pane_to_replace(&layout.remove(3).unwrap(), Some(4), true, Some(3)), 1);
        let layout = Layout::Leaf(1).split(2, 1, Edge::Right);
        assert_eq!(pane_to_replace(&layout, Some(2), false, Some(1)), 2);
        assert_eq!(pane_to_replace(&layout, None, false, None), 1);
    }

    #[test]
    fn a_failed_terminal_migration_reverts_only_if_both_terminals_exist() {
        use MigrationRecovery::*;

        assert_eq!(migration_recovery(true, Some(7)), Revert(7));
        assert_eq!(migration_recovery(false, Some(7)), KeepReplacement);
        assert_eq!(migration_recovery(true, None), Nothing);
        assert_eq!(migration_recovery(false, None), Nothing);
    }

    const LANG: i18n::Language = i18n::Language::En;
    const CONNECTED: ConnStatus = ConnStatus::Connected { pid: 12345 };
    const INCOMPATIBLE: ConnStatus = ConnStatus::Incompatible {
        pid: None,
        version: None,
    };

    #[test]
    fn terminal_restarts_require_a_connected_daemon_that_is_not_restarting() {
        assert!(can_restart_terminals(false, CONNECTED));
        assert!(!can_restart_terminals(true, CONNECTED));
        assert!(!can_restart_terminals(false, ConnStatus::Reconnecting));
        assert!(!can_restart_terminals(false, INCOMPATIBLE));
    }

    #[test]
    fn activity_selects_one_tab_per_run_and_closes_back_to_a_diff_or_terminal() {
        use crate::daemon_client::TerminalOwner;
        use sich_git::compare::CompareSource;
        use sich_proto::AgentKind;

        let mut tabs = WorkspaceTabs::default();
        let diff = TabKey::Diff(CompareKey { path: "src/main.rs".into(), source: CompareSource::Working });
        tabs.open(1, diff, || ());
        let run = |pid, since| TabKey::Activity(ActivityTarget {
            owner: TerminalOwner { socket: "test".into(), id: 1, pid, created_ms: 10 },
            key: SubagentActivityKey {
                agent: AgentKind::Omp, session_id: Some("session".into()), id: "worker".into(), since_unix_ms: since,
            },
        });
        tabs.open(2, run(7, 30), || ());
        assert_eq!(tabs.open(3, run(7, 30), || panic!("duplicate activity view")).id, 2);
        tabs.open(3, run(7, 31), || ());
        tabs.open(4, run(8, 30), || ());
        assert_eq!(tabs.tabs.len(), 4);
        tabs.activate(1);
        tabs.close(3);
        assert_eq!(tabs.active, Some(1));
        tabs.close(4);
        tabs.activate(2);
        tabs.close(2);
        assert_eq!(tabs.active, Some(1));
        tabs.close(1);
        assert!(tabs.active().is_none());
    }

    #[test]
    fn periodic_checks_do_not_reannounce_a_dismissed_version() {
        let release = |patch| updater::Release {
            version: semver::Version::new(1, 0, patch),
            notes: String::new(),
            zip_name: String::new(),
            zip_url: String::new(),
            zip_size: 0,
            sha_url: String::new(),
        };
        let mut available = None;
        assert!(!record_release(&mut available, None));
        assert!(record_release(&mut available, Some(release(1))));
        let reread = updater::Release { notes: "Updated release notes".into(), ..release(1) };
        assert!(!record_release(&mut available, Some(reread.clone())));
        assert_eq!(available.as_ref(), Some(&reread));
        assert!(record_release(&mut available, Some(release(2))));
        assert!(!record_release(&mut available, None));
        assert!(available.is_none());
    }

    #[test]
    fn connected_keeps_the_pid_indicator_and_only_offers_restart_with_legacy_terminals() {
        for legacy in [false, true] {
            assert_eq!(
                status_line(LANG, &CONNECTED, false, legacy),
                StatusLine::Dot { tooltip: "Connected to daemon (pid 12345)".into(), show_restart: legacy },
            );
        }
    }

    #[test]
    fn batch_restart_selects_only_legacy_terminals_in_snapshot_order() {
        assert_eq!(legacy_restart_ids([(10, false), (7, true), (2, true), (9, false)]).collect::<Vec<_>>(), [7, 2]);
        assert_eq!(legacy_restart_ids([(10, false)]).next(), None);
    }

    #[test]
    fn restarting_hides_the_batch_restart_action() {
        for status in [CONNECTED, ConnStatus::Reconnecting, INCOMPATIBLE] {
            assert_eq!(
                status_line(LANG, &status, true, true),
                StatusLine::Text(tr(LANG, Tr::DaemonRestarting).into()),
            );
        }
    }

    #[test]
    fn reconnecting_shows_muted_text() {
        assert_eq!(
            status_line(LANG, &ConnStatus::Reconnecting, false, true),
            StatusLine::Text(tr(LANG, Tr::Reconnecting).into()),
        );
    }

    #[test]
    fn incompatible_shows_muted_text_with_or_without_version() {
        for version in [None, Some(PROTO_VERSION - 1)] {
            let status = ConnStatus::Incompatible { pid: None, version };
            assert_eq!(
                status_line(LANG, &status, false, true),
                StatusLine::Text(i18n::daemon_incompatible(LANG, version, PROTO_VERSION)),
            );
        }
    }
}
