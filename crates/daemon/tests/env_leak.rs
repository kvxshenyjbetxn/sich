//! Terminals must not inherit launcher-specific variables. Own test binary because it mutates
//! the process environment before starting the daemon.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::{SinkExt as _, StreamExt as _};
use sich_proto::{ClientMsg, PROTO_VERSION, ServerMsg, decode, encode, frame_codec};
use tokio::time::timeout;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

type Conn = Framed<sich_daemon::ClientStream, LengthDelimitedCodec>;

async fn send(c: &mut Conn, m: ClientMsg) {
    c.send(encode(&m).unwrap()).await.unwrap();
}

async fn recv(c: &mut Conn) -> ServerMsg {
    let f = timeout(Duration::from_secs(15), c.next()).await.expect("timeout").expect("closed").unwrap();
    decode(&f).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_env_is_clean() {
    let dir = tempfile::tempdir().unwrap();
    // SAFETY: the only test in this binary; set before the daemon starts.
    unsafe {
        std::env::set_var("SICH_CONFIG_DIR", dir.path().join("config"));
        std::env::set_var("SICH_DATA_DIR", dir.path().join("data"));
        std::env::set_var("SICH_WORKTREE_ROOT", dir.path().join("worktrees"));
        std::env::set_var("CLAUDECODE", "1");
        std::env::set_var("CLAUDE_CODE_CHILD_SESSION", "1");
    }
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let name = format!("sich-test-env-{}-{stamp}", std::process::id());
    let daemon = tokio::spawn({
        let name = name.clone();
        async move { sich_daemon::run(&name).await }
    });

    let mut conn = None;
    for _ in 0..100 {
        if let Ok(s) = sich_daemon::connect(&name).await {
            conn = Some(Framed::new(s, frame_codec()));
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let mut c = conn.expect("could not connect to daemon");

    send(&mut c, ClientMsg::Hello { proto_version: PROTO_VERSION, client_name: "test".into() }).await;
    assert!(matches!(recv(&mut c).await, ServerMsg::Welcome { .. }));

    let (shell, args): (String, Vec<String>) = if cfg!(windows) {
        ("cmd.exe".into(), vec!["/c".into(), "set".into()])
    } else {
        ("/usr/bin/env".into(), vec![])
    };
    send(
        &mut c,
        ClientMsg::CreateTerminal { cwd: None, shell: Some(shell), args, cols: 200, rows: 50, worktree: None },
    )
    .await;
    let id = loop {
        if let ServerMsg::TerminalCreated(info) = recv(&mut c).await {
            break info.id;
        }
    };
    send(&mut c, ClientMsg::Subscribe { id, resume_from_seq: None }).await;
    let mut out = Vec::new();
    loop {
        match recv(&mut c).await {
            ServerMsg::Output { id: oid, bytes, .. } if oid == id => {
                // ConPTY waits for a cursor position answer at startup.
                if bytes.windows(3).any(|w| w == b"[6n") {
                    send(&mut c, ClientMsg::Input { id, bytes: b"\x1b[1;1R".to_vec() }).await;
                }
                out.extend_from_slice(&bytes);
            }
            ServerMsg::TerminalExited { id: eid, .. } if eid == id => break,
            _ => {}
        }
    }
    let text = String::from_utf8_lossy(&out);
    assert!(text.contains("TERM_PROGRAM=sich"), "{text}");
    assert!(text.contains("COLORTERM=truecolor"), "{text}");
    assert!(!text.contains("CLAUDECODE="), "{text}");
    assert!(!text.contains("CLAUDE_CODE_CHILD_SESSION="), "{text}");

    send(&mut c, ClientMsg::Shutdown).await;
    timeout(Duration::from_secs(10), daemon).await.expect("daemon did not stop").unwrap().unwrap();
}
