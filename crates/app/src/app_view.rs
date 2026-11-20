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
    AppContext as _, ClickEvent, Context, Entity, InteractiveElement as _, IntoElement,
    CursorStyle, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement as _, Render,
    ScrollHandle, StatefulInteractiveElement as _, Styled as _, Window, WindowBounds,
    div, px,
};
use sich_git::Repo;
use sich_git::compare::CompareKey;
use sich_orch::auto_close::{AutoClose, countdown_seconds};
use sich_proto::{GroupId, GroupInfo, PROTO_VERSION, ProjectId, ProjectInfo, TermId, TerminalInfo};

use crate::agent_badge::{BadgeKind, kind_of, should_flash, should_mark_seen};
use crate::daemon_client::{ConnStatus, DEFAULT_COLS, DEFAULT_ROWS, DaemonClient, RestartResult, RestoreData, Snapshot};
use crate::diff_tabs::{DiffTab, DiffTabs};
use crate::diff_view::DiffView;
use crate::git_panel::{DEFAULT_RIGHT_W, GitPanel, GitPanelEvent, MIN_RIGHT_W, clamp_right_w};
use crate::i18n::{self, Tr, tr};
use crate::notifications;
use crate::orch_watch;
use crate::sidebar::{DEFAULT_SIDEBAR_W, clamp_sidebar_w};
use crate::session::{self, RightPanelState, RightTab, SavedWindow, Session, SessionInput};
use crate::settings;
use crate::settings_dialog;
use crate::tray::{self, Tray, UiEvent};
use crate::update_dialog::{self, UpdateFlow};
use crate::update_status::{self, CheckStatus};
use crate::updater;
use crate::view::TerminalView;
use crate::workspace::{
    WorkspaceKey, Workspaces, cycle_id, restore_selection, terminals_in, workspace_exists,
};
use crate::win_flash;
use crate::win_show;
use crate::worktree_dialogs;

/// How often the sidebar is repainted while agents are present, to refresh relative times.
const AGE_REFRESH: Duration = Duration::from_secs(30);

/// Upper bound for creations waiting to be matched with a `TerminalCreated`.
const MAX_PENDING_CREATES: usize = 8;

/// Delay before a changed session is written to disk.
const SESSION_SAVE_DEBOUNCE: Duration = Duration::from_millis(500);

/// How long a session restore may take before it stops blocking session saves.
const RESTORE_TIMEOUT: Duration = Duration::from_secs(15);

/// Terminals of one workspace being recreated from the saved session.
struct RestoreTarget {
    key: WorkspaceKey,
    expected: usize,
    active: Option<usize>,
}

pub struct AppView {
    pub(crate) client: DaemonClient,
    views: HashMap<TermId, Entity<TerminalView>>,
    diff_tabs: HashMap<WorkspaceKey, DiffTabs<Entity<DiffView>>>,
    next_diff_id: u64,
    focused_diff: Option<u64>,
    pub(crate) tab_scroll: ScrollHandle,
    pub(crate) ws: Workspaces,
    /// Saved workspace to restore when the first `Projects` update arrives.
    restore: Option<Option<PathBuf>>,
    pub(crate) projects: Vec<ProjectInfo>,
    pub(crate) worktree_visibility: crate::worktree_visibility::Store,
    pub(crate) groups: Vec<GroupInfo>,
    /// Groups whose project list is collapsed (default: expanded).
    pub(crate) collapsed_groups: HashSet<GroupId>,
    /// Projects whose tree is collapsed (default: expanded).
    pub(crate) collapsed: HashSet<ProjectId>,
    /// Workspaces this window asked a terminal for; the matching new terminal gets activated.
    pending_creates: VecDeque<WorkspaceKey>,
    seen_projects_rev: u64,
    shown_error: Option<Instant>,
    /// Terminal that last received keyboard focus.
    focused: Option<TermId>,
    /// Whether the window has focus (kept by the activation observer and `sync`).
    window_active: bool,
    /// Native window handle for the taskbar flash (Windows).
    hwnd: Option<isize>,
    /// Last badge kind seen per terminal, to detect transitions into "needs the user".
    attention: HashMap<TermId, Option<BadgeKind>>,
    /// `since_unix_ms` of the status for which `mark_seen` was already sent, per terminal.
    seen_sent: HashMap<TermId, u64>,
    /// Daemon socket name (for the final `Shutdown`).
    socket: String,
    events_tx: UnboundedSender<UiEvent>,
    events_rx: Option<UnboundedReceiver<UiEvent>>,
    tray: Option<Tray>,
    tray_lang: i18n::Language,
    /// Session loaded at startup, consumed by the restore.
    saved: Option<Session>,
    /// Whether the terminal restore decision was made (once per run).
    restored: bool,
    /// The client does not auto-create a first terminal because the restore will fill the daemon.
    suppress_auto_create: bool,
    restore_targets: Vec<RestoreTarget>,
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
    /// Newer release found by the periodic check.
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
        let git_panel = cx.new(|_| GitPanel::new(right_panel.history_collapsed));
        let suppress_auto_create = saved.as_ref().is_some_and(Session::has_terminals);
        // Coalescing wake path: bounded(1), try_send never blocks.
        let (wake_tx, wake_rx) = async_channel::bounded::<()>(1);
        let client = DaemonClient::start(
            &rt,
            socket.clone(),
            Arc::new(move || {
                let _ = wake_tx.try_send(());
            }),
            !suppress_auto_create,
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
        let orch_socket = socket.clone();
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
                        let out = w.tick(&input);
                        (w, out)
                    })
                    .await;
                watcher = Some(w);
                let due = std::mem::take(&mut out.due);
                let Ok((should_close, should_clean)) = this.update(cx, |this, cx| {
                    this.apply_orch(out, cx);
                    let synced = this.client.terminals_synced();
                    let settings = settings::get(cx);
                    (synced && settings.orch_auto_close_workers, synced && settings.orch_cleanup_worktrees)
                }) else { break };
                if closing.as_mut().is_some_and(|close| close.now_or_never().is_some()) {
                    closing = None;
                }
                if should_close && !due.is_empty() && closing.is_none() {
                    let socket = orch_socket.clone();
                    let rt = rt.clone();
                    closing = Some(cx.background_executor().spawn(async move {
                        orch_watch::release_due(&due, &socket, &rt);
                    }));
                }
                if cleanup.as_mut().is_some_and(|task| task.now_or_never().is_some()) {
                    cleanup = None;
                }
                if should_clean && cleanup.is_none()
                    && last_cleanup.is_none_or(|t| t.elapsed() >= orch_watch::WORKTREE_CLEANUP_INTERVAL)
                {
                    last_cleanup = Some(Instant::now());
                    let socket = orch_socket.clone();
                    let rt = rt.clone();
                    cleanup = Some(cx.background_executor().spawn(async move {
                        orch_watch::cleanup_worktrees(&socket, &rt);
                    }));
                }
            }
        })
        .detach();

        Self::start_update_checks(cx);
        Self {
            client,
            views: HashMap::new(),
            diff_tabs: HashMap::new(),
            next_diff_id: 0,
            focused_diff: None,
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
            collapsed_groups: HashSet::new(),
            collapsed: HashSet::new(),
            pending_creates: VecDeque::new(),
            seen_projects_rev: 0,
            shown_error: None,
            focused: None,
            window_active: false,
            hwnd: None,
            attention: HashMap::new(),
            seen_sent: HashMap::new(),
            socket,
            events_tx: events.0,
            events_rx: Some(events.1),
            tray: None,
            tray_lang: settings::language(cx),
            saved,
            restored: false,
            suppress_auto_create,
            restore_targets: Vec::new(),
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
        orch_watch::tick_data(&infos, self.client.terminals_synced(), sich_orch::paths::now_ms(), exe, Instant::now())
    }

    /// Applies one watcher result: sidebar data, and wake-up pointers typed like a user would
    /// (a bracketed paste, then Enter a moment later).
    fn apply_orch(&mut self, out: sich_orch::watch::TickOutput, cx: &mut Context<Self>) {
        let now_ms = sich_orch::paths::now_ms();
        let countdown_changed = !self.hidden_to_tray
            && self.active_diff().is_none()
            && self.focused
                .and_then(|id| self.orch.workers.get(&id))
                .and_then(|worker| worker.auto_close_state(settings::get(cx).orch_auto_close_workers))
                .is_some_and(|state| match state {
                    AutoClose::ClosesAt(deadline) => {
                        countdown_seconds(deadline.saturating_sub(self.orch_now_ms))
                            != countdown_seconds(deadline.saturating_sub(now_ms))
                    }
                    _ => false,
                });
        self.orch_now_ms = now_ms;
        let snapshot_changed = out.snapshot.is_some();
        if let Some(snapshot) = out.snapshot {
            self.orch = snapshot;
        }
        if snapshot_changed || countdown_changed {
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

    fn on_wake(&mut self, cx: &mut Context<Self>) {
        self.finish_daemon_restart();
        for view in self.views.values() {
            view.update(cx, |v, cx| v.refresh(cx));
        }
        let snap = self.client.snapshot();
        self.track_agents(&snap);
        // The window may be hidden in the tray (no renders), so keep these going from here too.
        let infos: Vec<TerminalInfo> = snap.terminals.iter().map(|t| t.info.clone()).collect();
        self.after_state_change(&infos, cx);
        cx.notify();
    }

    /// Hooks the window up: focus tracking (mark seen on focus, flash only in the background),
    /// the native handle (taskbar flash, hide to tray), close interception, the tray icon and
    /// window-bounds tracking for the session.
    pub fn watch_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.observe(&self.git_panel, |this, panel, cx| {
            let collapsed = panel.read(cx).history_collapsed();
            if this.right_panel.history_collapsed != collapsed {
                this.right_panel.history_collapsed = collapsed;
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
            this.track_agents(&snap);
            cx.notify();
        })
        .detach();
    }

    /// Agent status side effects: taskbar flash on a transition into "needs the user" while the
    /// window is in the background, and `mark_seen` for the visible terminal.
    fn track_agents(&mut self, snap: &Snapshot) {
        let infos: Vec<TerminalInfo> = snap.terminals.iter().map(|t| t.info.clone()).collect();
        let ws_ids = self.workspace_ids(&infos);
        let active = self.ws.active_in(&self.ws.selected, &ws_ids);
        let mut flash = false;
        for t in &snap.terminals {
            let id = t.info.id;
            let status = t.info.agent.as_ref();
            let now = kind_of(status);
            // A terminal seen for the first time only seeds the state (no flash at startup).
            if let Some(prev) = self.attention.insert(id, now) {
                flash |= !self.window_active && should_flash(prev, now);
            }
            if should_mark_seen(active == Some(id) && self.active_diff().is_none(), self.window_active, status) {
                let since = status.map_or(0, |s| s.since_unix_ms);
                if self.seen_sent.get(&id) != Some(&since) {
                    self.seen_sent.insert(id, since);
                    self.client.mark_seen(id);
                }
            }
        }
        let alive: Vec<TermId> = infos.iter().map(|t| t.id).collect();
        self.attention.retain(|id, _| alive.contains(id));
        self.seen_sent.retain(|id, _| alive.contains(id));
        if flash {
            if let Some(hwnd) = self.hwnd {
                win_flash::flash(hwnd);
            }
        }
    }

    // ---- tray, quit ------------------------------------------------------------------------

    fn on_ui_event(&mut self, ev: UiEvent, window: &mut Window, cx: &mut Context<Self>) {
        match ev {
            UiEvent::Show => self.show_window(cx),
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
    fn start_update_checks(cx: &mut Context<Self>) {
        if !updater::OFFICIAL {
            return;
        }
        if let Some(dir) = updater::install_dir() {
            cx.background_executor().spawn(async move { updater::cleanup_old(&dir) }).detach();
        }
        cx.spawn(async move |this, cx| {
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
                    if this.update(cx, |this, cx| this.apply_check(&found, cx)).is_err() {
                        break;
                    }
                }
                cx.background_executor().timer(updater::CHECK_INTERVAL).await;
            }
        })
        .detach();
    }

    /// Records a check result: sets the pending release (unless an update flow is running)
    /// and the status shown in settings. Returns the release when a newer one exists.
    fn apply_check(&mut self, result: &anyhow::Result<Option<updater::Release>>, cx: &mut Context<Self>) -> Option<updater::Release> {
        let release = match result {
            Ok(release) => {
                if self.update_flow.is_none() {
                    self.update_available = release.clone();
                    cx.notify();
                }
                release.clone()
            }
            Err(_) => None,
        };
        let status = match result {
            Ok(Some(r)) => CheckStatus::Available(r.version.to_string()),
            Ok(None) => CheckStatus::UpToDate,
            Err(err) => CheckStatus::Failed(format!("{err:#}")),
        };
        update_status::set(status, cx);
        release
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
                if let Some(release) = this.apply_check(&found, cx) {
                    // Dialogs can't coexist: close settings, then show the update dialog.
                    window.close_dialog(cx);
                    this.update_available = Some(release);
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
        if let Err(e) = crate::cli::stop_daemon(&self.socket) {
            tracing::warn!("could not stop the daemon: {e:#}");
        }
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
        let session = match snap.status {
            ConnStatus::Connected { .. } if self.session_ready() => {
                let infos: Vec<_> = snap.terminals.into_iter().map(|t| t.info).collect();
                Some(self.build_session(&infos))
            }
            ConnStatus::Incompatible { .. } => None,
            _ => return,
        };
        self.restarting_daemon = true;
        self.pending_save = None;
        self.client.restart_daemon(crate::daemon_restart::Request { expected: snap.status, session });
        cx.notify();
    }

    fn finish_daemon_restart(&mut self) {
        let Some(result) = self.client.take_restart_result() else { return };
        self.restarting_daemon = false;
        self.restart_dialog_shown = false;
        self.last_session = None;
        if let RestartResult::Stopped(session) = result {
            self.suppress_auto_create = session.has_terminals();
            self.restore = Some(session.selected.clone());
            self.saved = Some(session);
            self.restored = false;
            self.restore_targets.clear();
            self.restore_started = None;
            self.pending_creates.clear();
            self.views.clear();
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
            projects: &self.projects,
            groups: &self.groups,
            collapsed: &self.collapsed,
            collapsed_groups: &self.collapsed_groups,
            window: self.window_state,
            sidebar_w: Some(self.sidebar_w),
            right_panel: self.right_panel,
        });
        session.daemon_pid = self.client.daemon_pid();
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
        let plan = match self.saved.take() {
            Some(saved) if count > 0 => {
                for workspace in saved.workspaces {
                    let key = WorkspaceKey::of(workspace.worktree.as_deref());
                    let saved_id = workspace.active.and_then(|index| workspace.terminals.get(index)).and_then(|term| term.id);
                    let ids = terminals_in(&key, infos);
                    if saved.daemon_pid == self.client.daemon_pid() {
                        if let Some(id) = saved_id.filter(|id| ids.contains(id)) {
                            self.ws.set_active(&key, id);
                        }
                    }
                }
                Vec::new()
            }
            Some(saved) => session::restore_plan(&saved, count, &self.projects),
            None => Vec::new(),
        };
        if plan.is_empty() {
            if self.suppress_auto_create && count == 0 {
                // Nothing restorable: behave like a fresh start.
                self.client.create_terminal_in(DEFAULT_COLS, DEFAULT_ROWS, None);
            }
            return;
        }
        let lang = settings::language(cx);
        let resume_agents = settings::get(cx).resume_agents_on_restore;
        for (key, terminals, active) in plan {
            let worktree = key.path().map(PathBuf::from);
            self.restore_targets.push(RestoreTarget { key, expected: terminals.len(), active });
            for term in terminals {
                let resume = term.agent.filter(|_| resume_agents)
                    .and_then(|a| session::resume_command(a.kind, &a.session_id));
                let data = RestoreData {
                    scrollback: term.scrollback,
                    separator: tr(lang, Tr::RestoredScrollback),
                    interruption: term.interrupted.then(|| tr(lang, Tr::AgentInterrupted)),
                    resume,
                };
                let cwd = (!term.cwd.as_os_str().is_empty()).then_some(term.cwd);
                self.client.restore_terminal(worktree.clone(), cwd, data);
            }
        }
        self.restore_started = Some(Instant::now());
    }

    /// Activates the saved tab in each restored workspace once all its terminals exist.
    fn finish_restore_targets(&mut self, infos: &[TerminalInfo]) {
        if self.restore_targets.is_empty() {
            return;
        }
        let timed_out = self.restore_started.is_some_and(|t| t.elapsed() > RESTORE_TIMEOUT);
        let ws = &mut self.ws;
        self.restore_targets.retain(|t| {
            let ids = terminals_in(&t.key, infos);
            if ids.len() < t.expected && !timed_out {
                return true;
            }
            if let Some(id) = t.active.and_then(|a| ids.get(a)) {
                ws.set_active(&t.key, *id);
            }
            false
        });
    }

    /// Everything that follows terminal / selection changes: restore progress, tray and session.
    fn after_state_change(&mut self, infos: &[TerminalInfo], cx: &mut Context<Self>) {
        self.finish_restore_targets(infos);
        self.update_tray(infos, cx);
        self.touch_session(infos, cx);
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
            self.sync_right_worktree(cx);
            self.sync_diff_highlight(cx);
        }
        cx.notify();
    }

    pub(crate) fn select_workspace(&mut self, key: WorkspaceKey, _window: &mut Window, cx: &mut Context<Self>) {
        self.set_selected(key, cx);
    }

    pub(crate) fn activate_terminal(&mut self, id: TermId, window: &mut Window, cx: &mut Context<Self>) {
        let key = self.ws.selected.clone();
        self.ws.set_active(&key, id);
        self.deactivate_diff(&key, cx);
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

    pub(crate) fn workspace_diffs(&self) -> Option<&DiffTabs<Entity<DiffView>>> {
        self.diff_tabs.get(&self.ws.selected)
    }

    fn active_diff(&self) -> Option<&DiffTab<Entity<DiffView>>> {
        self.workspace_diffs()?.active()
    }

    fn sync_diff_highlight(&self, cx: &mut Context<Self>) {
        let key = self.active_diff().map(|tab| tab.key.clone());
        self.git_panel.update(cx, |panel, cx| panel.set_active_diff(key, cx));
    }

    fn deactivate_diff(&mut self, key: &WorkspaceKey, cx: &mut Context<Self>) {
        if let Some(tabs) = self.diff_tabs.get_mut(key) {
            tabs.active = None;
        }
        self.focused_diff = None;
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
                if let Some(tabs) = self.diff_tabs.get(&WorkspaceKey::of(Some(worktree))) {
                    for tab in &tabs.tabs {
                        if let GitPanelEvent::Mutated { diff_tab: Some(id), error, .. } = event {
                            if tab.id == *id {
                                tab.view.update(cx, |view, cx| view.mutation_finished(error.clone(), cx));
                            }
                        }
                        if tab.key.source.is_live() {
                            let rename = self.git_panel.read(cx).diff_rename(worktree, &tab.key);
                            tab.view.update(cx, |view, cx| view.refresh_live(rename, cx));
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
        let id = self.next_diff_id;
        let panel = self.git_panel.clone();
        let tabs = self.diff_tabs.entry(self.ws.selected.clone()).or_default();
        let tab = tabs.open(id, key.clone(), || {
            cx.new(|cx| DiffView::new(id, worktree, repo, key, original, panel, cx))
        });
        let opened = tab.id;
        if opened == id {
            self.next_diff_id += 1;
            tab.view.update(cx, DiffView::reload);
        }
        self.activate_diff(opened, window, cx);
    }

    pub(crate) fn activate_diff(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tabs) = self.diff_tabs.get_mut(&self.ws.selected) {
            tabs.activate(id);
        }
        self.focus_diff(window, cx);
        self.sync_diff_highlight(cx);
        cx.notify();
    }

    pub(crate) fn close_diff(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(tabs) = self.diff_tabs.get_mut(&self.ws.selected) {
            tabs.close(id);
        }
        self.sync_diff_highlight(cx);
        cx.notify();
    }

    fn focus_diff(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab) = self.active_diff() {
            let id = tab.id;
            let view = tab.view.clone();
            // The tab bar measures terminal children before diff children on the next frame.
            let snapshot = self.client.snapshot();
            let index = snapshot.terminals.iter().filter(|term| self.ws.selected.contains(term.info.worktree.as_deref())).count()
                + self.workspace_diffs().and_then(|tabs| tabs.tabs.iter().position(|tab| tab.id == id)).unwrap_or(0);
            self.tab_scroll.scroll_to_item(index);
            self.focused_diff = Some(id);
            self.focused = None;
            view.update(cx, |view, cx| view.focus(window, cx));
        }
    }

    fn workspace_ids(&self, infos: &[TerminalInfo]) -> Vec<TermId> {
        terminals_in(&self.ws.selected, infos)
    }

    // ---- commands --------------------------------------------------------------------------

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
            if let Some(pos) = self.pending_creates.iter().position(|k| *k == key) {
                self.pending_creates.remove(pos);
                self.ws.set_active(&key, id);
                self.set_selected(key.clone(), cx);
                self.deactivate_diff(&key, cx);
                self.focused = None;
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
        self.finish_restore_targets(infos);

        if let Some(id) = self.active_diff().map(|tab| tab.id) {
            if self.focused_diff != Some(id) {
                self.focus_diff(window, cx);
            }
        } else {
            self.focused_diff = None;
            let ws_ids = self.workspace_ids(infos);
            let active = self.ws.active_in(&self.ws.selected, &ws_ids);
            if active != self.focused {
                self.focused = active;
                if let Some(view) = active.and_then(|a| self.views.get(&a)) {
                    view.update(cx, |v, cx| v.focus(window, cx));
                }
            }
        }
        self.track_agents(snap);
        self.update_tray(infos, cx);
        self.touch_session(infos, cx);
    }

    fn on_projects_changed(&mut self, created: Vec<(PathBuf, PathBuf)>, cx: &mut Context<Self>) {
        self.diff_tabs.retain(|key, _| workspace_exists(key, &self.projects));
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

    fn render_main(
        &self,
        snap: &Snapshot,
        infos: &[TerminalInfo],
        lang: i18n::Language,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let ids = self.workspace_ids(infos);
        let diff = self.active_diff();
        let active = if diff.is_none() { self.ws.active_in(&self.ws.selected, &ids) } else { None };
        let tab_bar = self.render_tab_bar(snap, &ids, active, lang, cx);
        let content = match (diff, active.and_then(|a| self.views.get(&a).map(|view| (a, view)))) {
            (Some(tab), _) => div().flex_1().min_h_0().w_full().child(tab.view.clone()),
            (None, Some((id, view))) => {
                let banner = self.orch.workers.get(&id)
                    .filter(|worker| infos.iter().any(|t| t.id == id && t.created_unix_ms as i64 <= worker.started_ms))
                    .and_then(|worker| crate::orch_banner::render(worker, self.orch_now_ms, lang, cx));
                div().relative().flex_1().min_h_0().w_full().child(view.clone()).children(banner)
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
        let status = status_line(
            lang,
            &snap.status,
            self.restarting_daemon,
            snap.daemon_build.as_deref(),
            sich_app::daemon_build_id(),
        );
        let old_build = matches!(&status, StatusLine::Text { old_build: true, .. });
        let indicator = match status {
            StatusLine::Dot { tooltip } => {
                div()
                    .id("daemon-connected")
                    .size(px(16.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                    .child(div().size(px(8.)).rounded_full().bg(theme.success))
                    .into_any_element()
            }
            StatusLine::Text { text, .. } => text.into_any_element(),
        };
        let restart = old_build && self.session_ready();
        let app = cx.entity();
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
            .text_color(if old_build { theme.warning } else { theme.muted_foreground })
            .child(div().flex_1().child(indicator))
            .children(self.update_available.as_ref().map(|release| {
                let app = app.clone();
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .text_color(theme.muted_foreground)
                            .child(i18n::update_available(lang, &release.version.to_string())),
                    )
                    .child(Button::new("update-available").xsmall().label(tr(lang, Tr::Update)).on_click(
                        move |_, window, cx| update_dialog::open(app.clone(), window, cx),
                    ))
            }))
            .children(restart.then(|| {
                Button::new("restart-daemon").xsmall().label(tr(lang, Tr::RestartDaemon)).on_click(
                    move |_, window, cx| {
                        worktree_dialogs::open_restart_daemon(app.clone(), window, cx)
                    },
                )
            }))
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

#[derive(Debug, PartialEq, Eq)]
enum StatusLine {
    Dot { tooltip: String },
    Text { text: String, old_build: bool },
}

fn status_line(
    lang: i18n::Language,
    status: &ConnStatus,
    restarting: bool,
    daemon_build: Option<&str>,
    own_build: &str,
) -> StatusLine {
    let old_build = matches!(status, ConnStatus::Connected { .. })
        && daemon_build.is_some_and(|id| id != own_build);
    let text = if restarting {
        tr(lang, Tr::DaemonRestarting).to_string()
    } else {
        match *status {
            ConnStatus::Connected { .. } if old_build => tr(lang, Tr::DaemonOlderBuild).to_string(),
            ConnStatus::Connected { pid } => {
                return StatusLine::Dot {
                    tooltip: i18n::connected(lang, pid),
                };
            }
            ConnStatus::Reconnecting => tr(lang, Tr::Reconnecting).to_string(),
            ConnStatus::Incompatible { version, .. } => {
                i18n::daemon_incompatible(lang, version, PROTO_VERSION)
            }
        }
    };
    StatusLine::Text { text, old_build }
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

#[cfg(test)]
mod tests {
    use super::*;

    const LANG: i18n::Language = i18n::Language::En;
    const CONNECTED: ConnStatus = ConnStatus::Connected { pid: 12345 };
    const INCOMPATIBLE: ConnStatus = ConnStatus::Incompatible {
        pid: None,
        version: None,
    };

    #[test]
    fn connected_with_matching_build_shows_dot_with_pid() {
        assert_eq!(
            status_line(LANG, &CONNECTED, false, Some("current"), "current"),
            StatusLine::Dot {
                tooltip: "Connected to daemon (pid 12345)".into(),
            },
        );
    }

    #[test]
    fn connected_with_different_build_shows_warning() {
        assert_eq!(
            status_line(LANG, &CONNECTED, false, Some("old"), "current"),
            StatusLine::Text {
                text: tr(LANG, Tr::DaemonOlderBuild).into(),
                old_build: true,
            },
        );
    }

    #[test]
    fn connected_without_build_info_shows_dot() {
        assert_eq!(
            status_line(LANG, &CONNECTED, false, None, "current"),
            StatusLine::Dot {
                tooltip: i18n::connected(LANG, 12345),
            },
        );
    }

    #[test]
    fn restarting_takes_precedence_and_preserves_old_build_warning() {
        for (status, build, old_build) in [
            (CONNECTED, Some("old"), true),
            (CONNECTED, Some("current"), false),
            (ConnStatus::Reconnecting, Some("old"), false),
            (INCOMPATIBLE, Some("old"), false),
        ] {
            assert_eq!(
                status_line(LANG, &status, true, build, "current"),
                StatusLine::Text {
                    text: tr(LANG, Tr::DaemonRestarting).into(),
                    old_build,
                },
            );
        }
    }

    #[test]
    fn reconnecting_shows_muted_text() {
        assert_eq!(
            status_line(LANG, &ConnStatus::Reconnecting, false, Some("old"), "current"),
            StatusLine::Text {
                text: tr(LANG, Tr::Reconnecting).into(),
                old_build: false,
            },
        );
    }

    #[test]
    fn incompatible_shows_muted_text_with_or_without_version() {
        for version in [None, Some(PROTO_VERSION - 1)] {
            let status = ConnStatus::Incompatible { pid: None, version };
            assert_eq!(
                status_line(LANG, &status, false, Some("old"), "current"),
                StatusLine::Text {
                    text: i18n::daemon_incompatible(LANG, version, PROTO_VERSION),
                    old_build: false,
                },
            );
        }
    }
}
