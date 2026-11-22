//! Which worktrees the PR store keeps up to date: the selected one plus every worktree row the
//! sidebar currently shows. Pure, so it is unit-tested.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use sich_proto::{GroupId, GroupInfo, ProjectId, ProjectInfo};

use crate::worktree_visibility::{self, Store};
use crate::workspace::normalize_path;

/// A worktree and the branch the sidebar knows for it (`None` = detached HEAD).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TrackedRow {
    pub path: PathBuf,
    pub branch: Option<String>,
}

/// Rows of the sidebar as the user sees them: projects in collapsed groups or collapsed projects
/// show none, the visibility rules decide which worktrees of an expanded project are listed. The
/// selected worktree is always included, even when its project is collapsed. Projects that are
/// not Git repositories or failed to load contribute nothing.
#[allow(clippy::too_many_arguments)]
pub(crate) fn tracked_worktrees(
    projects: &[ProjectInfo],
    groups: &[GroupInfo],
    collapsed_projects: &HashSet<ProjectId>,
    collapsed_groups: &HashSet<GroupId>,
    visibility: &Store,
    show_all: bool,
    selected: Option<&Path>,
) -> Vec<TrackedRow> {
    let mut rows: Vec<TrackedRow> = Vec::new();
    let mut seen = HashSet::new();
    let mut push = |path: &Path, branch: &Option<String>, rows: &mut Vec<TrackedRow>| {
        if seen.insert(normalize_path(path)) {
            rows.push(TrackedRow {
                path: path.to_path_buf(),
                branch: branch.clone().filter(|branch| !branch.is_empty()),
            });
        }
    };
    for project in projects {
        if !project.is_git || project.error.is_some() || collapsed_projects.contains(&project.id) {
            continue;
        }
        let group_collapsed = project
            .group
            .filter(|gid| groups.iter().any(|group| group.id == *gid))
            .is_some_and(|gid| collapsed_groups.contains(&gid));
        if group_collapsed {
            continue;
        }
        let (shown, _) = worktree_visibility::sidebar_worktrees(project, visibility, show_all, selected);
        for worktree in shown {
            push(&worktree.path, &worktree.branch, &mut rows);
        }
    }
    if let Some(selected) = selected {
        for worktree in projects.iter().flat_map(|project| &project.worktrees) {
            if normalize_path(&worktree.path) == normalize_path(selected) {
                push(&worktree.path, &worktree.branch, &mut rows);
            }
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use sich_proto::WorktreeInfo;

    use super::*;

    fn wt(path: &str, branch: Option<&str>, is_main: bool) -> WorktreeInfo {
        WorktreeInfo {
            path: PathBuf::from(path),
            branch: branch.map(str::to_owned),
            head: "abc1234".into(),
            is_main,
            dirty: false,
        }
    }

    fn project(id: u64, path: &str, group: Option<u64>, worktrees: Vec<WorktreeInfo>) -> ProjectInfo {
        ProjectInfo {
            id,
            name: format!("p{id}"),
            path: PathBuf::from(path),
            is_git: true,
            worktrees,
            error: None,
            group,
        }
    }

    fn group(id: u64) -> GroupInfo {
        GroupInfo { id, name: format!("g{id}") }
    }

    fn paths(rows: &[TrackedRow]) -> Vec<String> {
        rows.iter().map(|row| row.path.to_string_lossy().into_owned()).collect()
    }

    fn run(
        projects: &[ProjectInfo],
        groups: &[GroupInfo],
        collapsed: &[u64],
        collapsed_groups: &[u64],
        store: &Store,
        selected: Option<&str>,
    ) -> Vec<TrackedRow> {
        tracked_worktrees(
            projects,
            groups,
            &collapsed.iter().copied().collect(),
            &collapsed_groups.iter().copied().collect(),
            store,
            false,
            selected.map(Path::new),
        )
    }

    fn two_worktrees() -> Vec<ProjectInfo> {
        vec![project(
            1,
            "/w/app",
            None,
            vec![wt("/w/app", Some("main"), true), wt("/w/app-x", Some("feat/x"), false)],
        )]
    }

    #[test]
    fn every_row_of_an_expanded_project_is_tracked_with_its_branch() {
        let rows = run(&two_worktrees(), &[], &[], &[], &Store::default(), None);
        assert_eq!(paths(&rows), ["/w/app", "/w/app-x"]);
        assert_eq!(rows[1].branch.as_deref(), Some("feat/x"));
    }

    #[test]
    fn collapsed_projects_and_groups_show_no_rows() {
        let projects = two_worktrees();
        assert!(run(&projects, &[], &[1], &[], &Store::default(), None).is_empty());
        let mut grouped = projects.clone();
        grouped[0].group = Some(5);
        assert!(run(&grouped, &[group(5)], &[], &[5], &Store::default(), None).is_empty());
        assert_eq!(run(&grouped, &[group(5)], &[], &[6], &Store::default(), None).len(), 2);
        // A group id the daemon no longer lists is a plain ungrouped project.
        assert_eq!(run(&grouped, &[], &[], &[5], &Store::default(), None).len(), 2);
    }

    #[test]
    fn the_selected_worktree_is_tracked_even_inside_a_collapsed_project() {
        let rows = run(&two_worktrees(), &[], &[1], &[], &Store::default(), Some("/w/app-x"));
        assert_eq!(paths(&rows), ["/w/app-x"]);
        assert_eq!(rows[0].branch.as_deref(), Some("feat/x"));
        assert!(run(&two_worktrees(), &[], &[1], &[], &Store::default(), Some("/elsewhere")).is_empty());
    }

    #[test]
    fn hidden_external_worktrees_are_not_tracked_unless_selected() {
        let projects = two_worktrees();
        let store: Store = serde_json::from_str(&format!(
            r#"{{"{}": {{"initialized": true, "paths": {{"{}": "hidden"}}}}}}"#,
            normalize_path(Path::new("/w/app")).replace('\\', "\\\\"),
            normalize_path(Path::new("/w/app-x")).replace('\\', "\\\\"),
        ))
        .unwrap();
        assert_eq!(paths(&run(&projects, &[], &[], &[], &store, None)), ["/w/app"]);
        // Selecting a hidden worktree makes the sidebar show it, so it is tracked too.
        assert_eq!(
            paths(&run(&projects, &[], &[], &[], &store, Some("/w/app-x"))),
            ["/w/app", "/w/app-x"]
        );
        let all = tracked_worktrees(&projects, &[], &HashSet::new(), &HashSet::new(), &store, true, None);
        assert_eq!(all.len(), 2, "show_external_worktrees lists everything");
    }

    #[test]
    fn detached_and_non_git_rows_have_no_branch_or_are_skipped() {
        let mut projects = vec![project(
            1,
            "/w/app",
            None,
            vec![wt("/w/app", None, true), wt("/w/app-y", Some(""), false)],
        )];
        let rows = run(&projects, &[], &[], &[], &Store::default(), None);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| row.branch.is_none()), "empty and missing branches both mean none");
        projects[0].is_git = false;
        assert!(run(&projects, &[], &[], &[], &Store::default(), Some("/w/app")).len() <= 1);
        projects[0].is_git = true;
        projects[0].error = Some("Folder not found".into());
        assert!(run(&projects, &[], &[], &[], &Store::default(), None).is_empty());
    }

    #[test]
    fn a_worktree_listed_twice_is_tracked_once() {
        let projects = two_worktrees();
        let rows = run(&projects, &[], &[], &[], &Store::default(), Some("/w/app"));
        assert_eq!(paths(&rows), ["/w/app", "/w/app-x"]);
    }
}
