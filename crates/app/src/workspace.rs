//! Workspace model: which terminals belong to Home or a worktree, selection and per-workspace
//! active terminal. Pure logic only (no GPUI), so it is unit-tested.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use sich_proto::{ProjectId, ProjectInfo, TermId, TerminalInfo};

/// Normalizes a path for comparison: case-insensitive with `\` separators on Windows,
/// without trailing separators and without the `\\?\` prefix.
pub fn normalize_path(path: &Path) -> String {
    normalize_str(&path.to_string_lossy())
}

fn normalize_str(raw: &str) -> String {
    let mut s = raw.to_string();
    if cfg!(windows) {
        s = s.replace('/', "\\");
        if let Some(rest) = s.strip_prefix("\\\\?\\") {
            s = rest.to_string();
        }
        s = s.to_lowercase();
    }
    let sep: &[char] = if cfg!(windows) { &['\\'] } else { &['/'] };
    let trimmed = s.trim_end_matches(sep);
    if trimmed.is_empty() && !s.is_empty() { s.chars().take(1).collect() } else { trimmed.to_string() }
}

pub fn same_path(a: &Path, b: &Path) -> bool {
    normalize_path(a) == normalize_path(b)
}

/// The selected workspace.
#[derive(Clone, Debug)]
pub enum WorkspaceKey {
    Home,
    Worktree(PathBuf),
}

impl WorkspaceKey {
    /// Workspace of a terminal with the given `worktree`.
    pub fn of(worktree: Option<&Path>) -> Self {
        match worktree {
            Some(p) => WorkspaceKey::Worktree(p.to_path_buf()),
            None => WorkspaceKey::Home,
        }
    }

    pub fn path(&self) -> Option<&Path> {
        match self {
            WorkspaceKey::Home => None,
            WorkspaceKey::Worktree(p) => Some(p),
        }
    }

    pub fn contains(&self, worktree: Option<&Path>) -> bool {
        *self == WorkspaceKey::of(worktree)
    }
}

impl PartialEq for WorkspaceKey {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (WorkspaceKey::Home, WorkspaceKey::Home) => true,
            (WorkspaceKey::Worktree(a), WorkspaceKey::Worktree(b)) => same_path(a, b),
            _ => false,
        }
    }
}
impl Eq for WorkspaceKey {}

impl Hash for WorkspaceKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            WorkspaceKey::Home => 0u8.hash(state),
            WorkspaceKey::Worktree(p) => {
                1u8.hash(state);
                normalize_path(p).hash(state);
            }
        }
    }
}

/// Terminal ids of `key` in creation order.
pub fn terminals_in(key: &WorkspaceKey, terminals: &[TerminalInfo]) -> Vec<TermId> {
    let mut list: Vec<&TerminalInfo> =
        terminals.iter().filter(|t| key.contains(t.worktree.as_deref())).collect();
    list.sort_by_key(|t| (t.created_unix_ms, t.id));
    list.into_iter().map(|t| t.id).collect()
}

#[cfg(test)]
pub fn count_in(key: &WorkspaceKey, terminals: &[TerminalInfo]) -> usize {
    terminals.iter().filter(|t| key.contains(t.worktree.as_deref())).count()
}

/// Picks the workspace to show at startup from the saved path; Home when it no longer exists.
pub fn restore_selection(saved: Option<&Path>, projects: &[ProjectInfo]) -> WorkspaceKey {
    let Some(saved) = saved else { return WorkspaceKey::Home };
    projects
        .iter()
        .flat_map(|p| p.worktrees.iter())
        .find(|w| same_path(&w.path, saved))
        .map(|w| WorkspaceKey::Worktree(w.path.clone()))
        .unwrap_or(WorkspaceKey::Home)
}

/// True when `key` is Home or a worktree that still exists.
pub fn workspace_exists(key: &WorkspaceKey, projects: &[ProjectInfo]) -> bool {
    match key {
        WorkspaceKey::Home => true,
        WorkspaceKey::Worktree(p) => {
            projects.iter().flat_map(|pr| pr.worktrees.iter()).any(|w| same_path(&w.path, p))
        }
    }
}

/// Next/previous id in `ids` (wraps around). Unknown `current` starts from the first.
pub fn cycle_id(ids: &[TermId], current: Option<TermId>, delta: isize) -> Option<TermId> {
    if ids.is_empty() {
        return None;
    }
    let n = ids.len() as isize;
    let cur = current.and_then(|c| ids.iter().position(|&i| i == c)).unwrap_or(0) as isize;
    Some(ids[(cur + delta).rem_euclid(n) as usize])
}

/// Selected workspace plus the active terminal remembered for each workspace.
#[derive(Debug)]
pub struct Workspaces {
    pub selected: WorkspaceKey,
    active: HashMap<WorkspaceKey, TermId>,
}

impl Default for Workspaces {
    fn default() -> Self {
        Self { selected: WorkspaceKey::Home, active: HashMap::new() }
    }
}

impl Workspaces {
    pub fn set_active(&mut self, key: &WorkspaceKey, id: TermId) {
        self.active.insert(key.clone(), id);
    }

    /// Active terminal of `key` among `ids` (its terminals): the remembered one if it still
    /// exists, otherwise the first.
    pub fn active_in(&self, key: &WorkspaceKey, ids: &[TermId]) -> Option<TermId> {
        self.active.get(key).copied().filter(|a| ids.contains(a)).or_else(|| ids.first().copied())
    }

    /// Forgets remembered terminals that no longer exist.
    pub fn prune(&mut self, alive: &[TermId]) {
        self.active.retain(|_, id| alive.contains(id));
    }
}

/// A worktree creation requested by the user, waiting for the daemon to report it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingWorktree {
    pub project_id: ProjectId,
    pub name: String,
}

/// Path of the new worktree once a Projects update contains it (non-main, path ends with `name`).
pub fn match_pending_worktree(pending: &PendingWorktree, projects: &[ProjectInfo]) -> Option<PathBuf> {
    let want = normalize_str(&pending.name);
    if want.is_empty() {
        return None;
    }
    let sep = if cfg!(windows) { '\\' } else { '/' };
    let project = projects.iter().find(|p| p.id == pending.project_id)?;
    project
        .worktrees
        .iter()
        .filter(|w| !w.is_main)
        .find(|w| {
            let have = normalize_path(&w.path);
            have == want || have.ends_with(&format!("{sep}{want}"))
        })
        .map(|w| w.path.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sich_proto::WorktreeInfo;

    fn term(id: TermId, wt: Option<&str>, created: u64) -> TerminalInfo {
        TerminalInfo {
            id,
            title: String::new(),
            cwd: PathBuf::new(),
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

    fn project(id: u64, worktrees: Vec<WorktreeInfo>) -> ProjectInfo {
        ProjectInfo { id, name: "p".into(), path: PathBuf::new(), is_git: true, worktrees, error: None, group: None }
    }

    #[test]
    fn trailing_separators_are_ignored() {
        assert_eq!(WorkspaceKey::Worktree("/a/b/".into()), WorkspaceKey::Worktree("/a/b".into()));
        assert_ne!(WorkspaceKey::Worktree("/a/b".into()), WorkspaceKey::Home);
    }

    #[cfg(windows)]
    #[test]
    fn windows_paths_ignore_case_and_slash_style() {
        assert!(same_path(Path::new("C:\\Work\\App\\"), Path::new("c:/work/app")));
        assert!(same_path(Path::new("\\\\?\\C:\\Work"), Path::new("c:\\work")));
        assert!(!same_path(Path::new("C:\\Work\\App"), Path::new("C:\\Work\\App2")));
    }

    #[test]
    fn root_path_is_not_emptied() {
        assert!(!normalize_path(Path::new("/")).is_empty());
    }

    #[test]
    fn terminals_are_grouped_and_ordered_by_creation() {
        let list = vec![term(3, None, 30), term(1, Some("/w/a"), 20), term(2, Some("/w/a/"), 10), term(4, Some("/w/b"), 5)];
        assert_eq!(terminals_in(&WorkspaceKey::Home, &list), vec![3]);
        assert_eq!(terminals_in(&WorkspaceKey::Worktree("/w/a".into()), &list), vec![2, 1]);
        assert_eq!(count_in(&WorkspaceKey::Worktree("/w/b".into()), &list), 1);
        assert!(terminals_in(&WorkspaceKey::Worktree("/w/none".into()), &list).is_empty());
    }

    #[test]
    fn restore_finds_existing_worktree_else_home() {
        let projects = vec![project(1, vec![wt("/w/a", true), wt("/w/a-x", false)])];
        assert_eq!(restore_selection(Some(Path::new("/w/a-x/")), &projects), WorkspaceKey::Worktree("/w/a-x".into()));
        assert_eq!(restore_selection(Some(Path::new("/gone")), &projects), WorkspaceKey::Home);
        assert_eq!(restore_selection(None, &projects), WorkspaceKey::Home);
        assert_eq!(restore_selection(Some(Path::new("/w/a")), &[]), WorkspaceKey::Home);
    }

    #[test]
    fn workspace_exists_checks_projects() {
        let projects = vec![project(1, vec![wt("/w/a", true)])];
        assert!(workspace_exists(&WorkspaceKey::Home, &[]));
        assert!(workspace_exists(&WorkspaceKey::Worktree("/w/a".into()), &projects));
        assert!(!workspace_exists(&WorkspaceKey::Worktree("/w/b".into()), &projects));
    }

    #[test]
    fn active_terminal_is_remembered_per_workspace() {
        let mut ws = Workspaces::default();
        let a = WorkspaceKey::Worktree("/w/a".into());
        assert_eq!(ws.active_in(&a, &[5, 6]), Some(5));
        ws.set_active(&a, 6);
        ws.set_active(&WorkspaceKey::Home, 1);
        assert_eq!(ws.active_in(&a, &[5, 6]), Some(6));
        assert_eq!(ws.active_in(&WorkspaceKey::Home, &[1, 2]), Some(1));
        // The remembered terminal vanished: fall back to the first.
        assert_eq!(ws.active_in(&a, &[5]), Some(5));
        assert_eq!(ws.active_in(&a, &[]), None);
        ws.prune(&[1]);
        assert_eq!(ws.active_in(&a, &[5, 6]), Some(5));
    }

    #[test]
    fn cycling_wraps_both_ways() {
        assert_eq!(cycle_id(&[1, 2, 3], Some(3), 1), Some(1));
        assert_eq!(cycle_id(&[1, 2, 3], Some(1), -1), Some(3));
        assert_eq!(cycle_id(&[1, 2, 3], None, 1), Some(2));
        assert_eq!(cycle_id(&[], Some(1), 1), None);
    }

    #[test]
    fn pending_worktree_matches_by_folder_name() {
        let pending = PendingWorktree { project_id: 7, name: "feature-x".into() };
        let projects = vec![
            project(1, vec![wt("/w/feature-x", false)]),
            project(7, vec![wt("/w/app", true), wt("/w/app-wt/feature-x/", false)]),
        ];
        assert_eq!(match_pending_worktree(&pending, &projects), Some(PathBuf::from("/w/app-wt/feature-x/")));
        let early = vec![project(7, vec![wt("/w/app", true)])];
        assert_eq!(match_pending_worktree(&pending, &early), None);
        // A partial folder-name match is not a match.
        let other = vec![project(7, vec![wt("/w/old-feature-x", false)])];
        assert_eq!(match_pending_worktree(&pending, &other), None);
        // The main worktree is never the new one.
        let main = vec![project(7, vec![wt("/w/feature-x", true)])];
        assert_eq!(match_pending_worktree(&pending, &main), None);
    }
}
