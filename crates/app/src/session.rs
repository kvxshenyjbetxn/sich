//! Persistent UI session (`<config>/sich/session.json`): what the window looked like when Sich
//! was last quit, so a full quit (daemon stopped, terminals gone) can be restored.
//!
//! Everything except the disk access is pure and unit-tested: building a `Session` from the
//! live state, planning the terminal restore, resolving collapse state and window placement.

use std::collections::HashSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sich_proto::{AgentKind, AgentState, GroupId, GroupInfo, ProjectId, ProjectInfo, TermId, TerminalInfo};

use crate::workspace::{WorkspaceKey, Workspaces, same_path, terminals_in, workspace_exists};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    /// Selected worktree; `None` = Home.
    pub selected: Option<PathBuf>,
    pub daemon_pid: Option<u32>,
    pub workspaces: Vec<SavedWorkspace>,
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
}

impl RightTab {
    pub const ALL: &'static [Self] = &[Self::Git];
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(from = "serde_json::Value")]
pub struct RightPanelState {
    pub width: Option<f32>,
    pub open: bool,
    pub tab: RightTab,
    pub history_collapsed: bool,
}

impl Default for RightPanelState {
    fn default() -> Self {
        Self { width: None, open: true, tab: RightTab::default(), history_collapsed: false }
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
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedTerminal {
    pub id: Option<TermId>,
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
}

impl Session {
    /// True when there is at least one terminal to bring back.
    pub fn has_terminals(&self) -> bool {
        self.workspaces.iter().any(|w| !w.terminals.is_empty())
    }

    /// Associates filenames with the saved IDs only when this is their daemon.
    pub fn attach_scrollback(&mut self, pid: Option<u32>, files: &[(TermId, String)]) -> bool {
        if pid.is_none() || self.daemon_pid != pid {
            return false;
        }
        for term in self.workspaces.iter_mut().flat_map(|w| &mut w.terminals) {
            term.scrollback = files.iter().find(|(id, _)| Some(*id) == term.id).map(|(_, name)| name.clone());
        }
        true
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
        ok.then_some(SavedWindow { x, y, width, height, maximized })
    }
}

/// Live state a `Session` is built from.
pub struct SessionInput<'a> {
    pub terminals: &'a [TerminalInfo],
    pub ws: &'a Workspaces,
    pub projects: &'a [ProjectInfo],
    pub groups: &'a [GroupInfo],
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
        workspaces.push(SavedWorkspace { worktree: key.path().map(Path::to_path_buf), terminals, active });
    }

    Session {
        selected: input.ws.selected.path().map(Path::to_path_buf),
        daemon_pid: None,
        workspaces,
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
}

/// Placement for the saved window: size clamped to a sane minimum, position kept only when the
/// title-bar area is reasonably visible on one of `displays`.
pub fn window_placement(saved: Option<&SavedWindow>, displays: &[Rect]) -> Placement {
    let Some(s) = saved.filter(|s| [s.x, s.y, s.width, s.height].iter().all(|v| v.is_finite())) else {
        return Placement { width: DEFAULT_WINDOW.0, height: DEFAULT_WINDOW.1, origin: None, maximized: false };
    };
    let width = s.width.max(MIN_WINDOW.0);
    let height = s.height.max(MIN_WINDOW.1);
    // The strip that must stay reachable: top-left part of the title bar.
    let grab = Rect { x: s.x, y: s.y, w: 120_f32.min(width), h: 40_f32.min(height) };
    let on_screen = displays.iter().any(|d| {
        let ix = (grab.x + grab.w).min(d.x + d.w) - grab.x.max(d.x);
        let iy = (grab.y + grab.h).min(d.y + d.h) - grab.y.max(d.y);
        ix >= grab.w.min(80.) && iy >= grab.h.min(30.)
    });
    Placement { width, height, origin: on_screen.then_some((s.x, s.y)), maximized: s.maximized }
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
    fn right_panel_preferences_are_lenient_and_round_trip() {
        let old = Session::parse(r#"{"selected":null}"#).unwrap();
        assert_eq!(old.right_panel, RightPanelState::default());
        for right_panel in [r#""bad""#, r#"null"#, r#"{"open":"yes","width":-1,"tab":"future"}"#] {
            let session = Session::parse(&format!(r#"{{"right_panel":{right_panel}}}"#)).unwrap();
            assert_eq!(session.right_panel, RightPanelState::default());
        }
        let prefs = RightPanelState { width: Some(520.), open: false, tab: RightTab::Git, history_collapsed: true };
        let session = Session { right_panel: prefs, ..Default::default() };
        let json = serde_json::to_string(&session).unwrap();
        assert_eq!(Session::parse(&json).unwrap().right_panel, prefs);
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
    fn round_trips_through_disk_atomically() {
        let path = unique_path("rt").join("nested").join("session.json");
        let s = Session {
            selected: Some("/w/a".into()),
            daemon_pid: Some(45),
            workspaces: vec![saved_ws(None, &["/h"], Some(0)), saved_ws(Some("/w/a"), &["/w/a", "/w/a/src"], Some(1))],
            collapsed_projects: vec!["/w".into()],
            collapsed_groups: vec!["game".into()],
            window: Some(SavedWindow { x: 10., y: 20., width: 900., height: 700., maximized: true }),
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
            projects: &projects,
            groups: &[],
            collapsed: &collapsed,
            collapsed_groups: &cg,
            window: None,
            sidebar_w: None,
            right_panel: RightPanelState::default(),
        });
        assert_eq!(s.selected, Some(PathBuf::from("/w/a-x")));
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
            projects: &projects,
            groups: &groups,
            collapsed: &collapsed,
            collapsed_groups: &cg,
            window: Some(SavedWindow { x: 1., y: 2., width: 800., height: 600., maximized: false }),
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
            projects: &[],
            groups: &[],
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
        let s = SavedWindow { x: 100., y: 50., width: 1000., height: 700., maximized: true };
        let p = window_placement(Some(&s), &screen());
        assert_eq!((p.width, p.height, p.origin, p.maximized), (1000., 700., Some((100., 50.)), true));
    }

    #[test]
    fn off_screen_position_is_dropped_but_size_kept() {
        let s = SavedWindow { x: 5000., y: 50., width: 1000., height: 700., maximized: false };
        let p = window_placement(Some(&s), &screen());
        assert_eq!((p.width, p.height, p.origin), (1000., 700., None));
        // A second monitor that is gone.
        let s = SavedWindow { x: -1800., y: 100., width: 800., height: 600., maximized: false };
        assert_eq!(window_placement(Some(&s), &screen()).origin, None);
        // ...but present when the monitor is there.
        let two = vec![Rect { x: -1920., y: 0., w: 1920., h: 1080. }, screen()[0]];
        assert_eq!(window_placement(Some(&s), &two).origin, Some((-1800., 100.)));
        // Barely touching an edge does not count.
        let s = SavedWindow { x: 1915., y: 50., width: 800., height: 600., maximized: false };
        assert_eq!(window_placement(Some(&s), &screen()).origin, None);
    }

    #[test]
    fn tiny_size_is_clamped_and_nan_ignored() {
        let s = SavedWindow { x: 0., y: 0., width: 10., height: 10., maximized: false };
        let p = window_placement(Some(&s), &screen());
        assert_eq!((p.width, p.height), MIN_WINDOW);
        let s = SavedWindow { x: f32::NAN, y: 0., width: 800., height: 600., maximized: true };
        let p = window_placement(Some(&s), &screen());
        assert_eq!((p.width, p.height, p.origin, p.maximized), (1200., 760., None, false));
    }

    #[test]
    fn from_bounds_rejects_nonsense() {
        assert!(SavedWindow::from_bounds(0., 0., 800., 600., false).is_some());
        assert!(SavedWindow::from_bounds(-32000., -32000., 800., 600., false).is_none());
        assert!(SavedWindow::from_bounds(0., 0., 0., 0., false).is_none());
        assert!(SavedWindow::from_bounds(f32::NAN, 0., 800., 600., false).is_none());
    }
}
