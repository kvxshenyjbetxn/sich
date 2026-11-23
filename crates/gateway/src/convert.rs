//! sich-proto -> sich-remote-proto conversion (the phone never sees daemon types).

use sich_proto as dp;
use sich_remote_proto as rp;

pub fn term_ref(daemon: &str, id: dp::TermId) -> rp::TermRef {
    rp::TermRef { daemon: daemon.to_string(), id }
}

fn kind(kind: dp::AgentKind) -> rp::AgentKind {
    match kind {
        dp::AgentKind::Claude => rp::AgentKind::Claude,
        dp::AgentKind::Gemini => rp::AgentKind::Gemini,
        dp::AgentKind::Codex => rp::AgentKind::Codex,
        dp::AgentKind::Omp => rp::AgentKind::Omp,
        dp::AgentKind::Other => rp::AgentKind::Other,
    }
}

fn state(state: dp::AgentState) -> rp::AgentState {
    match state {
        dp::AgentState::Working => rp::AgentState::Working,
        dp::AgentState::Waiting => rp::AgentState::Waiting,
        dp::AgentState::Done => rp::AgentState::Done,
        dp::AgentState::Failed => rp::AgentState::Failed,
        dp::AgentState::Idle => rp::AgentState::Idle,
    }
}

pub fn agent(status: &dp::AgentStatus) -> rp::Agent {
    rp::Agent {
        kind: kind(status.kind),
        state: state(status.state),
        unread: status.unread,
        since_unix_ms: status.since_unix_ms,
        title: status.title.clone(),
        detail: status.detail.clone(),
        model: status.model.clone(),
        prompt: status.prompt.clone(),
        message: status.message.clone(),
        subagents_working: status.subagents.iter().filter(|s| s.state == dp::AgentState::Working).count() as u32,
    }
}

pub fn terminal(daemon: &str, info: &dp::TerminalInfo) -> rp::Terminal {
    rp::Terminal {
        term: term_ref(daemon, info.id),
        title: info.title.clone(),
        cwd: info.cwd.to_string_lossy().into_owned(),
        worktree: info.worktree.as_ref().map(|p| p.to_string_lossy().into_owned()),
        cols: info.cols,
        rows: info.rows,
        exited: info.exited,
        created_unix_ms: info.created_unix_ms,
        agent: info.agent.as_ref().map(agent),
    }
}

pub fn project(info: &dp::ProjectInfo) -> rp::Project {
    rp::Project {
        id: info.id,
        name: info.name.clone(),
        path: info.path.to_string_lossy().into_owned(),
        is_git: info.is_git,
        group: info.group,
        worktrees: info
            .worktrees
            .iter()
            .map(|w| rp::Worktree {
                path: w.path.to_string_lossy().into_owned(),
                branch: w.branch.clone(),
                is_main: w.is_main,
                dirty: w.dirty,
            })
            .collect(),
        error: info.error.clone(),
    }
}

pub fn workspace(
    host_name: &str,
    terminals: &[(String, dp::TerminalInfo)],
    projects: &[dp::ProjectInfo],
    groups: &[dp::GroupInfo],
) -> rp::Workspace {
    rp::Workspace {
        host_name: host_name.to_string(),
        groups: groups.iter().map(|g| rp::Group { id: g.id, name: g.name.clone() }).collect(),
        projects: projects.iter().map(project).collect(),
        terminals: terminals.iter().map(|(daemon, info)| terminal(daemon, info)).collect(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn status(kind: dp::AgentKind, subs: &[dp::AgentState]) -> dp::AgentStatus {
        dp::AgentStatus {
            kind,
            state: dp::AgentState::Working,
            unread: true,
            since_unix_ms: 5,
            detail: Some("Bash".into()),
            model: Some("m".into()),
            title: Some("t".into()),
            prompt: Some("p".into()),
            message: Some("msg".into()),
            subagents: subs
                .iter()
                .enumerate()
                .map(|(i, s)| dp::SubagentStatus {
                    id: i.to_string(),
                    agent_type: None,
                    description: None,
                    state: *s,
                    detail: None,
                    since_unix_ms: 0,
                    model: None,
                })
                .collect(),
            session_id: Some("secret-session".into()),
        }
    }

    #[test]
    fn agent_fields_and_subagent_count() {
        let a = agent(&status(dp::AgentKind::Omp, &[dp::AgentState::Working, dp::AgentState::Waiting, dp::AgentState::Working]));
        assert_eq!(a.kind, rp::AgentKind::Omp);
        assert_eq!(a.state, rp::AgentState::Working);
        assert!(a.unread);
        assert_eq!((a.since_unix_ms, a.subagents_working), (5, 2));
        assert_eq!(a.detail.as_deref(), Some("Bash"));
        assert_eq!(a.message.as_deref(), Some("msg"));
    }

    #[test]
    fn every_kind_and_state_maps() {
        use dp::AgentKind as K;
        let kinds = [K::Claude, K::Gemini, K::Codex, K::Other, K::Omp].map(kind);
        assert_eq!(
            kinds,
            [rp::AgentKind::Claude, rp::AgentKind::Gemini, rp::AgentKind::Codex, rp::AgentKind::Other, rp::AgentKind::Omp]
        );
        use dp::AgentState as S;
        let states = [S::Working, S::Waiting, S::Done, S::Failed, S::Idle].map(state);
        assert_eq!(
            states,
            [rp::AgentState::Working, rp::AgentState::Waiting, rp::AgentState::Done, rp::AgentState::Failed, rp::AgentState::Idle]
        );
    }

    #[test]
    fn terminal_conversion_uses_strings_for_paths() {
        let info = dp::TerminalInfo {
            id: 3,
            title: "zsh".into(),
            cwd: PathBuf::from("C:\\work\\app"),
            cols: 100,
            rows: 30,
            exited: Some(Some(2)),
            created_unix_ms: 9,
            worktree: Some(PathBuf::from("C:\\work\\app-wt")),
            agent: None,
        };
        let t = terminal("sock-a", &info);
        assert_eq!(t.term, rp::TermRef { daemon: "sock-a".into(), id: 3 });
        assert_eq!(t.cwd, "C:\\work\\app");
        assert_eq!(t.worktree.as_deref(), Some("C:\\work\\app-wt"));
        assert_eq!((t.cols, t.rows, t.exited, t.created_unix_ms), (100, 30, Some(Some(2)), 9));
        assert!(t.agent.is_none());
    }

    #[test]
    fn project_and_workspace_conversion() {
        let p = dp::ProjectInfo {
            id: 1,
            name: "app".into(),
            path: PathBuf::from("/w/app"),
            is_git: true,
            worktrees: vec![dp::WorktreeInfo {
                path: PathBuf::from("/w/app"),
                branch: Some("main".into()),
                head: "abc".into(),
                is_main: true,
                dirty: true,
            }],
            error: Some("boom".into()),
            group: Some(7),
        };
        let g = dp::GroupInfo { id: 7, name: "work".into() };
        let ws = workspace("HOST", &[], std::slice::from_ref(&p), std::slice::from_ref(&g));
        assert_eq!(ws.host_name, "HOST");
        assert_eq!(ws.groups, vec![rp::Group { id: 7, name: "work".into() }]);
        let rp0 = &ws.projects[0];
        assert_eq!((rp0.id, rp0.is_git, rp0.group), (1, true, Some(7)));
        assert_eq!(rp0.error.as_deref(), Some("boom"));
        assert_eq!(rp0.worktrees[0].branch.as_deref(), Some("main"));
        assert!(rp0.worktrees[0].is_main && rp0.worktrees[0].dirty);
    }
}
