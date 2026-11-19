//! Runs the real `sich hook claude` binary with piped stdin against an in-process daemon.
//! (The release build uses the `windows` subsystem; std treats a missing console handle as empty
//! input and inherited pipes work, so the debug binary exercises the same read path.)

use std::io::Write as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::{SinkExt as _, StreamExt as _};
use sich_proto::{
    AgentKind, AgentState, ClientMsg, PROTO_VERSION, ServerMsg, decode, encode, frame_codec,
};
use tokio::time::timeout;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

type Conn = Framed<sich_daemon::ClientStream, LengthDelimitedCodec>;

const BIN: &str = env!("CARGO_BIN_EXE_sich");

fn run_hook_bin(env: &[(&str, String)], stdin: &str) -> (Option<i32>, Vec<u8>, Duration) {
    let mut cmd = Command::new(BIN);
    cmd.args(["hook", "claude"])
        .env_remove("SICH_TERM_ID")
        .env_remove("SICH_SOCKET")
        // Keep the hook log out of the user's real data dir.
        .env("SICH_LOG_DIR", std::env::temp_dir().join("sich-test-hook-logs"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let started = Instant::now();
    let mut child = cmd.spawn().expect("spawn sich");
    {
        let mut si = child.stdin.take().unwrap();
        let _ = si.write_all(stdin.as_bytes());
    }
    let out = child.wait_with_output().unwrap();
    (out.status.code(), out.stdout, started.elapsed())
}

async fn recv(c: &mut Conn) -> ServerMsg {
    let f = timeout(Duration::from_secs(20), c.next()).await.expect("timeout").expect("closed").unwrap();
    decode(&f).unwrap()
}

#[test]
fn without_term_id_exits_zero_silently() {
    let (code, stdout, _) = run_hook_bin(&[], r#"{"hook_event_name":"Stop"}"#);
    assert_eq!(code, Some(0));
    assert!(stdout.is_empty());
}

#[test]
fn unreachable_daemon_exits_zero_silently_and_fast() {
    let name = format!("sich-hook-cli-none-{}", std::process::id());
    let (code, stdout, took) = run_hook_bin(
        &[("SICH_TERM_ID", "1".into()), ("SICH_SOCKET", name)],
        r#"{"hook_event_name":"Stop"}"#,
    );
    assert_eq!(code, Some(0));
    assert!(stdout.is_empty());
    assert!(took < Duration::from_secs(5), "{took:?}");
}

#[test]
fn garbage_term_id_and_stdin_still_exit_zero() {
    let (code, stdout, _) = run_hook_bin(&[("SICH_TERM_ID", "nope".into())], "\u{0}\u{1}garbage");
    assert_eq!(code, Some(0));
    assert!(stdout.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn piped_stdin_reaches_the_daemon() {
    let base = std::env::temp_dir().join(format!("sich-hookcli-test-{}", std::process::id()));
    std::fs::create_dir_all(&base).unwrap();
    // SAFETY: the only test in this binary that starts a daemon; set before it starts.
    unsafe {
        std::env::set_var("SICH_CONFIG_DIR", base.join("cfg"));
        std::env::set_var("SICH_WORKTREE_ROOT", base.join("wt"));
    }
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let name = format!("sich-test-hookcli-{}-{stamp}", std::process::id());
    let daemon = tokio::spawn({
        let name = name.clone();
        async move { sich_daemon::run(&name).await }
    });

    let mut c = None;
    for _ in 0..100 {
        if let Ok(s) = sich_daemon::connect(&name).await {
            c = Some(Framed::new(s, frame_codec()));
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let mut c = c.expect("connect");
    c.send(encode(&ClientMsg::Hello { proto_version: PROTO_VERSION, client_name: "t".into() }).unwrap())
        .await
        .unwrap();
    assert!(matches!(recv(&mut c).await, ServerMsg::Welcome { .. }));

    let shell = if cfg!(windows) { "cmd.exe" } else { "/bin/sh" };
    c.send(
        encode(&ClientMsg::CreateTerminal {
            cwd: None,
            shell: Some(shell.into()),
            args: vec![],
            cols: 80,
            rows: 24,
            worktree: None,
        })
        .unwrap(),
    )
    .await
    .unwrap();
    let id = loop {
        if let ServerMsg::TerminalCreated(info) = recv(&mut c).await {
            break info.id;
        }
    };

    let sock = name.clone();
    let (code, stdout, _) = tokio::task::spawn_blocking(move || {
        run_hook_bin(
            &[("SICH_TERM_ID", id.to_string()), ("SICH_SOCKET", sock)],
            r#"{"session_id":"s","hook_event_name":"PermissionRequest","tool_name":"Bash"}"#,
        )
    })
    .await
    .unwrap();
    assert_eq!(code, Some(0));
    assert!(stdout.is_empty(), "the hook must never print to stdout");

    let status = loop {
        if let ServerMsg::AgentStatusChanged { id: sid, status } = recv(&mut c).await {
            if sid == id {
                break status.expect("status");
            }
        }
    };
    assert_eq!((status.kind, status.state), (AgentKind::Claude, AgentState::Waiting));
    assert_eq!(status.detail.as_deref(), Some("Bash"));

    c.send(encode(&ClientMsg::Shutdown).unwrap()).await.unwrap();
    timeout(Duration::from_secs(10), daemon).await.expect("daemon did not stop").unwrap().unwrap();
}
