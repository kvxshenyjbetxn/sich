use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::{SinkExt as _, StreamExt as _};
use sich_proto::{ClientMsg, PROTO_VERSION, ServerMsg, TermId, decode, encode, frame_codec};
use tokio::time::timeout;
use tokio_util::codec::Framed;

const MARKER: &[u8] = b"SICH_MARKER";

type Conn = Framed<interprocess::local_socket::tokio::Stream, tokio_util::codec::LengthDelimitedCodec>;

async fn connect(name: &str) -> Conn {
    // The daemon task may not have bound the socket yet.
    for _ in 0..100 {
        if let Ok(s) = sich_daemon::connect(name).await {
            return Framed::new(s, frame_codec());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("could not connect to daemon");
}

async fn send(c: &mut Conn, msg: ClientMsg) {
    c.send(encode(&msg).unwrap()).await.unwrap();
}

async fn recv(c: &mut Conn) -> ServerMsg {
    let frame = timeout(Duration::from_secs(10), c.next())
        .await
        .expect("timed out waiting for a server message")
        .expect("connection closed")
        .expect("read frame");
    decode(&frame).unwrap()
}

async fn handshake(c: &mut Conn) {
    send(c, ClientMsg::Hello { proto_version: PROTO_VERSION, client_name: "test".into() }).await;
    match recv(c).await {
        ServerMsg::Welcome { proto_version, daemon_pid } => {
            assert_eq!(proto_version, PROTO_VERSION);
            assert_eq!(daemon_pid, std::process::id());
        }
        other => panic!("expected Welcome, got {other:?}"),
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// Subscribes (from the start) and collects output until the marker was seen and the
/// terminal reported as exited (or, for a late subscriber, until the marker is seen).
async fn collect(c: &mut Conn, id: TermId, need_exit: bool, answer_dsr: bool) -> Vec<u8> {
    send(c, ClientMsg::Subscribe { id, resume_from_seq: None }).await;
    let mut out = Vec::new();
    let mut expected_seq = 0u64;
    let mut exited = false;
    while !(contains(&out, MARKER) && (exited || !need_exit)) {
        let m = recv(c).await;
        match m {
            ServerMsg::Output { id: oid, seq, bytes } if oid == id => {
                assert_eq!(seq, expected_seq, "gap or duplicate in output stream");
                expected_seq += bytes.len() as u64;
                out.extend_from_slice(&bytes);
                // ConPTY asks for the cursor position at startup and waits for the answer;
                // a real terminal emulator replies, so do the same here.
                if answer_dsr && contains(&bytes, b"[6n") {
                    send(c, ClientMsg::Input { id, bytes: b"[1;1R".to_vec() }).await;
                }
            }
            ServerMsg::TerminalExited { id: eid, .. } if eid == id => exited = true,
            _ => {}
        }
    }
    out
}

fn shell_command() -> (String, Vec<String>) {
    if cfg!(windows) {
        ("cmd.exe".into(), vec!["/c".into(), "echo SICH_MARKER".into()])
    } else {
        ("/bin/sh".into(), vec!["-c".into(), "echo SICH_MARKER".into()])
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_subscribe_replay_shutdown() {
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let name = format!("sich-test-{}-{stamp}", std::process::id());
    let daemon = tokio::spawn({
        let name = name.clone();
        async move { sich_daemon::run(&name).await }
    });

    let mut a = connect(&name).await;
    handshake(&mut a).await;

    let (shell, args) = shell_command();
    send(
        &mut a,
        ClientMsg::CreateTerminal { cwd: None, shell: Some(shell), args, cols: 80, rows: 24 },
    )
    .await;
    let id = loop {
        if let ServerMsg::TerminalCreated(info) = recv(&mut a).await {
            assert_eq!((info.cols, info.rows), (80, 24));
            break info.id;
        }
    };

    let out_a = collect(&mut a, id, true, true).await;
    assert!(contains(&out_a, MARKER));

    // Second connection: lists the (exited) terminal and gets the same bytes replayed.
    let mut b = connect(&name).await;
    handshake(&mut b).await;
    send(&mut b, ClientMsg::ListTerminals).await;
    match recv(&mut b).await {
        ServerMsg::Terminals(list) => {
            assert_eq!(list.len(), 1);
            assert_eq!(list[0].id, id);
        }
        other => panic!("expected Terminals, got {other:?}"),
    }
    let out_b = collect(&mut b, id, false, false).await;
    assert!(out_b.starts_with(&out_a[..out_a.len().min(out_b.len())]));
    assert!(contains(&out_b, MARKER));

    // Kill removes the terminal and tells everyone.
    send(&mut b, ClientMsg::KillTerminal { id }).await;
    loop {
        if let ServerMsg::TerminalRemoved { id: rid } = recv(&mut a).await {
            assert_eq!(rid, id);
            break;
        }
    }

    send(&mut b, ClientMsg::Shutdown).await;
    timeout(Duration::from_secs(10), daemon)
        .await
        .expect("daemon did not stop")
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn version_mismatch_is_rejected() {
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let name = format!("sich-test-ver-{}-{stamp}", std::process::id());
    let daemon = tokio::spawn({
        let name = name.clone();
        async move { sich_daemon::run(&name).await }
    });
    let mut c = connect(&name).await;
    send(&mut c, ClientMsg::Hello { proto_version: PROTO_VERSION + 1, client_name: "x".into() })
        .await;
    assert!(matches!(recv(&mut c).await, ServerMsg::Error { .. }));
    let mut d = connect(&name).await;
    handshake(&mut d).await;
    send(&mut d, ClientMsg::Shutdown).await;
    let _ = timeout(Duration::from_secs(10), daemon).await;
}
