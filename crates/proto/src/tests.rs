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
        },
        ClientMsg::CreateTerminal { cwd: None, shell: None, args: vec![], cols: 1, rows: 1 },
        ClientMsg::Subscribe { id: 1, resume_from_seq: Some(42) },
        ClientMsg::Subscribe { id: 1, resume_from_seq: None },
        ClientMsg::Unsubscribe { id: 2 },
        ClientMsg::Input { id: 3, bytes: vec![0, 1, 255] },
        ClientMsg::Resize { id: 4, cols: 100, rows: 50 },
        ClientMsg::KillTerminal { id: 5 },
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
