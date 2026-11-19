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
        ClientMsg::AddProject { path: PathBuf::from("/repo") },
        ClientMsg::RemoveProject { id: 3 },
        ClientMsg::CreateWorktree { project_id: 1, name: "feat".into(), base: Some("main".into()) },
        ClientMsg::CreateWorktree { project_id: 1, name: "feat".into(), base: None },
        ClientMsg::RemoveWorktree {
            project_id: 1,
            path: PathBuf::from("/wt/feat"),
            force: true,
            delete_branch: false,
        },
        ClientMsg::Shutdown,
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
        ServerMsg::Projects(vec![project()]),
        ServerMsg::Projects(vec![]),
        ServerMsg::OpFailed { op: "create_worktree".into(), message: "exists".into() },
        ServerMsg::Error { message: "boom".into() },
    ];
    for m in msgs {
        let bytes = encode(&m).unwrap();
        assert_eq!(decode::<ServerMsg>(&bytes).unwrap(), m);
    }
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
