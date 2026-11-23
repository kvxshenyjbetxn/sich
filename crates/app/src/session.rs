//! Persistent UI session (`<config>/sich/session.json`): what the window looked like when Sich
//! was last quit, so a full quit (daemon stopped, terminals gone) can be restored.
//!
//! Everything except the disk access is pure and unit-tested: building a `Session` from the
//! live state, planning the terminal restore, resolving collapse state and window placement.

use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sich_proto::{AgentKind, AgentState, GroupId, GroupInfo, ProjectId, ProjectInfo, TermId, TerminalInfo};

use crate::workspace::{WorkspaceKey, Workspaces, same_path, terminals_in, workspace_exists};
use crate::daemon_client::TerminalOwner;
use crate::terminal_layout::Layout;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    /// Selected worktree; `None` = Home.
    pub selected: Option<PathBuf>,
    pub daemon_pid: Option<u32>,
    pub workspaces: Vec<SavedWorkspace>,
    /// Whether the Home terminal list is collapsed (expanded in older sessions).
    pub home_collapsed: bool,
    /// Collapsed projects by path (ids may change between daemon runs).
    pub collapsed_projects: Vec<PathBuf>,
    /// Collapsed groups by name.
    pub collapsed_groups: Vec<String>,
    pub window: Option<SavedWindow>,
    /// Sidebar width in logical pixels; `None` = default. Garbage reads as `None`.
    #[serde(deserialize_with = "lenient_width")]
    pub sidebar_w: Option<f32>,
    pub right_panel: RightPanelState,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RightTab {
    #[default]
    Git,
    Pr,
    /// Claude context usage of the focused agent terminal.
    Context,
    /// Every image seen in the selected Claude session.
    Images,
}

impl RightTab {
    pub const ALL: &'static [Self] = &[Self::Git, Self::Pr, Self::Context, Self::Images];

    /// The tab to actually show: the "Pull request" tab exists only while the PR section does,
    /// otherwise the panel falls back to Git (the saved choice is kept for when it returns).
    pub fn effective(self, pr_visible: bool) -> Self {
        match self {
            Self::Pr if !pr_visible => Self::Git,
            tab => tab,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(from = "serde_json::Value")]
pub struct RightPanelState {
    pub width: Option<f32>,
    pub open: bool,
    pub tab: RightTab,
    pub history_collapsed: bool,
    /// Explicit commit-history height; `None` keeps the responsive 40% default.
    pub history_height: Option<f32>,
}

impl Default for RightPanelState {
    fn default() -> Self {
        Self { width: None, open: true, tab: RightTab::default(), history_collapsed: false, history_height: None }
    }
}

impl From<serde_json::Value> for RightPanelState {
    fn from(value: serde_json::Value) -> Self {
        let default = Self::default();
        Self {
            width: width_value(&value["width"]),
            open: value["open"].as_bool().unwrap_or(default.open),
            tab: serde_json::from_value(value["tab"].clone()).unwrap_or_default(),
            history_collapsed: value["history_collapsed"].as_bool().unwrap_or(default.history_collapsed),
            history_height: width_value(&value["history_height"]),
        }
    }
}

/// A positive finite number, anything else (wrong type, negative, zero, huge) is `None`.
fn lenient_width<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<f32>, D::Error> {
    Ok(width_value(&serde_json::Value::deserialize(d)?))
}

fn width_value(value: &serde_json::Value) -> Option<f32> {
    value.as_f64().filter(|w| w.is_finite() && *w > 0. && *w <= f32::MAX as f64).map(|w| w as f32)
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedWorkspace {
    /// `None` = Home.
    pub worktree: Option<PathBuf>,
    pub terminals: Vec<SavedTerminal>,
    /// Index into `terminals` of the active tab.
    pub active: Option<usize>,
    #[serde(default, deserialize_with = "read_layout", skip_serializing_if = "Option::is_none")]
    pub layout: Option<Layout<usize>>,
}

fn read_layout<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Layout<usize>>, D::Error> {
    let value = serde_json::Value::deserialize(d)?;
    Ok(serde_json::from_value(value).ok())
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedTerminal {
    pub id: Option<TermId>,
    pub socket: Option<String>,
    pub daemon_pid: Option<u32>,
    pub created_ms: Option<u64>,
    pub cwd: PathBuf,
    #[serde(deserialize_with = "read_saved_agent")]
    pub agent: Option<SavedAgent>,
    pub scrollback: Option<String>,
    pub interrupted: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedAgent {
    pub kind: AgentKind,
    pub session_id: String,
}

fn read_saved_agent<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<SavedAgent>, D::Error> {
    let value = serde_json::Value::deserialize(d)?;
    Ok(serde_json::from_value(value).ok())
}

/// Produces only supported official CLI invocations. A leading dash would be a CLI option.
pub fn resume_command(kind: AgentKind, session_id: &str) -> Option<String> {
    if !(8..=64).contains(&session_id.len())
        || session_id.starts_with('-')
        || !session_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return None;
    }
    let cmd = match kind {
        AgentKind::Claude => "claude --resume",
        AgentKind::Codex => "codex resume",
        _ => return None,
    };
    Some(format!("{cmd} {session_id}"))
}

impl SavedTerminal {
    fn from_info(info: &TerminalInfo) -> Self {
        let agent = info.agent.as_ref();
        Self {
            id: Some(info.id),
            cwd: info.cwd.clone(),
            agent: agent.and_then(|a| {
                let session_id = a.session_id.as_ref()?;
                resume_command(a.kind, session_id)?;
                Some(SavedAgent { kind: a.kind, session_id: session_id.clone() })
            }),
            interrupted: agent.is_some_and(|a| a.state == AgentState::Working),
            scrollback: None,
            ..Self::default()
        }
    }
}

/// Window bounds in logical pixels (`width`/`height` are the restore size when maximized).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedWindow {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub maximized: bool,
    /// Stable key of the monitor the window was on (`display_key`). Needed because logical
    /// coordinates are per-monitor on Windows, so the position alone does not name a monitor.
    pub display: Option<u64>,
}

impl Session {
    /// True when there is at least one terminal to bring back.
    #[cfg(test)]
    pub fn has_terminals(&self) -> bool {
        self.workspaces.iter().any(|w| !w.terminals.is_empty())
    }

    /// Converts the in-memory terminal keys into identities that survive a GUI takeover.
    pub(crate) fn record_owners(&mut self, owner_of: impl Fn(TermId) -> Option<TerminalOwner>) {
        for term in self.workspaces.iter_mut().flat_map(|w| &mut w.terminals) {
            let owner = term.id.and_then(&owner_of);
            term.id = owner.as_ref().map(|owner| owner.id);
            if let Some(owner) = owner {
                term.socket = Some(owner.socket);
                term.daemon_pid = Some(owner.pid);
                term.created_ms = Some(owner.created_ms);
            }
        }
    }

    /// Associates filenames with the saved IDs only when this is their daemon.
    #[cfg(test)]
    pub fn attach_scrollback(&mut self, pid: Option<u32>, files: &[(TermId, String)]) -> bool {
        self.attach_owner_scrollback(None, pid, files)
    }

    pub(crate) fn attach_owner_scrollback(
        &mut self,
        socket: Option<&str>,
        pid: Option<u32>,
        files: &[(TermId, String)],
    ) -> bool {
        let mut matched = false;
        for term in self.workspaces.iter_mut().flat_map(|w| &mut w.terminals) {
            let belongs = match term.socket.as_deref() {
                Some(owner) => Some(owner) == socket && term.daemon_pid == pid,
                None => self.daemon_pid == pid,
            };
            if pid.is_some() && belongs {
                matched = true;
                term.scrollback = files.iter().find(|(id, _)| Some(*id) == term.id).map(|(_, name)| name.clone());
            }
        }
        matched
    }

    /// Parses file contents; `None` for garbage.
    pub fn parse(text: &str) -> Option<Session> {
        match serde_json::from_str(text) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!("invalid session file: {e}");
                None
            }
        }
    }

    /// Reads a session file; `None` when it is missing or unreadable.
    pub fn load_from(path: &Path) -> Option<Session> {
        Self::parse(&std::fs::read_to_string(path).ok()?)
    }

    /// Atomic write: temp file + rename.
    pub fn save_to(&self, path: &Path) -> anyhow::Result<()> {
        let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir)?;
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        tmp.write_all(&serde_json::to_vec_pretty(self)?)?;
        tmp.persist(path)?;
        Ok(())
    }

    pub fn load() -> Option<Session> {
        Self::load_from(&file_path()?)
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = file_path().ok_or_else(|| anyhow::anyhow!("no config directory"))?;
        self.save_to(&path)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum RestoreLeaf {
    Live(TermId),
    Missing(usize),
}

/// Resolve saved tabs to live GUI keys and leave only the terminals that need recreating.
/// A live owner that no longer lists a terminal means it was closed since the last save.
pub(crate) fn reattach(
    mut session: Session,
    attached: &[(TermId, TerminalOwner)],
    peers: &[(String, u32)],
    initial_count: usize,
) -> (Session, Vec<(WorkspaceKey, TermId)>, HashMap<WorkspaceKey, Layout<RestoreLeaf>>) {
    let mut selected = Vec::new();
    let mut layouts = HashMap::new();
    for workspace in &mut session.workspaces {
        let key = WorkspaceKey::of(workspace.worktree.as_deref());
        let mut mapping = Vec::with_capacity(workspace.terminals.len());
        let mut missing = Vec::new();
        let mut active = None;
        for (index, term) in std::mem::take(&mut workspace.terminals).into_iter().enumerate() {
            let (live, owner_alive) = match term.socket.as_deref() {
                Some(socket) => {
                    let owner_alive = peers.iter().any(|(name, pid)| name == socket && Some(*pid) == term.daemon_pid);
                    let live = attached.iter().find(|(_, owner)| {
                        owner.socket == socket && Some(owner.pid) == term.daemon_pid
                            && Some(owner.id) == term.id && Some(owner.created_ms) == term.created_ms
                    });
                    (live, owner_alive)
                }
                None => {
                    let live = attached.iter().find(|(_, owner)| {
                        Some(owner.pid) == session.daemon_pid && Some(owner.id) == term.id
                    });
                    (live, initial_count > 0)
                }
            };
            if let Some((id, _)) = live {
                if workspace.active == Some(index) {
                    selected.push((key.clone(), *id));
                }
                mapping.push(Some(RestoreLeaf::Live(*id)));
            } else if !owner_alive {
                if workspace.active == Some(index) {
                    active = Some(missing.len());
                }
                mapping.push(Some(RestoreLeaf::Missing(missing.len())));
                missing.push(term);
            } else {
                mapping.push(None);
            }
        }
        if let Some(layout) = workspace.layout.take()
            .and_then(|layout| layout.remap(|index| mapping.get(index).copied().flatten()))
            .filter(|layout| matches!(layout, Layout::Split { .. }))
        {
            layouts.entry(key).or_insert(layout);
        }
        workspace.terminals = missing;
        workspace.active = active;
    }
    (session, selected, layouts)
}

pub(crate) fn file_path() -> Option<PathBuf> {
    crate::settings::config_file("session.json")
}

impl SavedWindow {
    /// From logical bounds; `None` for nonsense (non-finite, tiny, or the parked
    /// position Windows reports for minimized windows).
    pub fn from_bounds(x: f32, y: f32, width: f32, height: f32, maximized: bool) -> Option<Self> {
        let ok = [x, y, width, height].iter().all(|v| v.is_finite())
            && width >= 100.
            && height >= 100.
            && x > -10_000.
            && y > -10_000.;
        ok.then_some(SavedWindow { x, y, width, height, maximized, display: None })
    }
}

/// Folds a display UUID into the compact key stored in `SavedWindow::display`.
pub fn display_key(uuid: u128) -> u64 {
    (uuid >> 64) as u64 ^ uuid as u64
}

/// Live state a `Session` is built from.
pub struct SessionInput<'a> {
    pub terminals: &'a [TerminalInfo],
    pub ws: &'a Workspaces,
    pub layouts: &'a HashMap<WorkspaceKey, Layout<TermId>>,
    pub projects: &'a [ProjectInfo],
    pub groups: &'a [GroupInfo],
    pub home_collapsed: bool,
    pub collapsed: &'a HashSet<ProjectId>,
    pub collapsed_groups: &'a HashSet<GroupId>,
    pub window: Option<SavedWindow>,
    pub sidebar_w: Option<f32>,
    pub right_panel: RightPanelState,
}

/// Builds the session to save. Workspaces come in a stable order (Home, then worktrees in
/// project order); only workspaces that have a live (not exited) terminal are listed, and
/// worktrees that no longer belong to a project are dropped.
pub fn build_session(input: &SessionInput) -> Session {
    let live: Vec<TerminalInfo> =
        input.terminals.iter().filter(|t| t.exited.is_none()).cloned().collect();

    let mut keys = vec![WorkspaceKey::Home];
    for w in input.projects.iter().flat_map(|p| p.worktrees.iter()) {
        let key = WorkspaceKey::Worktree(w.path.clone());
        if !keys.contains(&key) {
            keys.push(key);
        }
    }

    let mut workspaces = Vec::new();
    for key in keys {
        let ids = terminals_in(&key, &live);
        if ids.is_empty() {
            continue;
        }
        let terminals = ids
            .iter()
            .filter_map(|id| live.iter().find(|t| t.id == *id))
            .map(SavedTerminal::from_info)
            .collect();
        let active = input.ws.active_in(&key, &ids).and_then(|a| ids.iter().position(|i| *i == a));
        let layout = input.layouts.get(&key)
            .and_then(|layout| layout.remap(|id| ids.iter().position(|&saved| saved == id)))
            .filter(|layout| matches!(layout, Layout::Split { .. }));
        let active = match &layout {
            Some(layout) if !active.is_some_and(|index| layout.contains(index)) => Some(layout.first_leaf()),
            _ => active,
        };
        workspaces.push(SavedWorkspace { worktree: key.path().map(Path::to_path_buf), terminals, active, layout });
    }

    Session {
        selected: input.ws.selected.path().map(Path::to_path_buf),
        daemon_pid: None,
        workspaces,
        home_collapsed: input.home_collapsed,
        collapsed_projects: input
            .projects
            .iter()
            .filter(|p| input.collapsed.contains(&p.id))
            .map(|p| p.path.clone())
            .collect(),
        collapsed_groups: input
            .groups
            .iter()
            .filter(|g| input.collapsed_groups.contains(&g.id))
            .map(|g| g.name.clone())
            .collect(),
        window: input.window,
        sidebar_w: input.sidebar_w,
        right_panel: input.right_panel,
    }
}

/// Maps the saved collapse state (project paths, group names) back to current ids.
pub fn resolve_collapsed(
    session: &Session,
    projects: &[ProjectInfo],
    groups: &[GroupInfo],
) -> (HashSet<ProjectId>, HashSet<GroupId>) {
    let projs = projects
        .iter()
        .filter(|p| session.collapsed_projects.iter().any(|c| same_path(c, &p.path)))
        .map(|p| p.id)
        .collect();
    let grps = groups
        .iter()
        .filter(|g| session.collapsed_groups.iter().any(|n| n == &g.name))
        .map(|g| g.id)
        .collect();
    (projs, grps)
}

/// One workspace to refill: its key, each terminal to recreate (in order) and the
/// index of the tab to activate.
pub type RestoreItem = (WorkspaceKey, Vec<SavedTerminal>, Option<usize>);

/// What to recreate after a full quit. Nothing when the daemon still has terminals.
pub fn restore_plan(session: &Session, daemon_terminal_count: usize, projects: &[ProjectInfo]) -> Vec<RestoreItem> {
    restore_plan_with(session, daemon_terminal_count, projects, |p| p.is_dir(), dirs::home_dir())
}

/// [`restore_plan`] with injectable directory check and home directory (for tests).
pub fn restore_plan_with(
    session: &Session,
    daemon_terminal_count: usize,
    projects: &[ProjectInfo],
    dir_exists: impl Fn(&Path) -> bool,
    home: Option<PathBuf>,
) -> Vec<RestoreItem> {
    if daemon_terminal_count > 0 {
        return Vec::new();
    }
    let mut plan: Vec<RestoreItem> = Vec::new();
    for saved in &session.workspaces {
        if saved.terminals.is_empty() {
            continue;
        }
        let key = WorkspaceKey::of(saved.worktree.as_deref());
        if !workspace_exists(&key, projects) {
            continue;
        }
        // Prefer the daemon's spelling of the worktree path.
        let key = match &key {
            WorkspaceKey::Worktree(p) => projects
                .iter()
                .flat_map(|pr| pr.worktrees.iter())
                .find(|w| same_path(&w.path, p))
                .map(|w| WorkspaceKey::Worktree(w.path.clone()))
                .unwrap_or(key),
            WorkspaceKey::Home => key,
        };
        if plan.iter().any(|(k, _, _)| *k == key) {
            continue;
        }
        let fallback = key.path().map(Path::to_path_buf).or_else(|| home.clone()).unwrap_or_default();
        let mut terminals = saved.terminals.clone();
        for terminal in &mut terminals {
            if terminal.cwd.as_os_str().is_empty() || !dir_exists(&terminal.cwd) {
                terminal.cwd = fallback.clone();
            }
        }
        let active = saved.active.filter(|i| *i < terminals.len());
        plan.push((key, terminals, active));
    }
    plan
}

/// Smallest window we restore to.
pub const MIN_WINDOW: (f32, f32) = (640., 400.);
/// Size used when nothing usable was saved.
pub const DEFAULT_WINDOW: (f32, f32) = (1200., 760.);

/// A rectangle in logical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Where to open the window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    pub width: f32,
    pub height: f32,
    /// `None` = let the caller center it (no saved position, or it is off-screen).
    pub origin: Option<(f32, f32)>,
    pub maximized: bool,
    /// Index into the displays slice of the monitor the window must be opened on.
    pub display: Option<usize>,
}

/// Placement for the saved window: size clamped to a sane minimum, position kept only when the
/// title-bar area is reasonably visible on one of `displays`.
#[cfg(test)]
pub fn window_placement(saved: Option<&SavedWindow>, displays: &[Rect]) -> Placement {
    window_placement_on(saved, displays, &[])
}

/// Like `window_placement`, also picking the monitor to open on. `keys[i]` is the `display_key`
/// of `displays[i]`. The saved monitor wins when the window is still visible on it; otherwise the
/// first monitor showing the title-bar area is used.
pub fn window_placement_on(saved: Option<&SavedWindow>, displays: &[Rect], keys: &[Option<u64>]) -> Placement {
    let Some(s) = saved.filter(|s| [s.x, s.y, s.width, s.height].iter().all(|v| v.is_finite())) else {
        return Placement { width: DEFAULT_WINDOW.0, height: DEFAULT_WINDOW.1, origin: None, maximized: false, display: None };
    };
    let width = s.width.max(MIN_WINDOW.0);
    let height = s.height.max(MIN_WINDOW.1);
    // The strip that must stay reachable: top-left part of the title bar.
    let grab = Rect { x: s.x, y: s.y, w: 120_f32.min(width), h: 40_f32.min(height) };
    let visible_on = |d: &Rect| {
        let ix = (grab.x + grab.w).min(d.x + d.w) - grab.x.max(d.x);
        let iy = (grab.y + grab.h).min(d.y + d.h) - grab.y.max(d.y);
        ix >= grab.w.min(80.) && iy >= grab.h.min(30.)
    };
    let saved_idx = s
        .display
        .and_then(|k| keys.iter().position(|x| *x == Some(k)))
        .filter(|&i| displays.get(i).is_some_and(visible_on));
    let display = saved_idx.or_else(|| displays.iter().position(visible_on));
    Placement { width, height, origin: display.map(|_| (s.x, s.y)), maximized: s.maximized, display }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sich_proto::WorktreeInfo;

    fn term(id: u64, wt: Option<&str>, cwd: &str, created: u64) -> TerminalInfo {
        TerminalInfo {
            id,
            title: String::new(),
            cwd: PathBuf::from(cwd),
            cols: 80,
            rows: 24,
            exited: None,
            created_unix_ms: created,
            worktree: wt.map(PathBuf::from),
            agent: None,
        }
    }

    fn wt(path: &str, main: bool) -> WorktreeInfo {
        WorktreeInfo { path: PathBuf::from(path), branch: None, head: String::new(), is_main: main, dirty: false }
    }

    fn project(id: u64, path: &str, group: Option<u64>, worktrees: Vec<WorktreeInfo>) -> ProjectInfo {
        ProjectInfo { id, name: "p".into(), path: PathBuf::from(path), is_git: true, worktrees, error: None, group }
    }

    fn saved_ws(wt: Option<&str>, cwds: &[&str], active: Option<usize>) -> SavedWorkspace {
        SavedWorkspace {
            worktree: wt.map(PathBuf::from),
            terminals: cwds.iter().map(|c| SavedTerminal { cwd: PathBuf::from(c), ..Default::default() }).collect(),
            active,
            layout: None,
        }
    }

    fn unique_path(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        std::env::temp_dir().join(format!("sich-session-{tag}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)))
    }

    #[test]
    fn old_and_unknown_agent_records_leave_other_terminal_data_intact() {
        let old = Session::parse(r#"{"workspaces":[{"terminals":[{"cwd":"abc"}]}]}"#).unwrap();
        assert_eq!(old.workspaces[0].terminals, saved_ws(None, &["abc"], None).terminals);
        let future = Session::parse(r#"{"workspaces":[{"terminals":[{"cwd":"abc","agent":{"kind":"Future","session_id":"abcd1234"},"scrollback":"1-2.bin"}]}]}"#).unwrap();
        let terminal = &future.workspaces[0].terminals[0];
        assert_eq!(terminal.cwd, Path::new("abc"));
        assert_eq!(terminal.scrollback.as_deref(), Some("1-2.bin"));
        assert_eq!(terminal.agent, None);
    }

    #[test]
    fn resume_commands_accept_only_safe_provider_ids() {
        assert_eq!(resume_command(AgentKind::Claude, "Ab12-456").as_deref(), Some("claude --resume Ab12-456"));
        assert_eq!(resume_command(AgentKind::Codex, "Ab12-456").as_deref(), Some("codex resume Ab12-456"));
        for kind in [AgentKind::Other, AgentKind::Gemini] {
            assert_eq!(resume_command(kind, "Ab12-456"), None);
        }
        for id in ["short", "abc;abcd", "abc/abcd", "abcd 1234", "abcd1234\r", "abcd1234\n", "─abcd1234", "--dangerous"] {
            assert_eq!(resume_command(AgentKind::Claude, id), None, "{id:?}");
        }
        assert!(resume_command(AgentKind::Claude, &"a".repeat(64)).is_some());
        assert!(resume_command(AgentKind::Claude, &"a".repeat(65)).is_none());
    }

    #[test]
    fn scrollback_is_matched_by_live_terminal_id_in_restore_order() {
        let mut session = Session {
            daemon_pid: Some(5),
            workspaces: vec![saved_ws(None, &["/h", "/h"], None), saved_ws(Some("/w"), &["/w"], None)],
            ..Default::default()
        };
        session.workspaces[0].terminals[0].id = Some(4);
        session.workspaces[0].terminals[1].id = Some(2);
        session.workspaces[1].terminals[0].id = Some(8);
        let files = [(8, "8-1.bin".into()), (2, "2-1.bin".into())];
        assert!(!session.attach_scrollback(Some(6), &files));
        assert_eq!(session.workspaces[0].terminals[1].scrollback, None);
        assert!(session.attach_scrollback(Some(5), &files));
        assert_eq!(session.workspaces[0].terminals[0].scrollback, None);
        assert_eq!(session.workspaces[0].terminals[1].scrollback.as_deref(), Some("2-1.bin"));
        assert_eq!(session.workspaces[1].terminals[0].scrollback.as_deref(), Some("8-1.bin"));
    }

    // ---- serde / disk ----

    #[test]
    fn empty_object_is_default() {
        assert_eq!(Session::parse("{}"), Some(Session::default()));
    }

    #[test]
    fn pr_tab_round_trips_defaults_to_git_and_falls_back_while_invisible() {
        assert_eq!(RightTab::default(), RightTab::Git);
        let prefs = RightPanelState { tab: RightTab::Pr, ..RightPanelState::default() };
        let json = serde_json::to_string(&Session { right_panel: prefs, ..Default::default() }).unwrap();
        assert!(json.contains(r#""tab":"pr""#), "{json}");
        assert_eq!(Session::parse(&json).unwrap().right_panel.tab, RightTab::Pr);
        let old = Session::parse(r#"{"right_panel":{"open":true}}"#).unwrap();
        assert_eq!(old.right_panel.tab, RightTab::Git);

        assert_eq!(RightTab::Pr.effective(true), RightTab::Pr);
        assert_eq!(RightTab::Pr.effective(false), RightTab::Git);
        assert_eq!(RightTab::Git.effective(true), RightTab::Git);
        assert_eq!(RightTab::Git.effective(false), RightTab::Git);
        assert_eq!(RightTab::ALL, &[RightTab::Git, RightTab::Pr, RightTab::Context, RightTab::Images]);
        assert_eq!(RightTab::Images.effective(false), RightTab::Images);
        assert_eq!(RightTab::Context.effective(false), RightTab::Context);
        assert_eq!(RightTab::Context.effective(true), RightTab::Context);
    }

    #[test]
    fn context_tab_round_trips_and_old_values_keep_working() {
        let prefs = RightPanelState { tab: RightTab::Context, ..RightPanelState::default() };
        let json = serde_json::to_string(&Session { right_panel: prefs, ..Default::default() }).unwrap();
        assert!(json.contains(r#""tab":"context""#), "{json}");
        assert_eq!(Session::parse(&json).unwrap().right_panel.tab, RightTab::Context);
        for (raw, tab) in [("git", RightTab::Git), ("pr", RightTab::Pr), ("context", RightTab::Context), ("images", RightTab::Images)] {
            let session = Session::parse(&format!(r#"{{"right_panel":{{"tab":"{raw}"}}}}"#)).unwrap();
            assert_eq!(session.right_panel.tab, tab);
        }
    }

    #[test]
    fn images_tab_round_trips() {
        let prefs = RightPanelState { tab: RightTab::Images, ..RightPanelState::default() };
        let json = serde_json::to_string(&Session { right_panel: prefs, ..Default::default() }).unwrap();
        assert!(json.contains(r#""tab":"images""#), "{json}");
        assert_eq!(Session::parse(&json).unwrap().right_panel.tab, RightTab::Images);
    }

    #[test]
    fn right_panel_preferences_are_lenient_and_round_trip() {
        let old = Session::parse(r#"{"selected":null}"#).unwrap();
        assert_eq!(old.right_panel, RightPanelState::default());
        for right_panel in [r#""bad""#, r#"null"#, r#"{"open":"yes","width":-1,"tab":"future"}"#] {
            let session = Session::parse(&format!(r#"{{"right_panel":{right_panel}}}"#)).unwrap();
            assert_eq!(session.right_panel, RightPanelState::default());
        }
        let prefs = RightPanelState { width: Some(520.), open: false, tab: RightTab::Git, history_collapsed: true, history_height: Some(280.) };
        let session = Session { right_panel: prefs, ..Default::default() };
        let json = serde_json::to_string(&session).unwrap();
        assert_eq!(Session::parse(&json).unwrap().right_panel, prefs);
        for value in [r#"{"history_height":-1}"#, r#"{"history_height":"tall"}"#] {
            let session = Session::parse(&format!(r#"{{"right_panel":{value}}}"#)).unwrap();
            assert_eq!(session.right_panel.history_height, None);
        }
    }

    #[test]
    fn sidebar_width_missing_or_garbage_is_default() {
        assert_eq!(Session::parse("{}").unwrap().sidebar_w, None);
        assert_eq!(Session::parse(r#"{"sidebar_w":null}"#).unwrap().sidebar_w, None);
        assert_eq!(Session::parse(r#"{"sidebar_w":320}"#).unwrap().sidebar_w, Some(320.));
        assert_eq!(Session::parse(r#"{"sidebar_w":-5}"#).unwrap().sidebar_w, None);
        assert_eq!(Session::parse(r#"{"sidebar_w":0}"#).unwrap().sidebar_w, None);
        assert_eq!(Session::parse(r#"{"sidebar_w":"wide"}"#).unwrap().sidebar_w, None);
        assert_eq!(Session::parse(r#"{"sidebar_w":1e300}"#).unwrap().sidebar_w, None);
        // Bare NaN is not valid JSON: the file is rejected as a whole, never a crash.
        assert_eq!(Session::parse(r#"{"sidebar_w":NaN}"#), None);
    }

    #[test]
    fn partial_and_unknown_fields_are_tolerated() {
        let s = Session::parse(r#"{"selected":"/w/a","future":1,"workspaces":[{"worktree":null}]}"#).unwrap();
        assert_eq!(s.selected, Some(PathBuf::from("/w/a")));
        assert_eq!(s.workspaces.len(), 1);
        assert!(s.workspaces[0].terminals.is_empty());
        assert!(!s.home_collapsed);
        assert!(!s.has_terminals());
    }

    #[test]
    fn garbage_is_none_not_a_crash() {
        assert_eq!(Session::parse("not json {{"), None);
        assert_eq!(Session::parse(""), None);
        assert_eq!(Session::parse(r#"{"workspaces":5}"#), None);
        assert_eq!(Session::load_from(&unique_path("missing")), None);
    }
    #[test]
    fn malformed_layout_does_not_discard_terminals() {
        for layout in [r#""bad""#, r#"{"split":{"axis":"future"}}"#, r#"{"leaf":"bad"}"#, "null"] {
            let text = format!(r#"{{"workspaces":[{{"terminals":[{{"cwd":"kept"}}],"layout":{layout}}}]}}"#);
            let session = Session::parse(&text).unwrap();
            assert_eq!(session.workspaces[0].terminals[0].cwd, Path::new("kept"));
            assert_eq!(session.workspaces[0].layout, None);
        }
    }

    #[test]
    fn persisted_layout_indexes_live_tabs_and_prunes_exited_and_duplicates() {
        use crate::terminal_layout::Axis;
        let split = |first, second| Layout::Split {
            axis: Axis::Vertical, ratio: 0.7, first: Box::new(first), second: Box::new(second),
        };
        let mut dead = term(3, None, "dead", 3);
        dead.exited = Some(Some(0));
        let hidden = term(4, None, "hidden", 0);
        let terminals = [term(1, None, "a", 1), term(2, None, "b", 2), dead, hidden];
        let layouts = HashMap::from([(WorkspaceKey::Home,
            split(Layout::Leaf(2), split(Layout::Leaf(3), split(Layout::Leaf(1), Layout::Leaf(2)))))]);
        let mut ws = Workspaces::default();
        ws.set_active(&WorkspaceKey::Home, 3);
        let (collapsed, groups) = (HashSet::new(), HashSet::new());
        let session = build_session(&SessionInput {
            terminals: &terminals, ws: &ws, layouts: &layouts, projects: &[], groups: &[],
            home_collapsed: false, collapsed: &collapsed, collapsed_groups: &groups,
            window: None, sidebar_w: None, right_panel: RightPanelState::default(),
        });
        let expected = Layout::Split { axis: Axis::Vertical, ratio: 0.7,
            first: Box::new(Layout::Leaf(2_usize)), second: Box::new(Layout::Leaf(1_usize)) };
        assert_eq!(session.workspaces[0].layout, Some(expected));
        assert_eq!(session.workspaces[0].active, Some(2));
        let decoded = Session::parse(&serde_json::to_string(&session).unwrap()).unwrap();
        assert_eq!(decoded, session);
        let solo = HashMap::from([(WorkspaceKey::Home, split(Layout::Leaf(3), Layout::Leaf(1)))]);
        let input = SessionInput {
            terminals: &terminals, ws: &ws, layouts: &solo, projects: &[], groups: &[],
            home_collapsed: false, collapsed: &collapsed, collapsed_groups: &groups,
            window: None, sidebar_w: None, right_panel: RightPanelState::default(),
        };
        assert_eq!(build_session(&input).workspaces[0].layout, None);
    }

    #[test]
    fn round_trips_through_disk_atomically() {
        let path = unique_path("rt").join("nested").join("session.json");
        let s = Session {
            selected: Some("/w/a".into()),
            daemon_pid: Some(45),
            workspaces: vec![saved_ws(None, &["/h"], Some(0)), saved_ws(Some("/w/a"), &["/w/a", "/w/a/src"], Some(1))],
            home_collapsed: true,
            collapsed_projects: vec!["/w".into()],
            collapsed_groups: vec!["game".into()],
            window: Some(SavedWindow { x: 10., y: 20., width: 900., height: 700., maximized: true, display: None }),
            sidebar_w: Some(345.5),
            right_panel: RightPanelState::default(),
        };
        s.save_to(&path).unwrap();
        assert!(!path.with_extension("json.tmp").exists());
        assert_eq!(Session::load_from(&path), Some(s.clone()));
        // Overwriting works too.
        Session::default().save_to(&path).unwrap();
        assert_eq!(Session::load_from(&path), Some(Session::default()));
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn garbage_file_on_disk_loads_as_none() {
        let path = unique_path("garbage");
        std::fs::write(&path, b"\xff\xfe garbage").unwrap();
        assert_eq!(Session::load_from(&path), None);
        let _ = std::fs::remove_file(&path);
    }

    // ---- build_session ----

    #[test]
    fn build_groups_terminals_per_workspace_in_creation_order() {
        let projects = vec![project(1, "/w/a", None, vec![wt("/w/a", true), wt("/w/a-x", false)])];
        let terminals = vec![
            term(5, Some("/w/a-x"), "/w/a-x", 50),
            term(1, None, "/home", 10),
            term(3, Some("/w/a"), "/w/a/src", 30),
            term(2, Some("/w/a"), "/w/a", 20),
        ];
        let mut ws = Workspaces::default();
        ws.selected = WorkspaceKey::Worktree("/w/a-x".into());
        ws.set_active(&WorkspaceKey::Worktree("/w/a".into()), 3);
        let collapsed: HashSet<ProjectId> = HashSet::new();
        let cg: HashSet<GroupId> = HashSet::new();
        let s = build_session(&SessionInput {
            terminals: &terminals,
            ws: &ws,
            layouts: &HashMap::new(),
            projects: &projects,
            groups: &[],
            home_collapsed: true,
            collapsed: &collapsed,
            collapsed_groups: &cg,
            window: None,
            sidebar_w: None,
            right_panel: RightPanelState::default(),
        });
        assert_eq!(s.selected, Some(PathBuf::from("/w/a-x")));
        assert!(s.home_collapsed);
        assert_eq!(s.workspaces.len(), 3);
        let mut expected = [
            saved_ws(None, &["/home"], Some(0)),
            saved_ws(Some("/w/a"), &["/w/a", "/w/a/src"], Some(1)),
            saved_ws(Some("/w/a-x"), &["/w/a-x"], Some(0)),
        ];
        for (term, id) in expected.iter_mut().flat_map(|w| &mut w.terminals).zip([1, 2, 3, 5]) {
            term.id = Some(id);
        }
        assert_eq!(s.workspaces, expected);
    }

    #[test]
    fn build_skips_exited_and_unknown_worktrees_and_maps_collapse() {
        let projects = vec![
            project(1, "/w/a", Some(9), vec![wt("/w/a", true)]),
            project(2, "/w/b", None, vec![wt("/w/b", true)]),
        ];
        let groups = vec![GroupInfo { id: 9, name: "game".into() }, GroupInfo { id: 10, name: "other".into() }];
        let mut dead = term(1, None, "/home", 10);
        dead.exited = Some(Some(0));
        let terminals = vec![dead, term(2, Some("/gone"), "/gone", 20), term(3, Some("/w/b"), "/w/b", 30)];
        let ws = Workspaces::default();
        let collapsed: HashSet<ProjectId> = [2].into_iter().collect();
        let cg: HashSet<GroupId> = [9].into_iter().collect();
        let s = build_session(&SessionInput {
            terminals: &terminals,
            ws: &ws,
            layouts: &HashMap::new(),
            projects: &projects,
            groups: &groups,
            home_collapsed: false,
            collapsed: &collapsed,
            collapsed_groups: &cg,
            window: Some(SavedWindow { x: 1., y: 2., width: 800., height: 600., maximized: false, display: None }),
            sidebar_w: Some(250.),
            right_panel: RightPanelState::default(),
        });
        assert_eq!(s.sidebar_w, Some(250.));
        assert_eq!(s.selected, None);
        let mut expected = saved_ws(Some("/w/b"), &["/w/b"], Some(0));
        expected.terminals[0].id = Some(3);
        assert_eq!(s.workspaces, vec![expected]);
        assert_eq!(s.collapsed_projects, vec![PathBuf::from("/w/b")]);
        assert_eq!(s.collapsed_groups, vec!["game".to_string()]);
        assert_eq!(s.window.unwrap().width, 800.);
    }

    #[test]
    fn build_with_no_terminals_is_empty() {
        let ws = Workspaces::default();
        let (c, g) = (HashSet::new(), HashSet::new());
        let s = build_session(&SessionInput {
            terminals: &[],
            ws: &ws,
            layouts: &HashMap::new(),
            projects: &[],
            groups: &[],
            home_collapsed: false,
            collapsed: &c,
            collapsed_groups: &g,
            window: None,
            sidebar_w: None,
            right_panel: RightPanelState::default(),
        });
        assert_eq!(s, Session::default());
    }

    // ---- resolve_collapsed ----

    #[test]
    fn collapse_maps_back_by_path_and_name() {
        let session = Session {
            collapsed_projects: vec!["/w/b/".into(), "/gone".into()],
            collapsed_groups: vec!["game".into(), "missing".into()],
            ..Default::default()
        };
        let projects = vec![project(7, "/w/a", None, vec![]), project(8, "/w/b", None, vec![])];
        let groups = vec![GroupInfo { id: 3, name: "game".into() }];
        let (p, g) = resolve_collapsed(&session, &projects, &groups);
        assert_eq!(p, [8].into_iter().collect());
        assert_eq!(g, [3].into_iter().collect());
        let (p, g) = resolve_collapsed(&Session::default(), &projects, &groups);
        assert!(p.is_empty() && g.is_empty());
    }

    // ---- restore_plan ----

    fn exists_only(paths: &'static [&'static str]) -> impl Fn(&Path) -> bool {
        move |p| paths.iter().any(|x| Path::new(x) == p)
    }

    #[test]
    fn plan_is_empty_when_the_daemon_has_terminals() {
        let session = Session { workspaces: vec![saved_ws(None, &["/h"], Some(0))], ..Default::default() };
        assert!(restore_plan_with(&session, 1, &[], exists_only(&["/h"]), None).is_empty());
        assert_eq!(restore_plan_with(&session, 0, &[], exists_only(&["/h"]), None).len(), 1);
    }

    #[test]
    fn plan_keeps_home_and_existing_worktrees_in_order() {
        let projects = vec![project(1, "/w/a", None, vec![wt("/w/a", true), wt("/w/a-x", false)])];
        let session = Session {
            workspaces: vec![
                saved_ws(Some("/w/a-x"), &["/w/a-x", "/w/a-x/src"], Some(1)),
                saved_ws(None, &["/h"], None),
            ],
            ..Default::default()
        };
        let plan = restore_plan_with(&session, 0, &projects, exists_only(&["/w/a-x", "/w/a-x/src", "/h"]), None);
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0], (WorkspaceKey::Worktree("/w/a-x".into()), saved_ws(None, &["/w/a-x", "/w/a-x/src"], None).terminals, Some(1)));
        assert_eq!(plan[1], (WorkspaceKey::Home, saved_ws(None, &["/h"], None).terminals, None));
    }

    #[test]
    fn plan_drops_missing_worktrees_and_empty_workspaces() {
        let projects = vec![project(1, "/w/a", None, vec![wt("/w/a", true)])];
        let session = Session {
            workspaces: vec![
                saved_ws(Some("/w/removed"), &["/w/removed"], Some(0)),
                saved_ws(Some("/w/a"), &[], None),
                saved_ws(None, &["/h"], Some(0)),
            ],
            ..Default::default()
        };
        let plan = restore_plan_with(&session, 0, &projects, exists_only(&["/h"]), None);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].0, WorkspaceKey::Home);
    }

    #[test]
    fn plan_falls_back_to_worktree_or_home_for_missing_cwd() {
        let projects = vec![project(1, "/w/a", None, vec![wt("/w/a", true)])];
        let session = Session {
            workspaces: vec![
                saved_ws(Some("/w/a"), &["/w/a/deleted", "/w/a/src", ""], Some(2)),
                saved_ws(None, &["/nope"], Some(5)),
            ],
            ..Default::default()
        };
        let plan = restore_plan_with(&session, 0, &projects, exists_only(&["/w/a/src"]), Some("/home/u".into()));
        assert_eq!(plan[0].1, saved_ws(None, &["/w/a", "/w/a/src", "/w/a"], None).terminals);
        assert_eq!(plan[0].2, Some(2));
        assert_eq!(plan[1].1, saved_ws(None, &["/home/u"], None).terminals);
        // Out-of-range active index is dropped.
        assert_eq!(plan[1].2, None);
    }

    #[test]
    fn plan_uses_the_daemons_spelling_and_ignores_duplicates() {
        let projects = vec![project(1, "/w/a", None, vec![wt("/w/a", true)])];
        let session = Session {
            workspaces: vec![saved_ws(Some("/w/a/"), &["/w/a"], None), saved_ws(Some("/w/a"), &["/w/a"], None)],
            ..Default::default()
        };
        let plan = restore_plan_with(&session, 0, &projects, |_| true, None);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].0.path(), Some(Path::new("/w/a")));
    }

    // ---- window ----

    fn screen() -> Vec<Rect> {
        vec![Rect { x: 0., y: 0., w: 1920., h: 1080. }]
    }

    #[test]
    fn no_saved_window_uses_default_size_and_centering() {
        let p = window_placement(None, &screen());
        assert_eq!((p.width, p.height, p.origin, p.maximized), (1200., 760., None, false));
    }

    #[test]
    fn saved_window_on_screen_is_kept() {
        let s = SavedWindow { x: 100., y: 50., width: 1000., height: 700., maximized: true, display: None };
        let p = window_placement(Some(&s), &screen());
        assert_eq!((p.width, p.height, p.origin, p.maximized), (1000., 700., Some((100., 50.)), true));
    }

    #[test]
    fn off_screen_position_is_dropped_but_size_kept() {
        let s = SavedWindow { x: 5000., y: 50., width: 1000., height: 700., maximized: false, display: None };
        let p = window_placement(Some(&s), &screen());
        assert_eq!((p.width, p.height, p.origin), (1000., 700., None));
        // A second monitor that is gone.
        let s = SavedWindow { x: -1800., y: 100., width: 800., height: 600., maximized: false, display: None };
        assert_eq!(window_placement(Some(&s), &screen()).origin, None);
        // ...but present when the monitor is there.
        let two = vec![Rect { x: -1920., y: 0., w: 1920., h: 1080. }, screen()[0]];
        assert_eq!(window_placement(Some(&s), &two).origin, Some((-1800., 100.)));
        // Barely touching an edge does not count.
        let s = SavedWindow { x: 1915., y: 50., width: 800., height: 600., maximized: false, display: None };
        assert_eq!(window_placement(Some(&s), &screen()).origin, None);
    }

    #[test]
    fn tiny_size_is_clamped_and_nan_ignored() {
        let s = SavedWindow { x: 0., y: 0., width: 10., height: 10., maximized: false, display: None };
        let p = window_placement(Some(&s), &screen());
        assert_eq!((p.width, p.height), MIN_WINDOW);
        let s = SavedWindow { x: f32::NAN, y: 0., width: 800., height: 600., maximized: true, display: None };
        let p = window_placement(Some(&s), &screen());
        assert_eq!((p.width, p.height, p.origin, p.maximized), (1200., 760., None, false));
    }

    #[test]
    fn placement_prefers_saved_display_and_falls_back() {
        let two = [
            Rect { x: 0., y: 0., w: 1920., h: 1080. },
            Rect { x: -1440., y: -300., w: 1440., h: 1700. },
        ];
        let keys = [Some(1), Some(2)];
        let mut s = SavedWindow { x: -1439., y: -273., width: 1438., height: 1649., maximized: false, display: Some(2) };
        let p = window_placement_on(Some(&s), &two, &keys);
        assert_eq!((p.origin, p.display), (Some((-1439., -273.)), Some(1)));
        // unknown key: monitor found by position
        s.display = Some(99);
        assert_eq!(window_placement_on(Some(&s), &two, &keys).display, Some(1));
        s.display = None;
        assert_eq!(window_placement_on(Some(&s), &two, &keys).display, Some(1));
        // saved monitor no longer shows the window: use the one that does
        s.x = 100.;
        s.y = 100.;
        s.display = Some(2);
        assert_eq!(window_placement_on(Some(&s), &two, &keys).display, Some(0));
        // off-screen: no origin, no display
        s.x = 9000.;
        let p = window_placement_on(Some(&s), &two, &keys);
        assert_eq!((p.origin, p.display), (None, None));
    }

    #[test]
    fn display_key_is_stable_and_mixes_halves() {
        assert_eq!(display_key(5), display_key(5));
        assert_ne!(display_key(2), display_key((1 << 64) | 2));
        assert_eq!(display_key(0), 0);
    }

    #[test]
    fn saved_window_without_display_field_still_loads() {
        let w: SavedWindow = serde_json::from_str(r#"{"x":1.0,"y":2.0,"width":800.0,"height":600.0,"maximized":false}"#).unwrap();
        assert_eq!(w.display, None);
    }

    #[test]
    fn from_bounds_rejects_nonsense() {
        assert!(SavedWindow::from_bounds(0., 0., 800., 600., false).is_some());
        assert!(SavedWindow::from_bounds(-32000., -32000., 800., 600., false).is_none());
        assert!(SavedWindow::from_bounds(0., 0., 0., 0., false).is_none());
        assert!(SavedWindow::from_bounds(f32::NAN, 0., 800., 600., false).is_none());
    }
}

#[cfg(test)]
mod generation_tests {
    use super::*;

    fn owner(socket: &str, id: TermId, pid: u32, created_ms: u64) -> TerminalOwner {
        TerminalOwner { socket: socket.into(), id, pid, created_ms }
    }

    fn saved(id: TermId, socket: Option<&str>, pid: Option<u32>, created: Option<u64>, cwd: &str) -> SavedTerminal {
        SavedTerminal {
            id: Some(id),
            socket: socket.map(Into::into),
            daemon_pid: pid,
            created_ms: created,
            cwd: PathBuf::from(cwd),
            ..Default::default()
        }
    }

    fn session(terminals: Vec<SavedTerminal>, active: Option<usize>) -> Session {
        Session {
            workspaces: vec![SavedWorkspace { worktree: None, terminals, active, layout: None }],
            ..Session::default()
        }
    }

    #[test]
    fn per_terminal_owner_survives_a_round_trip() {
        let mut s = session(vec![saved(7, None, None, None, "a"), saved(8, None, None, None, "b")], Some(1));
        s.record_owners(|key| match key {
            7 => Some(owner("base-new", 1, 40, 1000)),
            8 => Some(owner("base", 1, 30, 900)),
            _ => None,
        });
        let back = Session::parse(&serde_json::to_string(&s).unwrap()).unwrap();
        let terms = &back.workspaces[0].terminals;
        assert_eq!((terms[0].id, terms[0].socket.as_deref(), terms[0].daemon_pid, terms[0].created_ms), (Some(1), Some("base-new"), Some(40), Some(1000)));
        assert_eq!((terms[1].id, terms[1].socket.as_deref(), terms[1].daemon_pid, terms[1].created_ms), (Some(1), Some("base"), Some(30), Some(900)));
    }

    #[test]
    fn terminals_without_a_live_owner_lose_their_wire_identity() {
        let mut s = session(vec![saved(7, None, None, None, "a")], None);
        s.record_owners(|_| None);
        assert_eq!(s.workspaces[0].terminals[0].id, None);
    }

    #[test]
    fn old_session_files_without_owners_still_parse() {
        let s = Session::parse(r#"{"daemon_pid":5,"workspaces":[{"terminals":[{"id":3,"cwd":"x"}]}]}"#).unwrap();
        let t = &s.workspaces[0].terminals[0];
        assert_eq!((t.id, t.socket.as_deref(), t.daemon_pid, t.created_ms), (Some(3), None, None, None));
    }

    #[test]
    fn scrollback_is_attached_only_by_the_owning_daemon() {
        let mut s = session(
            vec![saved(1, Some("base-new"), Some(40), Some(1), "a"), saved(1, Some("base"), Some(30), Some(1), "b")],
            None,
        );
        let files = [(1, "1.bin".to_string())];
        assert!(!s.attach_owner_scrollback(Some("base-new"), Some(41), &files), "same socket, other process");
        assert!(!s.attach_owner_scrollback(None, Some(40), &files));
        assert!(s.attach_owner_scrollback(Some("base"), Some(30), &files));
        let terms = &s.workspaces[0].terminals;
        assert_eq!(terms[0].scrollback, None);
        assert_eq!(terms[1].scrollback.as_deref(), Some("1.bin"));
    }

    #[test]
    fn reattach_matches_live_terminals_by_owner_and_recreates_the_rest() {
        let s = session(
            vec![
                saved(1, Some("base-old"), Some(30), Some(100), "live-old"),
                saved(1, Some("base-new"), Some(40), Some(200), "live-new"),
                saved(2, Some("base-new"), Some(40), Some(300), "closed-meanwhile"),
                saved(3, Some("base-gone"), Some(50), Some(400), "owner-died"),
            ],
            Some(1),
        );
        let attached = [(11, owner("base-old", 1, 30, 100)), (12, owner("base-new", 1, 40, 200))];
        let peers = [("base-old".to_string(), 30), ("base-new".to_string(), 40)];
        let (rest, selected, _) = reattach(s, &attached, &peers, 2);
        let cwds: Vec<_> = rest.workspaces[0].terminals.iter().map(|t| t.cwd.to_str().unwrap()).collect();
        assert_eq!(cwds, ["owner-died"], "a live owner without the terminal means it was closed");
        assert_eq!(selected, vec![(WorkspaceKey::of(None), 12)]);
        assert_eq!(rest.workspaces[0].active, None);
    }

    #[test]
    fn reattach_ignores_a_reused_wire_id_of_another_incarnation() {
        let s = session(vec![saved(1, Some("base"), Some(30), Some(100), "stale")], Some(0));
        // Same socket and id, but created later: a different terminal.
        let attached = [(11, owner("base", 1, 30, 999))];
        let peers = [("base".to_string(), 30)];
        let (rest, selected, _) = reattach(s, &attached, &peers, 1);
        assert!(selected.is_empty());
        assert!(rest.workspaces[0].terminals.is_empty());
    }

    #[test]
    fn reattach_handles_sessions_saved_before_owners_were_recorded() {
        let mut s = session(vec![saved(4, None, None, None, "a"), saved(5, None, None, None, "b")], Some(0));
        s.daemon_pid = Some(30);
        let attached = [(11, owner("base", 4, 30, 1))];
        let (rest, selected, _) = reattach(s.clone(), &attached, &[("base".into(), 30)], 1);
        assert_eq!(selected, vec![(WorkspaceKey::of(None), 11)]);
        assert!(rest.workspaces[0].terminals.is_empty(), "a populated daemon is never refilled");

        // The daemon had no terminals at all: everything is recreated and the active tab is kept.
        let (rest, selected, _) = reattach(s, &[], &[("base-new".into(), 40)], 0);
        assert!(selected.is_empty());
        assert_eq!(rest.workspaces[0].terminals.len(), 2);
        assert_eq!(rest.workspaces[0].active, Some(0));
    }
    #[test]
    fn mixed_layout_preserves_live_and_missing_identity_and_active_slot() {
        use crate::terminal_layout::Axis;
        let split = |axis, ratio, first, second| Layout::Split {
            axis, ratio, first: Box::new(first), second: Box::new(second),
        };
        let mut s = session(vec![
            saved(1, Some("live"), Some(30), Some(100), "live"),
            saved(2, Some("live"), Some(30), Some(200), "closed"),
            saved(3, Some("gone"), Some(40), Some(300), "missing"),
        ], Some(2));
        s.workspaces[0].layout = Some(split(Axis::Horizontal, 0.3,
            Layout::Leaf(0), split(Axis::Vertical, 0.8, Layout::Leaf(1),
                split(Axis::Vertical, 0.6, Layout::Leaf(2),
                    split(Axis::Horizontal, 0.4, Layout::Leaf(2), Layout::Leaf(99))))));
        let (rest, selected, layouts) = reattach(s, &[(0, owner("live", 1, 30, 100))], &[("live".into(), 30)], 1);
        assert!(selected.is_empty());
        assert_eq!(rest.workspaces[0].active, Some(0));
        assert_eq!(rest.workspaces[0].terminals[0].cwd, Path::new("missing"));
        assert_eq!(rest.workspaces[0].layout, None);
        let expected = Layout::Split { axis: Axis::Horizontal, ratio: 0.3,
            first: Box::new(Layout::Leaf(RestoreLeaf::Live(0))),
            second: Box::new(Layout::Leaf(RestoreLeaf::Missing(0))) };
        assert_eq!(layouts.get(&WorkspaceKey::Home), Some(&expected));
        let plan = restore_plan_with(&rest, 0, &[], |_| true, None);
        assert_eq!(plan[0].2, Some(0));
    }

}
