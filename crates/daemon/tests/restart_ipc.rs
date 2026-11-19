use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::{SinkExt as _, StreamExt as _};
use sich_proto::{ClientMsg, PROTO_VERSION, ServerMsg, TermId, decode, encode, frame_codec};
use tokio::time::{sleep, timeout};
use tokio_util::codec::{Framed, LengthDelimitedCodec};

type Conn = Framed<sich_daemon::ClientStream, LengthDelimitedCodec>;

async fn connect(socket: &str) -> Conn {
    timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(stream) = sich_daemon::connect(socket).await {
                return Framed::new(stream, frame_codec());
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("connect to test daemon")
}

async fn send(conn: &mut Conn, message: ClientMsg) {
    conn.send(encode(&message).unwrap()).await.unwrap();
}

async fn recv(conn: &mut Conn) -> ServerMsg {
    let frame = timeout(Duration::from_secs(5), conn.next())
        .await
        .expect("timely reply")
        .expect("connection open")
        .expect("read reply");
    decode(&frame).unwrap()
}

async fn client(socket: &str) -> Conn {
    let mut conn = connect(socket).await;
    send(
        &mut conn,
        ClientMsg::Hello {
            proto_version: PROTO_VERSION,
            client_name: "restart test".into(),
        },
    )
    .await;
    assert!(matches!(recv(&mut conn).await, ServerMsg::Welcome { .. }));
    conn
}

async fn collect_marker(conn: &mut Conn, id: TermId, marker: &[u8]) {
    let mut output = Vec::new();
    loop {
        if let ServerMsg::Output {
            id: terminal,
            bytes,
            ..
        } = recv(conn).await
        {
            if terminal != id {
                continue;
            }
            if bytes.windows(4).any(|bytes| bytes == b"\x1b[6n") {
                send(
                    conn,
                    ClientMsg::Input {
                        id,
                        bytes: b"\x1b[1;1R".to_vec(),
                    },
                )
                .await;
            }
            output.extend(bytes);
            if output.windows(marker.len()).any(|bytes| bytes == marker) {
                return;
            }
        }
    }
}

#[test]
fn reports_build_restores_by_request_id_and_saves_the_final_live_tail() {
    let dir = tempfile::tempdir().unwrap();
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let socket = format!("sich-restart-test-{}-{stamp}", std::process::id());
    // SAFETY: this is the only test in this executable, set before starting any daemon tasks.
    unsafe {
        std::env::set_var("SICH_SOCKET", &socket);
        std::env::set_var("SICH_CONFIG_DIR", dir.path().join("config"));
        std::env::set_var("SICH_DATA_DIR", dir.path().join("data"));
        std::env::set_var("SICH_WORKTREE_ROOT", dir.path().join("worktrees"));
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let daemon = tokio::spawn({
            let socket = sich_daemon::socket_name_from_env();
            async move { sich_daemon::run_with_build(&socket, "build-under-test").await }
        });
        let mut conn = client(&socket).await;
        let mut other = client(&socket).await;
        send(&mut conn, ClientMsg::GetDaemonInfo).await;
        let ServerMsg::DaemonInfo { build_id, proto_version, started_unix_ms } = recv(&mut conn).await
            else { panic!("expected daemon info") };
        assert_eq!(build_id, "build-under-test");
        assert_eq!(proto_version, PROTO_VERSION);
        assert!(started_unix_ms > 0);

        send(&mut conn, ClientMsg::RestoreTerminal {
            request_id: 413, cwd: Some(dir.path().into()), cols: 80, rows: 24, worktree: None,
        }).await;
        let ServerMsg::TerminalRestored { request_id: 413, terminal: Some(restored) } = recv(&mut conn).await
            else { panic!("expected the requester's matching acknowledgment") };
        assert!(matches!(recv(&mut conn).await, ServerMsg::TerminalCreated(info) if info.id == restored.id));
        assert!(matches!(recv(&mut other).await, ServerMsg::TerminalCreated(info) if info.id == restored.id));
        send(&mut conn, ClientMsg::KillTerminal { id: restored.id }).await;

        let (shell, args): (&str, &[&str]) = if cfg!(windows) {
            ("cmd.exe", &["/d", "/k", "echo SICH_BEFORE_SHUTDOWN"])
        } else {
            ("/bin/sh", &["-i", "-c", "echo SICH_BEFORE_SHUTDOWN; read line; echo $line; read end"])
        };
        send(&mut conn, ClientMsg::CreateTerminal {
            cwd: Some(dir.path().into()), shell: Some(shell.into()),
            args: args.iter().map(|arg| (*arg).into()).collect(), cols: 80, rows: 24, worktree: None,
        }).await;
        let id = loop {
            if let ServerMsg::TerminalCreated(info) = recv(&mut conn).await {
                break info.id;
            }
        };
        send(&mut conn, ClientMsg::Subscribe { id, resume_from_seq: None }).await;
        collect_marker(&mut conn, id, b"SICH_BEFORE_SHUTDOWN").await;

        send(&mut conn, ClientMsg::SaveScrollback).await;
        let files = loop {
            if let ServerMsg::ScrollbackSaved { files } = recv(&mut conn).await {
                break files;
            }
        };
        assert_eq!(files, vec![(id, format!("{id}-{started_unix_ms}.bin"))]);
        let snapshots = sich_daemon::scrollback::directory().unwrap();
        let first = sich_daemon::scrollback::load(&snapshots, &files[0].1).unwrap();
        assert!(String::from_utf8_lossy(&first).contains("SICH_BEFORE_SHUTDOWN"));
        assert!(!String::from_utf8_lossy(&first).contains("SICH_AFTER_SAVE"));

        let input = if cfg!(windows) { b"echo SICH_AFTER_SAVE\r".as_slice() } else { b"SICH_AFTER_SAVE\n" };
        send(&mut conn, ClientMsg::Input { id, bytes: input.to_vec() }).await;
        collect_marker(&mut conn, id, b"SICH_AFTER_SAVE").await;
        send(&mut connect(&socket).await, ClientMsg::Shutdown).await;
        timeout(Duration::from_secs(10), daemon).await.unwrap().unwrap().unwrap();
        let final_tail = sich_daemon::scrollback::load(&snapshots, &files[0].1).unwrap();
        assert!(String::from_utf8_lossy(&final_tail).contains("SICH_AFTER_SAVE"));
    });
}
