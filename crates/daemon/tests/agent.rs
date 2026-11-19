//! Agent status and title tracking over real IPC (in-process daemon, isolated socket/config).

use std::sync::Once;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::{SinkExt as _, StreamExt as _};
use sich_proto::{
    AgentKind, AgentState, AgentStatus, ClientMsg, PROTO_VERSION, ServerMsg, TermId, decode,
    encode, frame_codec,
};
use tokio::time::timeout;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

type Conn = Framed<sich_daemon::ClientStream, LengthDelimitedCodec>;

static INIT: Once = Once::new();

fn isolate_config() {
    INIT.call_once(|| {
        let base = std::env::temp_dir().join(format!("sich-agent-test-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        // SAFETY: set once, before any daemon of this test binary starts.
        unsafe {
            std::env::set_var("SICH_CONFIG_DIR", base.join("cfg"));
            std::env::set_var("SICH_WORKTREE_ROOT", base.join("wt"));
        }
    });
}

struct Daemon {
    name: String,
    handle: tokio::task::JoinHandle<anyhow::Result<()>>,
}

fn start_daemon(tag: &str) -> Daemon {
    isolate_config();
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let name = format!("sich-test-{tag}-{}-{stamp}", std::process::id());
    let handle = tokio::spawn({
        let name = name.clone();
        async move { sich_daemon::run(&name).await }
    });
    Daemon { name, handle }
}

async fn connect(name: &str) -> Conn {
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
    let frame = timeout(Duration::from_secs(20), c.next())
        .await
        .expect("timed out waiting for a server message")
        .expect("connection closed")
        .expect("read frame");
    decode(&frame).unwrap()
}

async fn client(name: &str) -> Conn {
    let mut c = connect(name).await;
    send(&mut c, ClientMsg::Hello { proto_version: PROTO_VERSION, client_name: "test".into() }).await;
    assert!(matches!(recv(&mut c).await, ServerMsg::Welcome { .. }));
    c
}

async fn stop(mut c: Conn, d: Daemon) {
    send(&mut c, ClientMsg::Shutdown).await;
    timeout(Duration::from_secs(10), d.handle)
        .await
        .expect("daemon did not stop")
        .unwrap()
        .unwrap();
}

async fn create(c: &mut Conn, shell: &str, args: &[&str]) -> TermId {
    send(
        c,
        ClientMsg::CreateTerminal {
            cwd: None,
            shell: Some(shell.into()),
            args: args.iter().map(|s| s.to_string()).collect(),
            cols: 120,
            rows: 30,
            worktree: None,
        },
    )
    .await;
    loop {
        if let ServerMsg::TerminalCreated(info) = recv(c).await {
            return info.id;
        }
    }
}

/// A shell that stays alive until it is told to exit.
fn idle_shell() -> (&'static str, Vec<&'static str>) {
    if cfg!(windows) { ("cmd.exe", vec![]) } else { ("/bin/sh", vec![]) }
}

fn hook(id: TermId, json: &str) -> ClientMsg {
    ClientMsg::AgentHook { term_id: id, agent: AgentKind::Claude, payload: json.into() }
}

fn event(name: &str, extra: &str) -> String {
    format!(r#"{{"session_id":"s1","cwd":"C:\\dev","hook_event_name":"{name}"{extra}}}"#)
}

/// Waits for the next `AgentStatusChanged` of `id`.
async fn next_status(c: &mut Conn, id: TermId) -> Option<AgentStatus> {
    loop {
        if let ServerMsg::AgentStatusChanged { id: sid, status } = recv(c).await {
            if sid == id {
                return status;
            }
        }
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hooks_drive_status_and_all_clients_are_notified() {
    let d = start_daemon("hooks");
    let mut a = client(&d.name).await;
    let mut b = client(&d.name).await;
    let (shell, args) = idle_shell();
    let id = create(&mut a, shell, &args).await;

    send(&mut a, hook(id, &event("UserPromptSubmit", r#","prompt":"hi""#))).await;
    for c in [&mut a, &mut b] {
        let s = next_status(c, id).await.expect("status");
        assert_eq!((s.kind, s.state, s.unread), (AgentKind::Claude, AgentState::Working, false));
    }

    send(&mut a, hook(id, &event("PermissionRequest", r#","tool_name":"Bash""#))).await;
    let s = next_status(&mut b, id).await.unwrap();
    assert_eq!(s.state, AgentState::Waiting);
    assert_eq!(s.detail.as_deref(), Some("Bash"));
    next_status(&mut a, id).await;

    send(&mut a, hook(id, &event("Stop", r#","stop_hook_active":false"#))).await;
    let s = next_status(&mut b, id).await.unwrap();
    assert_eq!((s.state, s.unread), (AgentState::Done, true));
    next_status(&mut a, id).await;

    // The status is part of the terminal list for late clients.
    let mut late = client(&d.name).await;
    send(&mut late, ClientMsg::ListTerminals).await;
    loop {
        if let ServerMsg::Terminals(list) = recv(&mut late).await {
            let agent = list[0].agent.clone().expect("agent in TerminalInfo");
            assert_eq!((agent.state, agent.unread), (AgentState::Done, true));
            break;
        }
    }

    send(&mut b, ClientMsg::MarkSeen { id }).await;
    for c in [&mut a, &mut b] {
        let s = next_status(c, id).await.unwrap();
        assert_eq!((s.state, s.unread), (AgentState::Done, false));
    }

    // Garbage payloads, unknown terminals and non-Claude agents change nothing and do not crash.
    send(&mut a, hook(id, "not json")).await;
    send(&mut a, hook(9999, &event("Stop", ""))).await;
    send(&mut a, ClientMsg::AgentHook { term_id: id, agent: AgentKind::Gemini, payload: event("Stop", "") }).await;
    send(&mut a, ClientMsg::MarkSeen { id: 9999 }).await;

    // SessionEnd clears the status.
    send(&mut a, hook(id, &event("SessionEnd", ""))).await;
    assert_eq!(next_status(&mut a, id).await, None);

    stop(a, d).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn process_exit_clears_status() {
    let d = start_daemon("exit");
    let mut a = client(&d.name).await;
    let (shell, args) = idle_shell();
    let id = create(&mut a, shell, &args).await;
    send(&mut a, hook(id, &event("UserPromptSubmit", ""))).await;
    assert_eq!(next_status(&mut a, id).await.unwrap().state, AgentState::Working);

    send(&mut a, ClientMsg::Subscribe { id, resume_from_seq: None }).await;
    send(&mut a, ClientMsg::Input { id, bytes: b"exit\r\n".to_vec() }).await;
    // ConPTY waits for the cursor position answer at startup; answer like a real terminal.
    let status = loop {
        match recv(&mut a).await {
            ServerMsg::Output { id: oid, bytes, .. } if oid == id => {
                if contains(&bytes, b"[6n") {
                    send(&mut a, ClientMsg::Input { id, bytes: b"\x1b[1;1R".to_vec() }).await;
                }
            }
            ServerMsg::AgentStatusChanged { id: sid, status } if sid == id => break status,
            _ => {}
        }
    };
    assert_eq!(status, None);
    stop(a, d).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn killing_a_terminal_clears_status_before_removal() {
    let d = start_daemon("kill");
    let mut a = client(&d.name).await;
    let (shell, args) = idle_shell();
    let id = create(&mut a, shell, &args).await;
    send(&mut a, hook(id, &event("UserPromptSubmit", ""))).await;
    next_status(&mut a, id).await;
    send(&mut a, ClientMsg::KillTerminal { id }).await;
    let mut cleared = false;
    loop {
        match recv(&mut a).await {
            ServerMsg::AgentStatusChanged { id: sid, status: None } if sid == id => cleared = true,
            ServerMsg::TerminalRemoved { id: rid } if rid == id => break,
            _ => {}
        }
    }
    assert!(cleared);
    stop(a, d).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn osc_title_is_broadcast_and_listed() {
    let d = start_daemon("title");
    let mut a = client(&d.name).await;
    let (shell, args): (&str, Vec<&str>) = if cfg!(windows) {
        (
            "powershell.exe",
            vec![
                "-NoProfile",
                "-Command",
                "[Console]::OutputEncoding=[Text.Encoding]::UTF8; $e=[char]27; $b=[char]7; [Console]::Out.Write($e+']0;'+[char]0x2733+' Claude'+$b); Start-Sleep -Seconds 3",
            ],
        )
    } else {
        ("/bin/sh", vec!["-c", "printf '\\033]0;\\342\\234\\263 Claude\\007'; sleep 3"])
    };
    let id = create(&mut a, shell, &args).await;
    send(&mut a, ClientMsg::Subscribe { id, resume_from_seq: None }).await;

    let expected = "\u{2733} Claude";
    let mut saw_title = false;
    let mut saw_status = false;
    while !(saw_title && saw_status) {
        match recv(&mut a).await {
            ServerMsg::Output { id: oid, bytes, .. } if oid == id => {
                if contains(&bytes, b"[6n") {
                    send(&mut a, ClientMsg::Input { id, bytes: b"\x1b[1;1R".to_vec() }).await;
                }
            }
            ServerMsg::TerminalTitle { id: tid, title } if tid == id && title == expected => {
                saw_title = true;
            }
            // The Claude idle glyph also yields a title-derived status (no hooks seen).
            ServerMsg::AgentStatusChanged { id: sid, status: Some(s) } if sid == id => {
                assert_eq!(s.kind, AgentKind::Claude);
                assert_eq!(s.state, AgentState::Idle);
                saw_status = true;
            }
            _ => {}
        }
    }
    send(&mut a, ClientMsg::ListTerminals).await;
    loop {
        if let ServerMsg::Terminals(list) = recv(&mut a).await {
            assert_eq!(list[0].title, expected);
            break;
        }
    }
    stop(a, d).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_env_carries_term_id_and_socket() {
    let d = start_daemon("env");
    let mut a = client(&d.name).await;
    let (shell, args): (&str, Vec<&str>) =
        if cfg!(windows) { ("cmd.exe", vec!["/c", "set"]) } else { ("/usr/bin/env", vec![]) };
    let id = create(&mut a, shell, &args).await;
    send(&mut a, ClientMsg::Subscribe { id, resume_from_seq: None }).await;
    let mut out = Vec::new();
    loop {
        match recv(&mut a).await {
            ServerMsg::Output { id: oid, bytes, .. } if oid == id => {
                if contains(&bytes, b"[6n") {
                    send(&mut a, ClientMsg::Input { id, bytes: b"\x1b[1;1R".to_vec() }).await;
                }
                out.extend_from_slice(&bytes);
            }
            ServerMsg::TerminalExited { id: eid, .. } if eid == id => break,
            _ => {}
        }
    }
    let text = String::from_utf8_lossy(&out);
    assert!(text.contains(&format!("SICH_TERM_ID={id}")), "{text}");
    assert!(text.contains(&format!("SICH_SOCKET={}", d.name)), "{text}");
    stop(a, d).await;
}
