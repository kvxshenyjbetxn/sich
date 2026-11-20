//! Two daemon generations side by side on one base endpoint, over real IPC.
//! Hermetic: unique base socket, temp data/config/worktree dirs. Single test in this binary
//! because it sets process-wide env overrides.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::{SinkExt as _, StreamExt as _};
use sich_daemon::generation;
use sich_proto::{ClientMsg, PROTO_VERSION, ServerMsg, TermId, decode, encode, frame_codec};
use tokio::time::{sleep, timeout};
use tokio_util::codec::{Framed, LengthDelimitedCodec};

type Conn = Framed<sich_daemon::ClientStream, LengthDelimitedCodec>;

async fn client(socket: &str, name: &str) -> Conn {
    let mut conn = timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(stream) = sich_daemon::connect(socket).await {
                return Framed::new(stream, frame_codec());
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("connect to test daemon");
    conn.send(encode(&ClientMsg::Hello { proto_version: PROTO_VERSION, client_name: name.into() }).unwrap())
        .await
        .unwrap();
    assert!(matches!(recv(&mut conn).await, ServerMsg::Welcome { .. }));
    conn
}

async fn send(conn: &mut Conn, msg: ClientMsg) {
    conn.send(encode(&msg).unwrap()).await.unwrap();
}

async fn recv(conn: &mut Conn) -> ServerMsg {
    let frame = timeout(Duration::from_secs(10), conn.next())
        .await
        .expect("timely reply")
        .expect("connection open")
        .expect("read reply");
    decode(&frame).unwrap()
}

async fn create(conn: &mut Conn) -> TermId {
    let (shell, args): (&str, &[&str]) =
        if cfg!(windows) { ("cmd.exe", &["/d", "/k"]) } else { ("/bin/sh", &["-i"]) };
    send(conn, ClientMsg::CreateTerminal {
        cwd: None, shell: Some(shell.into()), args: args.iter().map(|a| (*a).into()).collect(),
        cols: 80, rows: 24, worktree: None,
    })
    .await;
    loop {
        if let ServerMsg::TerminalCreated(info) = recv(conn).await {
            return info.id;
        }
    }
}

async fn terminal_count(conn: &mut Conn) -> usize {
    send(conn, ClientMsg::ListTerminals).await;
    loop {
        if let ServerMsg::Terminals(list) = recv(conn).await {
            return list.len();
        }
    }
}

async fn wait_until_gone(socket: &str) {
    timeout(Duration::from_secs(10), async {
        while sich_daemon::connect(socket).await.is_ok() {
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("daemon exits");
}

#[test]
fn generations_coexist_and_the_old_one_retires_after_its_last_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let base = format!("sich-gen-test-{}-{stamp}", std::process::id());
    // SAFETY: the only test in this executable, set before any daemon task starts.
    unsafe {
        std::env::set_var("SICH_CONFIG_DIR", dir.path().join("config"));
        std::env::set_var("SICH_DATA_DIR", dir.path().join("data"));
        std::env::set_var("SICH_WORKTREE_ROOT", dir.path().join("worktrees"));
    }
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| rt.block_on(async {
        let old_socket = generation::socket(&base, "oldgen");
        let new_socket = generation::socket(&base, "newgen");
        let old = tokio::spawn({
            let base = base.clone();
            async move { sich_daemon::run_generation(&base, "oldgen").await }
        });
        let mut old_conn = client(&old_socket, "test").await;
        sleep(Duration::from_millis(20)).await;
        let new = tokio::spawn({
            let base = base.clone();
            async move { sich_daemon::run_generation(&base, "newgen").await }
        });
        let mut new_conn = client(&new_socket, "test").await;

        // Registry: both advertised, the newer one is preferred, nothing else is listening on the base.
        let entries = generation::entries();
        assert_eq!(entries.len(), 2, "{entries:?}");
        let newest = generation::newest_compatible(&base, &entries).expect("newest generation");
        assert_eq!(newest.socket, new_socket);
        assert!(sich_daemon::connect(&base).await.is_err(), "legacy name must stay unused");

        // Terminal ids collide across daemons; each daemon only knows its own terminals.
        let old_term = create(&mut old_conn).await;
        let new_term = create(&mut new_conn).await;
        assert_eq!(old_term, new_term, "ids restart per daemon");
        assert_eq!(terminal_count(&mut new_conn).await, 1);

        // A retiring client never stops a daemon that still has terminals.
        let omp_retire = format!("{}{}", generation::RETIRE_CLIENT, sich_proto::OMP_CLIENT_SUFFIX);
        let mut retire = client(&old_socket, &omp_retire).await;
        send(&mut retire, ClientMsg::Shutdown).await;
        sleep(Duration::from_millis(300)).await;
        assert_eq!(terminal_count(&mut old_conn).await, 1);

        // Killing the terminal on the new daemon does not touch the old one.
        send(&mut new_conn, ClientMsg::KillTerminal { id: new_term }).await;
        sleep(Duration::from_millis(300)).await;
        assert_eq!(terminal_count(&mut old_conn).await, 1);

        // Last terminal of the old generation closes, then the retire request takes effect.
        send(&mut old_conn, ClientMsg::KillTerminal { id: old_term }).await;
        timeout(Duration::from_secs(10), async {
            while terminal_count(&mut old_conn).await != 0 {
                sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("terminal removed");
        let mut retire = client(&old_socket, generation::RETIRE_CLIENT).await;
        send(&mut retire, ClientMsg::Shutdown).await;
        timeout(Duration::from_secs(10), old).await.expect("old daemon task ends").unwrap().unwrap();
        // A pipe stays connectable on Windows while any client of the old daemon is still open.
        drop((old_conn, retire));
        wait_until_gone(&old_socket).await;

        // The new generation keeps running and the old registry entry is gone.
        let entries = generation::entries();
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0].socket, new_socket);
        assert_eq!(terminal_count(&mut new_conn).await, 0);

        send(&mut new_conn, ClientMsg::Shutdown).await;
        timeout(Duration::from_secs(5), new).await.expect("new daemon task ends").unwrap().unwrap();
        assert!(generation::entries().is_empty());
    })));
    // Do not wait for PTY reader threads of terminals a failing assertion left behind.
    rt.shutdown_background();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
