//! Main window: sidebar (projects/worktrees), tab bar, active terminal, status line.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AppContext as _, ClickEvent, Context, Entity, InteractiveElement as _, IntoElement,
    KeyDownEvent, ParentElement as _, Render, Styled as _, Window,
    div, px,
};
use sich_proto::{PROTO_VERSION, ProjectId, ProjectInfo, TermId, TerminalInfo};

use crate::agent_badge::{BadgeKind, kind_of, should_flash, should_mark_seen};
use crate::daemon_client::{ConnStatus, DEFAULT_COLS, DEFAULT_ROWS, DaemonClient, Snapshot};
use crate::i18n::{self, Tr, tr};
use crate::notifications;
use crate::settings;
use crate::settings_dialog;
use crate::view::TerminalView;
use crate::workspace::{
    PendingWorktree, WorkspaceKey, Workspaces, cycle_id,
    match_pending_worktree, restore_selection, terminals_in, workspace_exists,
};
use crate::win_flash;
use crate::worktree_dialogs;

/// Upper bound for creations waiting to be matched with a `TerminalCreated`.
const MAX_PENDING_CREATES: usize = 8;

pub struct AppView {
    pub(crate) client: DaemonClient,
    views: HashMap<TermId, Entity<TerminalView>>,
    pub(crate) ws: Workspaces,
    /// Saved workspace to restore when the first `Projects` update arrives.
    restore: Option<Option<PathBuf>>,
    pub(crate) projects: Vec<ProjectInfo>,
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
}

impl AppView {
    pub fn new(rt: tokio::runtime::Handle, socket: String, cx: &mut Context<Self>) -> Self {
        // Coalescing wake path: bounded(1), try_send never blocks.
        let (wake_tx, wake_rx) = async_channel::bounded::<()>(1);
        let client = DaemonClient::start(
            &rt,
            socket,
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

        Self {
            client,
            views: HashMap::new(),
            ws: Workspaces::default(),
            restore: Some(settings::get(cx).last_workspace),
            projects: Vec::new(),
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
        }
    }

    fn on_wake(&mut self, cx: &mut Context<Self>) {
        for view in self.views.values() {
            view.update(cx, |v, cx| v.refresh(cx));
        }
        let snap = self.client.snapshot();
        self.track_agents(&snap);
        cx.notify();
    }

    /// Hooks the window up: focus tracking (mark seen on focus, flash only in the background)
    /// and the native handle for the taskbar flash.
    pub fn watch_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.window_active = window.is_window_active();
        self.hwnd = win_flash::hwnd_of(window);
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

    // ---- selection -------------------------------------------------------------------------

    /// Selects `key` and persists the choice.
    fn set_selected(&mut self, key: WorkspaceKey, cx: &mut Context<Self>) {
        if self.ws.selected != key {
            // Make sure the selected worktree is visible: expand its project.
            if let WorkspaceKey::Worktree(_) = &key {
                for p in &self.projects {
                    if p.worktrees.iter().any(|w| WorkspaceKey::Worktree(w.path.clone()) == key) {
                        self.collapsed.remove(&p.id);
                    }
                }
            }
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

        let (rev, projects) = self.client.projects_with_rev();
        if rev != self.seen_projects_rev {
            self.seen_projects_rev = rev;
            self.projects = projects;
            self.on_projects_changed(cx);
        }

        let ws_ids = self.workspace_ids(infos);
        let active = self.ws.active_in(&self.ws.selected, &ws_ids);
        if active != self.focused {
            self.focused = active;
            if let Some(view) = active.and_then(|a| self.views.get(&a)) {
                view.update(cx, |v, cx| v.focus(window, cx));
            }
        }
        self.track_agents(snap);
    }

    fn on_projects_changed(&mut self, cx: &mut Context<Self>) {
        if let Some(saved) = self.restore.take() {
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

        let sidebar = self.render_sidebar(&infos, lang, cx);
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
