//! Pure helpers for project groups (grouping the sidebar list, roll-up badge, auto-expand).

use std::collections::HashSet;

use sich_proto::{GroupId, GroupInfo, ProjectId, ProjectInfo, TerminalInfo};

use crate::agent_badge::Badge;
use crate::sidebar::project_badge;
use crate::workspace::WorkspaceKey;

/// Groups in order with their projects, plus the ungrouped projects. Projects whose group is
/// `None` or refers to an unknown group are ungrouped. Project order is preserved everywhere.
#[allow(clippy::type_complexity)]
pub fn group_projects<'a>(
    projects: &'a [ProjectInfo],
    groups: &[GroupInfo],
) -> (Vec<(GroupInfo, Vec<&'a ProjectInfo>)>, Vec<&'a ProjectInfo>) {
    let mut grouped: Vec<(GroupInfo, Vec<&'a ProjectInfo>)> =
        groups.iter().map(|g| (g.clone(), Vec::new())).collect();
    let mut ungrouped = Vec::new();
    for p in projects {
        match p.group.and_then(|gid| grouped.iter_mut().find(|(g, _)| g.id == gid)) {
            Some((_, list)) => list.push(p),
            None => ungrouped.push(p),
        }
    }
    (grouped, ungrouped)
}

/// Roll-up badge of every terminal in every worktree of the given projects (the most important
/// of the per-project roll-ups).
pub fn group_badge(projects: &[&ProjectInfo], infos: &[TerminalInfo]) -> Option<Badge> {
    best_of(projects.iter().filter_map(|p| project_badge(p, infos)))
}

fn best_of(badges: impl IntoIterator<Item = Badge>) -> Option<Badge> {
    let mut best: Option<Badge> = None;
    for b in badges {
        if best.as_ref().is_none_or(|x| b.kind < x.kind) {
            best = Some(b);
        }
    }
    best
}

/// Makes the selected worktree visible: removes its project (and that project's group) from the
/// collapsed sets.
pub fn expand_to(
    key: &WorkspaceKey,
    projects: &[ProjectInfo],
    collapsed: &mut HashSet<ProjectId>,
    collapsed_groups: &mut HashSet<GroupId>,
) {
    if !matches!(key, WorkspaceKey::Worktree(_)) {
        return;
    }
    for p in projects {
        if p.worktrees.iter().any(|w| WorkspaceKey::Worktree(w.path.clone()) == *key) {
            collapsed.remove(&p.id);
            if let Some(g) = p.group {
                collapsed_groups.remove(&g);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use sich_proto::{AgentKind, AgentState, AgentStatus, WorktreeInfo};

    use super::*;
    use crate::agent_badge::BadgeKind;

    fn project(id: u64, group: Option<u64>, wts: &[&str]) -> ProjectInfo {
        ProjectInfo {
            id,
            name: format!("p{id}"),
            path: PathBuf::from(wts.first().copied().unwrap_or("/x")),
            is_git: true,
            worktrees: wts
                .iter()
                .map(|p| WorktreeInfo {
                    path: PathBuf::from(p),
                    branch: None,
                    head: String::new(),
                    is_main: false,
                    dirty: false,
                })
                .collect(),
            error: None,
            group,
        }
    }

    fn group(id: u64, name: &str) -> GroupInfo {
        GroupInfo { id, name: name.into() }
    }

    fn term(id: u64, worktree: &str, state: AgentState, unread: bool) -> TerminalInfo {
        TerminalInfo {
            id,
            title: String::new(),
            cwd: PathBuf::new(),
            cols: 80,
            rows: 24,
            exited: None,
            created_unix_ms: id,
            worktree: Some(PathBuf::from(worktree)),
            agent: Some(AgentStatus {
                kind: AgentKind::Claude,
                state,
                unread,
                since_unix_ms: 1,
                detail: None,
                model: None,
                title: None,
                prompt: None,
                message: None,
                subagents: Vec::new(),
                session_id: None,
            }),
        }
    }

    #[test]
    fn groups_keep_order_and_collect_their_projects() {
        let projects = vec![
            project(1, Some(2), &["/a"]),
            project(2, None, &["/b"]),
            project(3, Some(1), &["/c"]),
            project(4, Some(2), &["/d"]),
        ];
        let groups = vec![group(1, "g1"), group(2, "g2"), group(3, "empty")];
        let (grouped, ungrouped) = group_projects(&projects, &groups);
        assert_eq!(grouped.len(), 3);
        assert_eq!(grouped[0].0.id, 1);
        assert_eq!(grouped[0].1.iter().map(|p| p.id).collect::<Vec<_>>(), [3]);
        assert_eq!(grouped[1].1.iter().map(|p| p.id).collect::<Vec<_>>(), [1, 4]);
        assert!(grouped[2].1.is_empty());
        assert_eq!(ungrouped.iter().map(|p| p.id).collect::<Vec<_>>(), [2]);
    }

    #[test]
    fn unknown_group_means_ungrouped() {
        let projects = vec![project(1, Some(99), &["/a"]), project(2, None, &["/b"])];
        let (grouped, ungrouped) = group_projects(&projects, &[]);
        assert!(grouped.is_empty());
        assert_eq!(ungrouped.len(), 2);
    }

    #[test]
    fn no_projects_no_groups() {
        let (grouped, ungrouped) = group_projects(&[], &[]);
        assert!(grouped.is_empty() && ungrouped.is_empty());
    }

    #[test]
    fn group_badge_rolls_up_all_worktrees_of_all_projects() {
        let p1 = project(1, Some(1), &["/a", "/a-x"]);
        let p2 = project(2, Some(1), &["/b"]);
        let infos = vec![
            term(1, "/a-x", AgentState::Done, true),
            term(2, "/b", AgentState::Working, false),
            term(3, "/other", AgentState::Waiting, false),
        ];
        let b = group_badge(&[&p1, &p2], &infos).unwrap();
        assert_eq!(b.kind, BadgeKind::Working);
        let infos = vec![term(1, "/a-x", AgentState::Waiting, false), infos[1].clone()];
        assert_eq!(group_badge(&[&p1, &p2], &infos).unwrap().kind, BadgeKind::Waiting);
    }

    #[test]
    fn group_badge_none_without_agents_or_projects() {
        let p1 = project(1, Some(1), &["/a"]);
        assert!(group_badge(&[&p1], &[]).is_none());
        assert!(group_badge(&[], &[term(1, "/a", AgentState::Working, false)]).is_none());
        assert!(group_badge(&[&p1], &[term(1, "/a", AgentState::Idle, false)]).is_none());
    }

    #[test]
    fn expand_to_opens_project_and_its_group() {
        let projects = vec![project(1, Some(5), &["/a", "/a-x"]), project(2, Some(6), &["/b"])];
        let mut collapsed: HashSet<ProjectId> = [1, 2].into();
        let mut cg: HashSet<GroupId> = [5, 6].into();
        expand_to(&WorkspaceKey::Worktree("/a-x".into()), &projects, &mut collapsed, &mut cg);
        assert_eq!(collapsed, HashSet::from([2]));
        assert_eq!(cg, HashSet::from([6]));
    }

    #[test]
    fn expand_to_ignores_home_and_ungrouped_projects() {
        let projects = vec![project(1, None, &["/a"])];
        let mut collapsed: HashSet<ProjectId> = [1].into();
        let mut cg: HashSet<GroupId> = [5].into();
        expand_to(&WorkspaceKey::Home, &projects, &mut collapsed, &mut cg);
        assert_eq!(collapsed.len(), 1);
        expand_to(&WorkspaceKey::Worktree("/a".into()), &projects, &mut collapsed, &mut cg);
        assert!(collapsed.is_empty());
        assert_eq!(cg, HashSet::from([5]));
    }
}
