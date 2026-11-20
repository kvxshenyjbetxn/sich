//! With the bundled ConPTY, an APC sequence (Kitty graphics) written by a child process must
//! reach the PTY reader; the system ConPTY drops it. Own test binary: the ConPTY library is
//! loaded once per process, so the files must be in place before the first terminal starts.
#![cfg(windows)]

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
    let f = timeout(Duration::from_secs(20), c.next()).await.expect("timeout").expect("closed").unwrap();
    decode(&f).unwrap()
}

/// Places the bundled ConPTY next to the test executable (found by plain-name DLL lookup).
fn sideload_conpty() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../third_party/conpty/x64");
    let dir = std::env::current_exe().unwrap().parent().unwrap().to_path_buf();
    for name in ["conpty.dll", "OpenConsole.exe"] {
        let to = dir.join(name);
        if !to.exists() {
            std::fs::copy(src.join(name), &to).unwrap();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kitty_apc_reaches_the_reader() {
    sideload_conpty();
    let dir = tempfile::tempdir().unwrap();
    // SAFETY: the only test in this binary; set before the daemon starts.
    unsafe {
        std::env::set_var("SICH_CONFIG_DIR", dir.path().join("config"));
        std::env::set_var("SICH_DATA_DIR", dir.path().join("data"));
        std::env::set_var("SICH_WORKTREE_ROOT", dir.path().join("worktrees"));
    }
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let name = format!("sich-test-apc-{}-{stamp}", std::process::id());
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

    let script = "[Console]::Out.Write([char]27+'_Ga=q,i=1;AAAA'+[char]27+'\'); Start-Sleep 2";
    send(
        &mut c,
        ClientMsg::CreateTerminal {
            cwd: None,
            shell: Some("pwsh".into()),
            args: vec!["-NoProfile".into(), "-Command".into(), script.into()],
            cols: 200,
            rows: 50,
            worktree: None,
        },
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
                if bytes.windows(3).any(|w| w == b"[6n") {
                    send(&mut c, ClientMsg::Input { id, bytes: b"\x1b[1;1R".to_vec() }).await;
                }
                out.extend_from_slice(&bytes);
            }
            ServerMsg::TerminalExited { id: eid, .. } if eid == id => break,
            _ => {}
        }
    }
    assert!(
        out.windows(3).any(|w| w == b"\x1b_G"),
        "APC dropped: {:?}",
        String::from_utf8_lossy(&out)
    );

    send(&mut c, ClientMsg::Shutdown).await;
    timeout(Duration::from_secs(10), daemon).await.expect("daemon did not stop").unwrap().unwrap();
}
