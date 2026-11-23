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

/// Terminal that receives text sent to "the agent" of a worktree. `terms` are the worktree's
/// live terminals as `(id, has_agent)` in creation order. Prefers the focused terminal, then the
/// workspace's active one, then the first agent terminal.
pub fn pick_agent_terminal(
    terms: &[(TermId, bool)],
    focused: Option<TermId>,
    active: Option<TermId>,
) -> Option<TermId> {
    let is_agent = |id: TermId| terms.iter().any(|&(t, agent)| t == id && agent);
    focused
        .filter(|&id| is_agent(id))
        .or_else(|| active.filter(|&id| is_agent(id)))
        .or_else(|| terms.iter().find(|&&(_, agent)| agent).map(|&(id, _)| id))
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

    pub(crate) fn replace_active(&mut self, key: &WorkspaceKey, old: TermId, new: TermId) {
        if let Some(active) = self.active.get_mut(key).filter(|active| **active == old) {
            *active = new;
        }
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

/// Project root and new worktree path once a Projects update contains the created branch and folder.
pub fn match_pending_worktree<'a>(
    pending: &PendingWorktree,
    projects: &'a [ProjectInfo],
) -> Option<(&'a Path, &'a Path)> {
    let want = normalize_str(&pending.name);
    if want.is_empty() {
        return None;
    }
    let sep = if cfg!(windows) { '\\' } else { '/' };
    let suffix = format!("{sep}{want}");
    // Branch names like `feat/x` live in a flat folder (`feat-x`).
    let flat_suffix = format!("{sep}{}", want.replace(['/', '\\'], "-"));
    let project = projects.iter().find(|p| p.id == pending.project_id)?;
    // The default location puts the worktree next to the project: `<repo>-<flat name>`.
    let sibling_suffix = project
        .path
        .file_name()
        .map(|repo| format!("{sep}{}-{}", normalize_str(&repo.to_string_lossy()), want.replace(['/', '\\'], "-")));
    project
        .worktrees
        .iter()
        .filter(|w| !w.is_main && w.branch.as_deref() == Some(&pending.name))
        .find(|w| {
            let have = normalize_path(&w.path);
            have == want
                || have.ends_with(&suffix)
                || have.ends_with(&flat_suffix)
                || sibling_suffix.as_ref().is_some_and(|s| have.ends_with(s))
        })
        .map(|w| (project.path.as_path(), w.path.as_path()))
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
    fn agent_terminal_choice_prefers_focused_then_active_then_first() {
        let terms = [(1, false), (2, true), (3, true), (4, true)];
        assert_eq!(pick_agent_terminal(&terms, Some(3), Some(4)), Some(3));
        // A focused plain shell is not an agent: fall back to the active agent terminal.
        assert_eq!(pick_agent_terminal(&terms, Some(1), Some(4)), Some(4));
        // Focused/active ids from another worktree (not in `terms`) are ignored.
        assert_eq!(pick_agent_terminal(&terms, Some(99), Some(98)), Some(2));
        assert_eq!(pick_agent_terminal(&terms, None, None), Some(2));
        assert_eq!(pick_agent_terminal(&terms, None, Some(1)), Some(2));
    }

    #[test]
    fn agent_terminal_choice_is_none_without_agents() {
        assert_eq!(pick_agent_terminal(&[], Some(1), Some(1)), None);
        assert_eq!(pick_agent_terminal(&[(1, false), (2, false)], Some(1), Some(2)), None);
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
    fn restart_only_replaces_the_active_terminal_if_it_was_the_source() {
        let mut ws = Workspaces::default();
        let key = WorkspaceKey::Home;
        ws.set_active(&key, 2);
        ws.replace_active(&key, 1, 3);
        assert_eq!(ws.active_in(&key, &[3, 2]), Some(2));
        ws.replace_active(&key, 2, 4);
        ws.prune(&[3, 4]);
        assert_eq!(ws.active_in(&key, &[3, 4]), Some(4));
    }

    #[test]
    fn cycling_wraps_both_ways() {
        assert_eq!(cycle_id(&[1, 2, 3], Some(3), 1), Some(1));
        assert_eq!(cycle_id(&[1, 2, 3], Some(1), -1), Some(3));
        assert_eq!(cycle_id(&[1, 2, 3], None, 1), Some(2));
        assert_eq!(cycle_id(&[], Some(1), 1), None);
    }

    #[test]
    fn pending_worktree_with_slash_name_matches_the_flat_folder() {
        let pending = PendingWorktree { project_id: 7, name: "feat/x".into() };
        let named = |path| WorktreeInfo { branch: Some(pending.name.clone()), ..wt(path, false) };
        let projects = vec![project(7, vec![wt("/w/app", true), named("/w/app-wt/feat-x")])];
        assert_eq!(
            match_pending_worktree(&pending, &projects),
            Some((projects[0].path.as_path(), Path::new("/w/app-wt/feat-x")))
        );
        // A different branch in that folder is not the new worktree.
        let other = vec![project(7, vec![WorktreeInfo { branch: Some("feat-x".into()), ..wt("/w/app-wt/feat-x", false) }])];
        assert_eq!(match_pending_worktree(&pending, &other), None);
    }

    #[test]
    fn pending_worktree_matches_the_sibling_folder_next_to_the_project() {
        let pending = PendingWorktree { project_id: 7, name: "feat/x".into() };
        let named = |path| WorktreeInfo { branch: Some(pending.name.clone()), ..wt(path, false) };
        let mut app = project(7, vec![wt("/w/app", true), named("/w/app-feat-x")]);
        app.path = PathBuf::from("/w/app");
        let projects = vec![app];
        assert_eq!(
            match_pending_worktree(&pending, &projects),
            Some((projects[0].path.as_path(), Path::new("/w/app-feat-x")))
        );
        // Another repo's sibling folder with the same suffix is not this worktree.
        let mut other = project(7, vec![named("/w/old-feat-x")]);
        other.path = PathBuf::from("/w/app");
        assert_eq!(match_pending_worktree(&pending, &[other]), None);
    }

    #[test]
    fn pending_worktree_matches_by_folder_name() {
        let pending = PendingWorktree { project_id: 7, name: "feature-x".into() };
        let named = |path, main| WorktreeInfo { branch: Some(pending.name.clone()), ..wt(path, main) };
        let projects = vec![
            project(1, vec![named("/w/feature-x", false)]),
            project(7, vec![wt("/w/app", true), named("/w/app-wt/feature-x/", false)]),
        ];
        assert_eq!(match_pending_worktree(&pending, &projects), Some((projects[1].path.as_path(), Path::new("/w/app-wt/feature-x/"))));
        let early = vec![project(7, vec![wt("/w/app", true)])];
        assert_eq!(match_pending_worktree(&pending, &early), None);
        // A partial folder-name match is not a match.
        let other = vec![project(7, vec![named("/w/old-feature-x", false)])];
        assert_eq!(match_pending_worktree(&pending, &other), None);
        // The main worktree is never the new one.
        let main = vec![project(7, vec![named("/w/feature-x", true)])];
        assert_eq!(match_pending_worktree(&pending, &main), None);
        // A detached checkout in an unrelated directory with this basename is external.
        let detached = vec![project(7, vec![wt("/other/feature-x", false)])];
        assert_eq!(match_pending_worktree(&pending, &detached), None);
    }
}
