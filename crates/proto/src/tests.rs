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
        ClientMsg::MarkSeen { id: 8 },
        ClientMsg::Shutdown,
        ClientMsg::CreateGroup { name: "game".into() },
        ClientMsg::RenameGroup { id: 2, name: "игры".into() },
        ClientMsg::RemoveGroup { id: 2 },
        ClientMsg::SetProjectGroup { project_id: 1, group: Some(2) },
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
    ];
    for m in msgs {
        let bytes = encode(&m).unwrap();
        assert_eq!(decode::<ServerMsg>(&bytes).unwrap(), m);
    }
}

#[test]
fn agent_enums_round_trip() {
    for kind in [AgentKind::Claude, AgentKind::Gemini, AgentKind::Codex, AgentKind::Other] {
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
fn proto_version_is_4() {
    assert_eq!(PROTO_VERSION, 4);
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
