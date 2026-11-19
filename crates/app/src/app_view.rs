//! Main window: sidebar (projects/worktrees), tab bar, active terminal, status line.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use futures::StreamExt as _;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::{
    AppContext as _, ClickEvent, Context, Entity, InteractiveElement as _, IntoElement,
    KeyDownEvent, ParentElement as _, Render, Styled as _, Window, WindowBounds,
    div, px,
};
use sich_proto::{GroupId, GroupInfo, PROTO_VERSION, ProjectId, ProjectInfo, TermId, TerminalInfo};

use crate::agent_badge::{BadgeKind, kind_of, should_flash, should_mark_seen};
use crate::daemon_client::{ConnStatus, DEFAULT_COLS, DEFAULT_ROWS, DaemonClient, Snapshot};
use crate::i18n::{self, Tr, tr};
use crate::notifications;
use crate::session::{self, SavedWindow, Session, SessionInput};
use crate::settings;
use crate::settings_dialog;
use crate::tray::{self, Tray, UiEvent};
use crate::view::TerminalView;
use crate::workspace::{
    PendingWorktree, WorkspaceKey, Workspaces, cycle_id,
    match_pending_worktree, restore_selection, terminals_in, workspace_exists,
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
    pub(crate) ws: Workspaces,
    /// Saved workspace to restore when the first `Projects` update arrives.
    restore: Option<Option<PathBuf>>,
    pub(crate) projects: Vec<ProjectInfo>,
    pub(crate) groups: Vec<GroupInfo>,
    /// Groups whose project list is collapsed (default: expanded).
    pub(crate) collapsed_groups: HashSet<GroupId>,
    /// Projects whose tree is collapsed (default: expanded).
    pub(crate) collapsed: HashSet<ProjectId>,
    /// Workspaces this window asked a terminal for; the matching new terminal gets activated.
    pending_creates: VecDeque<WorkspaceKey>,
    pending_worktree: Option<PendingWorktree>,
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
}

impl AppView {
    pub fn new(
        rt: tokio::runtime::Handle,
        socket: String,
        events: (UnboundedSender<UiEvent>, UnboundedReceiver<UiEvent>),
        saved: Option<Session>,
        cx: &mut Context<Self>,
    ) -> Self {
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

        Self {
            client,
            views: HashMap::new(),
            ws: Workspaces::default(),
            // The session's selection wins over the older `last_workspace` setting.
            restore: Some(match &saved {
                Some(s) => s.selected.clone(),
                None => settings::get(cx).last_workspace,
            }),
            projects: Vec::new(),
            groups: Vec::new(),
            collapsed_groups: HashSet::new(),
            collapsed: HashSet::new(),
            pending_creates: VecDeque::new(),
            pending_worktree: None,
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
        }
    }

    fn on_wake(&mut self, cx: &mut Context<Self>) {
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
        self.window_active = window.is_window_active();
        self.hwnd = win_flash::hwnd_of(window);
        self.window_state = saved_window_of(window);

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
            this.window_state = saved_window_of(window).or(this.window_state);
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
            this.window_active = window.is_window_active();
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
            if should_mark_seen(active == Some(id), self.window_active, status) {
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
            UiEvent::Quit => self.request_quit(window, cx),
        }
    }

    fn show_window(&mut self, cx: &mut Context<Self>) {
        if let Some(hwnd) = self.hwnd {
            win_show::show(hwnd);
        }
        cx.activate(true);
        cx.notify();
    }

    /// The window's close button / Alt+F4: hide to the tray, or quit.
    fn on_close_requested(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.hwnd {
            Some(hwnd) if self.tray.is_some() && settings::get(cx).close_to_tray => win_show::hide(hwnd),
            _ => self.request_quit(window, cx),
        }
    }

    /// Quits, after a confirmation when agents are still busy.
    fn request_quit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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

    /// Saves the session, stops the daemon (and with it every terminal) and exits.
    fn quit_now(&mut self, cx: &mut Context<Self>) {
        let infos: Vec<TerminalInfo> =
            self.client.snapshot().terminals.iter().map(|t| t.info.clone()).collect();
        if self.session_ready() {
            let session = self.build_session(&infos);
            if let Err(e) = session.save() {
                tracing::error!("failed to save session: {e:#}");
            }
        }
        if let Err(e) = crate::cli::stop_daemon(&self.socket) {
            tracing::warn!("could not stop the daemon: {e:#}");
        }
        self.tray = None;
        cx.quit();
    }

    // ---- session ---------------------------------------------------------------------------

    /// True once the saved state has been applied; before that the live state is incomplete and
    /// must not overwrite the file on disk.
    fn session_ready(&self) -> bool {
        self.restored && self.restore.is_none() && self.restore_targets.is_empty()
    }

    fn build_session(&self, infos: &[TerminalInfo]) -> Session {
        session::build_session(&SessionInput {
            terminals: infos,
            ws: &self.ws,
            projects: &self.projects,
            groups: &self.groups,
            collapsed: &self.collapsed,
            collapsed_groups: &self.collapsed_groups,
            window: self.window_state,
        })
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
    fn maybe_restore_terminals(&mut self) {
        if self.restored || self.restore.is_some() {
            return;
        }
        let Some(count) = self.client.first_list_count() else { return };
        self.restored = true;
        let plan = match self.saved.take() {
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
        for (key, cwds, active) in plan {
            let worktree = key.path().map(PathBuf::from);
            for cwd in &cwds {
                let cwd = (!cwd.as_os_str().is_empty()).then(|| cwd.clone());
                self.client.create_terminal_at(DEFAULT_COLS, DEFAULT_ROWS, worktree.clone(), cwd);
            }
            self.restore_targets.push(RestoreTarget { key, expected: cwds.len(), active });
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
        }
        cx.notify();
    }

    pub(crate) fn select_workspace(&mut self, key: WorkspaceKey, _window: &mut Window, cx: &mut Context<Self>) {
        self.set_selected(key, cx);
    }

    pub(crate) fn activate_terminal(&mut self, id: TermId, window: &mut Window, cx: &mut Context<Self>) {
        let key = self.ws.selected.clone();
        self.ws.set_active(&key, id);
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
        self.pending_worktree = Some(PendingWorktree { project_id, name: name.clone() });
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
            "," if !m.shift => settings_dialog::open(window, cx),
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
            if self.views.contains_key(&id) {
                continue;
            }
            let session = entry.session.clone();
            let view = cx.new(|cx| TerminalView::new(session, window, cx));
            self.views.insert(id, view);
            // A terminal this window asked for: show its workspace and activate it.
            let key = WorkspaceKey::of(entry.info.worktree.as_deref());
            if let Some(pos) = self.pending_creates.iter().position(|k| *k == key) {
                self.pending_creates.remove(pos);
                self.ws.set_active(&key, id);
                self.set_selected(key, cx);
                self.focused = None;
            }
        }
        let ids: Vec<TermId> = infos.iter().map(|t| t.id).collect();
        self.views.retain(|id, _| ids.contains(id));
        self.ws.prune(&ids);
        self.window_active = window.is_window_active();

        let (rev, projects, groups) = self.client.projects_with_rev();
        if rev != self.seen_projects_rev {
            self.seen_projects_rev = rev;
            self.projects = projects;
            self.groups = groups;
            self.on_projects_changed(cx);
        }

        self.maybe_restore_terminals();
        self.finish_restore_targets(infos);

        let ws_ids = self.workspace_ids(infos);
        let active = self.ws.active_in(&self.ws.selected, &ws_ids);
        if active != self.focused {
            self.focused = active;
            if let Some(view) = active.and_then(|a| self.views.get(&a)) {
                view.update(cx, |v, cx| v.focus(window, cx));
            }
        }
        self.track_agents(snap);
        self.update_tray(infos, cx);
        self.touch_session(infos, cx);
    }

    fn on_projects_changed(&mut self, cx: &mut Context<Self>) {
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
        if let Some(pending) = self.pending_worktree.clone() {
            if let Some(path) = match_pending_worktree(&pending, &self.projects) {
                self.pending_worktree = None;
                let key = WorkspaceKey::Worktree(path);
                self.set_selected(key.clone(), cx);
                self.new_terminal_in(key, cx);
            }
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
        let active = self.ws.active_in(&self.ws.selected, &ids);
        let tab_bar = self.render_tab_bar(snap, &ids, active, lang, cx);
        let content = match active.and_then(|a| self.views.get(&a)) {
            Some(view) => div().flex_1().min_h_0().w_full().child(view.clone()),
            None => div()
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

    fn render_status_bar(
        &self,
        status: ConnStatus,
        lang: i18n::Language,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let (text, restart_pid) = match status {
            ConnStatus::Connected { pid } => (i18n::connected(lang, pid), None),
            ConnStatus::Reconnecting => (tr(lang, Tr::Reconnecting).to_string(), None),
            ConnStatus::Incompatible { pid, version } => {
                (i18n::daemon_incompatible(lang, version, PROTO_VERSION), Some(pid))
            }
        };
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
            .text_color(if restart_pid.is_some() { theme.warning } else { theme.muted_foreground })
            .child(div().flex_1().child(text))
            .children(restart_pid.map(|pid| {
                Button::new("restart-daemon").xsmall().label(tr(lang, Tr::RestartDaemon)).on_click(
                    move |_, window, cx| {
                        worktree_dialogs::open_restart_daemon(app.clone(), pid, window, cx)
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
                        settings_dialog::open(window, cx)
                    })),
            )
    }
}

/// Window bounds for the session (logical pixels); `None` when they look bogus.
fn saved_window_of(window: &Window) -> Option<SavedWindow> {
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
}

impl Render for AppView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let snap = self.client.snapshot();
        let infos: Vec<TerminalInfo> = snap.terminals.iter().map(|t| t.info.clone()).collect();
        self.sync(&snap, &infos, window, cx);
        let theme = cx.theme().clone();
        let lang = settings::language(cx);

        if let Some(msg) = notifications::take_new_error(self.client.last_error(), &mut self.shown_error) {
            cx.defer_in(window, move |_, window, cx| notifications::show_error(msg, window, cx));
        }

        let sidebar = self.render_sidebar(&snap, &infos, lang, cx);
        let main = self.render_main(&snap, &infos, lang, cx);
        let status_bar = self.render_status_bar(snap.status, lang, cx);

        v_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .capture_key_down(cx.listener(Self::on_key_capture))
            .child(h_flex().flex_1().min_h_0().child(sidebar).child(main))
            .child(status_bar)
    }
}
