use std::path::PathBuf;

use super::*;

fn info() -> TerminalInfo {
    TerminalInfo {
        id: 7,
        title: "pwsh".into(),
        cwd: PathBuf::from("C:/Users/test"),
        cols: 120,
        rows: 32,
        exited: Some(Some(-1)),
        created_unix_ms: 1_700_000_000_000,
        worktree: Some(PathBuf::from("/wt/a")),
        agent: Some(status()),
    }
}

fn status() -> AgentStatus {
    AgentStatus {
        kind: AgentKind::Claude,
        state: AgentState::Waiting,
        unread: true,
        since_unix_ms: 1_700_000_000_123,
        detail: Some("Claude needs your permission".into()),
        model: Some("claude-opus-5-5".into()),
        title: Some("Sync phone feature".into()),
        prompt: Some("fix the bug".into()),
        message: Some("Done, tests pass".into()),
        session_id: Some("abc12345-6789".into()),
        subagents: vec![
            SubagentStatus {
                id: "agent-abc123".into(),
                agent_type: Some("Explore".into()),
                description: Some("Find the parser".into()),
                state: AgentState::Working,
                detail: Some("Grep".into()),
                since_unix_ms: 1_700_000_000_050,
            },
            SubagentStatus {
                id: "agent-def".into(),
                agent_type: None,
                description: None,
                state: AgentState::Waiting,
                detail: None,
                since_unix_ms: 1_700_000_000_060,
            },
        ],
    }
}

fn project() -> ProjectInfo {
    ProjectInfo {
        id: 2,
        name: "repo".into(),
        path: PathBuf::from("/repo"),
        is_git: true,
        worktrees: vec![
            WorktreeInfo {
                path: PathBuf::from("/repo"),
                branch: Some("main".into()),
                head: "abc1234".into(),
                is_main: true,
                dirty: false,
            },
            WorktreeInfo {
                path: PathBuf::from("/wt/a"),
                branch: None,
                head: "def5678".into(),
                is_main: false,
                dirty: true,
            },
        ],
        error: Some("Folder not found".into()),
        group: Some(7),
    }
}

#[test]
fn client_msgs_round_trip() {
    let msgs = vec![
        ClientMsg::Hello { proto_version: PROTO_VERSION, client_name: "t".into() },
        ClientMsg::ListTerminals,
        ClientMsg::CreateTerminal {
            cwd: Some(PathBuf::from("/tmp")),
            shell: Some("cmd.exe".into()),
            args: vec!["/c".into(), "echo".into()],
            cols: 80,
            rows: 24,
            worktree: Some(PathBuf::from("/wt")),
        },
        ClientMsg::CreateTerminal { cwd: None, shell: None, args: vec![], cols: 1, rows: 1, worktree: None },
        ClientMsg::Subscribe { id: 1, resume_from_seq: Some(42) },
        ClientMsg::Subscribe { id: 1, resume_from_seq: None },
        ClientMsg::Unsubscribe { id: 2 },
        ClientMsg::Input { id: 3, bytes: vec![0, 1, 255] },
        ClientMsg::Resize { id: 4, cols: 100, rows: 50 },
        ClientMsg::KillTerminal { id: 5 },
        ClientMsg::ListProjects,
        ClientMsg::RefreshProjects,
        ClientMsg::AddProject { path: PathBuf::from("/repo"), group: None },
        ClientMsg::AddProject { path: PathBuf::from("/repo"), group: Some(2) },
        ClientMsg::RemoveProject { id: 3 },
        ClientMsg::CreateWorktree { project_id: 1, name: "feat".into(), base: Some("main".into()) },
        ClientMsg::CreateWorktree { project_id: 1, name: "feat".into(), base: None },
        ClientMsg::RemoveWorktree {
            project_id: 1,
            path: PathBuf::from("/wt/feat"),
            force: true,
            delete_branch: false,
        },
        ClientMsg::AgentHook {
            term_id: 3,
            agent: AgentKind::Claude,
            payload: r#"{"hook_event_name":"Stop"}"#.into(),
        },
        ClientMsg::AgentHook { term_id: 1, agent: AgentKind::Gemini, payload: String::new() },
        ClientMsg::AgentHook { term_id: 1, agent: AgentKind::Omp, payload: r#"{"state":"idle"}"#.into() },
        ClientMsg::MarkSeen { id: 8 },
        ClientMsg::Shutdown,
        ClientMsg::CreateGroup { name: "game".into() },
        ClientMsg::RenameGroup { id: 2, name: "игры".into() },
        ClientMsg::RemoveGroup { id: 2 },
        ClientMsg::SetProjectGroup { project_id: 1, group: Some(2) },
        ClientMsg::GetDaemonInfo,
        ClientMsg::SaveScrollback,
        ClientMsg::RestoreTerminal {
            request_id: 7, cwd: None, cols: 80, rows: 24, worktree: Some("/test".into()),
        },
        ClientMsg::SetProjectGroup { project_id: 1, group: None },
    ];
    for m in msgs {
        let bytes = encode(&m).unwrap();
        assert_eq!(decode::<ClientMsg>(&bytes).unwrap(), m);
    }
}

#[test]
fn server_msgs_round_trip() {
    let msgs = vec![
        ServerMsg::Welcome { proto_version: PROTO_VERSION, daemon_pid: 1234 },
        ServerMsg::Terminals(vec![info(), info()]),
        ServerMsg::Terminals(vec![]),
        ServerMsg::TerminalCreated(info()),
        ServerMsg::TerminalExited { id: 1, code: Some(0) },
        ServerMsg::TerminalExited { id: 1, code: None },
        ServerMsg::TerminalRemoved { id: 9 },
        ServerMsg::Output { id: 1, seq: u64::MAX, bytes: b"hello".to_vec() },
        ServerMsg::Projects {
            projects: vec![project()],
            groups: vec![GroupInfo { id: 7, name: "game".into() }],
        },
        ServerMsg::Projects { projects: vec![], groups: vec![] },
        ServerMsg::OpFailed { op: "create_worktree".into(), message: "exists".into() },
        ServerMsg::AgentStatusChanged { id: 2, status: Some(status()) },
        ServerMsg::AgentStatusChanged { id: 2, status: None },
        ServerMsg::TerminalTitle { id: 2, title: "Claude Code".into() },
        ServerMsg::Error { message: "boom".into() },
        ServerMsg::DaemonInfo { build_id: "0.0.1-abc-123".into(), proto_version: PROTO_VERSION, started_unix_ms: 100 },
        ServerMsg::ScrollbackSaved { files: vec![(7, "7-100.bin".into())] },
        ServerMsg::ScrollbackSaved { files: Vec::new() },
        ServerMsg::TerminalRestored { request_id: 7, terminal: Some(info()) },
        ServerMsg::TerminalRestored { request_id: 8, terminal: None },
    ];
    for m in msgs {
        let bytes = encode(&m).unwrap();
        assert_eq!(decode::<ServerMsg>(&bytes).unwrap(), m);
    }
}

#[test]
fn agent_enums_round_trip() {
    for (tag, kind) in [AgentKind::Claude, AgentKind::Gemini, AgentKind::Codex, AgentKind::Other, AgentKind::Omp]
        .into_iter().enumerate()
    {
        assert_eq!(encode(&kind).unwrap().as_ref(), &[tag as u8]);
        let st = AgentStatus { kind, ..status() };
        assert_eq!(decode::<AgentStatus>(&encode(&st).unwrap()).unwrap(), st);
    }
    for state in [
        AgentState::Working,
        AgentState::Waiting,
        AgentState::Done,
        AgentState::Failed,
        AgentState::Idle,
    ] {
        let st = AgentStatus { state, detail: None, ..status() };
        assert_eq!(decode::<AgentStatus>(&encode(&st).unwrap()).unwrap(), st);
    }
}

#[test]
fn proto_version_is_7() {
    assert_eq!(PROTO_VERSION, 7);
}

#[test]
fn agent_status_optional_fields_round_trip() {
    let empty = AgentStatus { detail: None, model: None, title: None, prompt: None, message: None, ..status() };
    assert_eq!(decode::<AgentStatus>(&encode(&empty).unwrap()).unwrap(), empty);
    let only_model = AgentStatus { model: Some("m".into()), ..empty.clone() };
    assert_eq!(decode::<AgentStatus>(&encode(&only_model).unwrap()).unwrap(), only_model);
    let unicode = AgentStatus { prompt: Some("привіт ✳".into()), message: Some("…".into()), ..empty };
    assert_eq!(decode::<AgentStatus>(&encode(&unicode).unwrap()).unwrap(), unicode);
}

#[test]
fn garbage_fails_to_decode() {
    assert!(decode::<ServerMsg>(&[0xff, 0xff, 0xff]).is_err());
}

/// Handshake compatibility rule: `Hello` / `Welcome` stay the first variants with unchanged
/// fields, so every protocol version can decode them.
#[test]
fn hello_and_welcome_stay_first_variants() {
    let hello = encode(&ClientMsg::Hello { proto_version: 1, client_name: "x".into() }).unwrap();
    // postcard: variant index 0, then the u32 version as a varint, then the string.
    assert_eq!(&hello[..], &[0u8, 1, 1, b'x'][..]);
    let welcome = encode(&ServerMsg::Welcome { proto_version: 2, daemon_pid: 300 }).unwrap();
    assert_eq!(&welcome[..], &[0u8, 2, 0xAC, 0x02][..]);
}

#[test]
fn agent_status_with_and_without_subagents_round_trips() {
    let st = status();
    assert_eq!(st.subagents.len(), 2);
    assert_eq!(decode::<AgentStatus>(&encode(&st).unwrap()).unwrap(), st);
    let none = AgentStatus { subagents: Vec::new(), ..status() };
    assert_eq!(decode::<AgentStatus>(&encode(&none).unwrap()).unwrap(), none);
}
