//! The hook command. Hard rules: always exits 0, never prints to stdout (Claude injects hook
//! stdout into the conversation for some events), finishes within a 2 s budget.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context as _;
use futures::{SinkExt as _, StreamExt as _};
use sich_proto::{AgentKind, ClientMsg, PROTO_VERSION, ServerMsg, decode, encode, frame_codec};
use tokio_util::codec::Framed;

/// The hook's stdin is read up to this many bytes.
pub const MAX_STDIN_BYTES: usize = 1024 * 1024;
const TOTAL_BUDGET: Duration = Duration::from_secs(2);
/// Overall connect deadline, including retries. Leaves room in the 2 s budget for the send.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(1200);
const RETRY_FIRST_DELAY: Duration = Duration::from_millis(25);
const RETRY_MAX_DELAY: Duration = Duration::from_millis(200);
const WELCOME_WAIT: Duration = Duration::from_millis(300);
const MAX_LOG_BYTES: u64 = 1024 * 1024;

/// The part of the environment the hook cares about (injectable for tests).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookEnv {
    pub term_id: Option<String>,
    pub socket: Option<String>,
}

impl HookEnv {
    pub fn from_process() -> Self {
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        Self { term_id: get("SICH_TERM_ID"), socket: get("SICH_SOCKET") }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookOutcome {
    /// Not running inside a Sich terminal (or unusable id / oversized payload): nothing was sent.
    Skipped,
    Sent,
    /// The daemon could not be reached; the error text is for the log.
    DaemonUnreachable(String),
}

/// Reads at most `cap` bytes. The flag says whether the input was longer than that.
pub fn read_capped(r: &mut dyn Read, cap: usize) -> std::io::Result<(Vec<u8>, bool)> {
    let mut buf = Vec::new();
    r.take(cap as u64 + 1).read_to_end(&mut buf)?;
    let truncated = buf.len() > cap;
    buf.truncate(cap);
    Ok((buf, truncated))
}

/// Core of the hook, free of process-global state. `connect` delivers the messages to the daemon
/// at the given socket name. A missing `SICH_TERM_ID` means Claude runs outside Sich: stdin is
/// not even read. A daemon that is down is not an error.
pub fn run_hook(
    env: &HookEnv,
    agent: AgentKind,
    stdin: &mut dyn Read,
    connect: &mut dyn FnMut(&str, Vec<ClientMsg>) -> anyhow::Result<()>,
) -> anyhow::Result<HookOutcome> {
    let Some(term_id) = env.term_id.as_deref() else { return Ok(HookOutcome::Skipped) };
    let term_id: u64 = term_id.trim().parse().context("SICH_TERM_ID is not a number")?;
    let (bytes, truncated) = read_capped(stdin, MAX_STDIN_BYTES).context("read stdin")?;
    if truncated {
        // A cut-off JSON document is useless; do not send garbage.
        return Ok(HookOutcome::Skipped);
    }
    let payload = String::from_utf8_lossy(&bytes).into_owned();
    let socket = env
        .socket
        .clone()
        .unwrap_or_else(|| sich_daemon::DEFAULT_SOCKET_NAME.to_string());
    let msgs = vec![
        ClientMsg::Hello { proto_version: PROTO_VERSION, client_name: "sich-hook".into() },
        ClientMsg::AgentHook { term_id, agent, payload },
    ];
    match connect(&socket, msgs) {
        Ok(()) => Ok(HookOutcome::Sent),
        Err(e) => Ok(HookOutcome::DaemonUnreachable(format!("{e:#}"))),
    }
}

/// True for connect errors that mean "the listener is momentarily not ready": a Windows pipe with
/// no free instance between two accepts (os error 2 / 231) or a Unix socket file being recreated.
pub fn is_transient_connect_error(e: &std::io::Error) -> bool {
    use std::io::ErrorKind;
    matches!(e.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused)
        || matches!(e.raw_os_error(), Some(2 | 231))
}

/// Calls `connect` until it succeeds, fails with a non-transient error, or `deadline` passes.
/// Transient failures back off 25 ms, doubling up to 200 ms; the last error is returned.
pub async fn connect_with_retry<T, F, Fut>(mut connect: F, deadline: Duration) -> std::io::Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = std::io::Result<T>>,
{
    let start = tokio::time::Instant::now();
    let mut delay = RETRY_FIRST_DELAY;
    loop {
        match connect().await {
            Ok(v) => return Ok(v),
            Err(e) if is_transient_connect_error(&e) => {
                let left = deadline.saturating_sub(start.elapsed());
                if left.is_zero() {
                    return Err(e);
                }
                tokio::time::sleep(delay.min(left)).await;
                delay = (delay * 2).min(RETRY_MAX_DELAY);
            }
            Err(e) => return Err(e),
        }
    }
}

/// Real delivery: connects (retrying while the daemon is briefly not accepting, 1.2 s overall),
/// sends the frames and closes.
pub fn send_to_daemon(socket: &str, msgs: Vec<ClientMsg>) -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    rt.block_on(async {
        let stream = tokio::time::timeout(
            CONNECT_TIMEOUT,
            connect_with_retry(|| sich_daemon::connect(socket), CONNECT_TIMEOUT),
        )
        .await
            .context("connect timed out")?
            .context("connect")?;
        let mut framed = Framed::new(stream, frame_codec());
        let omp = matches!(msgs.last(), Some(ClientMsg::AgentHook { agent: AgentKind::Omp, .. }));
        for m in &msgs {
            framed.send(encode(m)?).await.context("send")?;
        }
        if omp {
            // The extension sends snapshots serially. Wait for a reply to a following
            // request so the next process cannot overtake this hook on a new connection.
            framed.send(encode(&ClientMsg::GetDaemonInfo)?).await.context("send hook barrier")?;
            return tokio::time::timeout(WELCOME_WAIT, async {
                while let Some(frame) = framed.next().await {
                    match decode::<ServerMsg>(&frame.context("read hook reply")?)? {
                        ServerMsg::DaemonInfo { .. } => return Ok(()),
                        ServerMsg::Error { message } => anyhow::bail!("{message}"),
                        _ => {}
                    }
                }
                anyhow::bail!("daemon closed before processing the hook");
            })
            .await
            .context("hook reply timed out")?;
        }
        // Give the daemon a moment to take the frames before the pipe closes.
        let _ = tokio::time::timeout(WELCOME_WAIT, framed.next()).await;
        Ok(())
    })
}

/// `%LOCALAPPDATA%\sich\logs\hook.log` (or the platform equivalent).
pub fn log_path() -> Option<PathBuf> {
    // SICH_LOG_DIR lets tests keep the hook log out of the user's real data dir.
    if let Some(d) = std::env::var_os("SICH_LOG_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(d).join("hook.log"));
    }
    Some(dirs::data_local_dir()?.join("sich").join("logs").join("hook.log"))
}

/// Appends a line; when the file is over 1 MiB it is truncated first. Errors are ignored.
pub fn append_log(path: &Path, line: &str) {
    let _ = (|| -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let too_big = std::fs::metadata(path).is_ok_and(|m| m.len() > MAX_LOG_BYTES);
        let mut opts = std::fs::OpenOptions::new();
        opts.create(true).write(true);
        if too_big {
            opts.truncate(true);
        } else {
            opts.append(true);
        }
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        writeln!(opts.open(path)?, "[{ts}] {line}")
    })();
}

/// Entry point of `sich hook <agent>`. Never returns and always exits with 0.
pub fn hook_main(agent_name: &str) -> ! {
    // Watchdog: whatever blocks (stdin never closed, a hung pipe), we are gone after 2 s.
    let _ = std::thread::Builder::new().name("hook-watchdog".into()).spawn(|| {
        std::thread::sleep(TOTAL_BUDGET);
        std::process::exit(0);
    });
    let agent = match agent_name {
        "claude" => AgentKind::Claude,
        "gemini" => AgentKind::Gemini,
        "codex" => AgentKind::Codex,
        "omp" => AgentKind::Omp,
        _ => AgentKind::Other,
    };
    let env = HookEnv::from_process();
    let result = std::panic::catch_unwind(|| {
        run_hook(&env, agent, &mut std::io::stdin().lock(), &mut send_to_daemon)
    });
    let problem = match result {
        Ok(Ok(HookOutcome::DaemonUnreachable(e))) => Some(format!("daemon unreachable: {e}")),
        Ok(Ok(_)) => None,
        Ok(Err(e)) => Some(format!("{e:#}")),
        Err(_) => Some("panic".to_string()),
    };
    if let (Some(msg), Some(path)) = (problem, log_path()) {
        append_log(&path, &format!("[{agent_name}] {msg}"));
    }
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Stdin that must never be read.
    struct NoRead;
    impl Read for NoRead {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            panic!("stdin must not be read");
        }
    }

    fn env(id: Option<&str>, socket: Option<&str>) -> HookEnv {
        HookEnv { term_id: id.map(Into::into), socket: socket.map(Into::into) }
    }

    #[test]
    fn no_term_id_means_no_connect_and_no_stdin_read() {
        let mut called = false;
        let mut connect = |_: &str, _: Vec<ClientMsg>| {
            called = true;
            Ok(())
        };
        let out = run_hook(&env(None, Some("s")), AgentKind::Claude, &mut NoRead, &mut connect).unwrap();
        assert_eq!(out, HookOutcome::Skipped);
        assert!(!called);
    }

    #[test]
    fn sends_hello_then_agent_hook() {
        let mut got: Option<(String, Vec<ClientMsg>)> = None;
        let mut connect = |s: &str, m: Vec<ClientMsg>| {
            got = Some((s.to_string(), m));
            Ok(())
        };
        let mut stdin = Cursor::new(br#"{"hook_event_name":"Stop"}"#.to_vec());
        let out = run_hook(&env(Some("12"), Some("sock-x")), AgentKind::Claude, &mut stdin, &mut connect).unwrap();
        assert_eq!(out, HookOutcome::Sent);
        let (socket, msgs) = got.unwrap();
        assert_eq!(socket, "sock-x");
        assert_eq!(msgs.len(), 2);
        assert!(matches!(&msgs[0], ClientMsg::Hello { proto_version, .. } if *proto_version == PROTO_VERSION));
        assert_eq!(
            msgs[1],
            ClientMsg::AgentHook {
                term_id: 12,
                agent: AgentKind::Claude,
                payload: r#"{"hook_event_name":"Stop"}"#.into()
            }
        );
    }

    #[test]
    fn missing_socket_uses_default_name() {
        let mut socket = String::new();
        let mut connect = |s: &str, _: Vec<ClientMsg>| {
            socket = s.to_string();
            Ok(())
        };
        run_hook(&env(Some("1"), None), AgentKind::Claude, &mut Cursor::new(Vec::new()), &mut connect).unwrap();
        assert_eq!(socket, sich_daemon::DEFAULT_SOCKET_NAME);
    }

    #[test]
    fn daemon_down_is_still_ok() {
        let mut connect = |_: &str, _: Vec<ClientMsg>| anyhow::bail!("connection refused");
        let out = run_hook(&env(Some("3"), Some("s")), AgentKind::Claude, &mut Cursor::new(b"{}".to_vec()), &mut connect)
            .unwrap();
        assert!(matches!(out, HookOutcome::DaemonUnreachable(m) if m.contains("refused")));
    }

    #[test]
    fn real_connect_to_missing_daemon_fails_fast_and_is_swallowed() {
        let name = format!("sich-hook-test-nonexistent-{}", std::process::id());
        let started = std::time::Instant::now();
        let out = run_hook(
            &env(Some("3"), Some(&name)),
            AgentKind::Claude,
            &mut Cursor::new(b"{}".to_vec()),
            &mut send_to_daemon,
        )
        .unwrap();
        assert!(matches!(out, HookOutcome::DaemonUnreachable(_)));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    fn err(kind: std::io::ErrorKind) -> std::io::Error {
        std::io::Error::from(kind)
    }

    #[test]
    fn transient_classification() {
        assert!(is_transient_connect_error(&err(std::io::ErrorKind::NotFound)));
        assert!(is_transient_connect_error(&err(std::io::ErrorKind::ConnectionRefused)));
        assert!(is_transient_connect_error(&std::io::Error::from_raw_os_error(2)));
        assert!(is_transient_connect_error(&std::io::Error::from_raw_os_error(231)));
        assert!(!is_transient_connect_error(&err(std::io::ErrorKind::PermissionDenied)));
        assert!(!is_transient_connect_error(&err(std::io::ErrorKind::InvalidInput)));
    }

    #[tokio::test]
    async fn retry_succeeds_after_transient_failures() {
        let mut calls = 0;
        let r = connect_with_retry(
            || {
                calls += 1;
                let n = calls;
                async move { if n < 4 { Err(err(std::io::ErrorKind::NotFound)) } else { Ok(n) } }
            },
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(r.unwrap(), 4);
    }

    #[tokio::test]
    async fn retry_does_not_repeat_non_transient_errors() {
        let mut calls = 0;
        let r: std::io::Result<()> = connect_with_retry(
            || {
                calls += 1;
                async { Err(err(std::io::ErrorKind::PermissionDenied)) }
            },
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(r.unwrap_err().kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(calls, 1);
    }

    #[tokio::test]
    async fn retry_respects_deadline_and_returns_last_error() {
        let started = std::time::Instant::now();
        let mut calls = 0;
        let r: std::io::Result<()> = connect_with_retry(
            || {
                calls += 1;
                async { Err(std::io::Error::from_raw_os_error(2)) }
            },
            Duration::from_millis(150),
        )
        .await;
        assert_eq!(r.unwrap_err().raw_os_error(), Some(2));
        assert!(calls >= 2, "must have retried, got {calls}");
        assert!(started.elapsed() >= Duration::from_millis(140));
        assert!(started.elapsed() < Duration::from_millis(1000));
    }

    /// The burst scenario: the hook starts while no listener exists yet and the daemon appears
    /// a bit later. Delivery must still succeed.
    #[test]
    fn delivery_succeeds_when_listener_appears_late() {
        use interprocess::local_socket::tokio::prelude::*;
        use interprocess::local_socket::{GenericFilePath, GenericNamespaced, ListenerOptions};
        let name = format!("sich-hook-test-late-{}", std::process::id());
        let (tx, rx) = std::sync::mpsc::channel();
        let server_name = name.clone();
        let server = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async {
                tokio::time::sleep(Duration::from_millis(300)).await;
                let n = if GenericNamespaced::is_supported() {
                    server_name.clone().to_ns_name::<GenericNamespaced>().unwrap()
                } else {
                    std::env::temp_dir().join(format!("{server_name}.sock")).to_fs_name::<GenericFilePath>().unwrap()
                };
                let listener = ListenerOptions::new().name(n).create_tokio().unwrap();
                let stream = tokio::time::timeout(Duration::from_secs(3), listener.accept())
                    .await
                    .expect("hook never connected")
                    .unwrap();
                let mut framed = Framed::new(stream, frame_codec());
                let first = framed.next().await.unwrap().unwrap();
                let _ = tx.send(decode::<ClientMsg>(&first).unwrap());
            });
        });
        let msgs = vec![ClientMsg::Hello { proto_version: PROTO_VERSION, client_name: "t".into() }];
        send_to_daemon(&name, msgs).expect("delivery must survive a late listener");
        let got = rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(matches!(got, ClientMsg::Hello { .. }));
        server.join().unwrap();
    }

    #[test]
    fn bad_term_id_is_an_error_for_the_caller_to_swallow() {
        let mut connect = |_: &str, _: Vec<ClientMsg>| Ok(());
        let r = run_hook(&env(Some("abc"), None), AgentKind::Claude, &mut Cursor::new(Vec::new()), &mut connect);
        assert!(r.is_err());
    }

    #[test]
    fn oversized_stdin_is_not_sent() {
        let mut called = false;
        let mut connect = |_: &str, _: Vec<ClientMsg>| {
            called = true;
            Ok(())
        };
        let mut stdin = Cursor::new(vec![b'x'; MAX_STDIN_BYTES + 10]);
        let out = run_hook(&env(Some("1"), None), AgentKind::Claude, &mut stdin, &mut connect).unwrap();
        assert_eq!(out, HookOutcome::Skipped);
        assert!(!called);
    }

    #[test]
    fn read_capped_limits() {
        let (b, t) = read_capped(&mut Cursor::new(vec![1u8; 10]), 10).unwrap();
        assert_eq!((b.len(), t), (10, false));
        let (b, t) = read_capped(&mut Cursor::new(vec![1u8; 11]), 10).unwrap();
        assert_eq!((b.len(), t), (10, true));
        let (b, t) = read_capped(&mut Cursor::new(Vec::new()), 10).unwrap();
        assert_eq!((b.len(), t), (0, false));
    }

    #[test]
    fn log_appends_and_truncates_when_big() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs").join("hook.log");
        append_log(&path, "one");
        append_log(&path, "two");
        // Lines are "[<unix secs>] <text>".
        let strip = |s: String| -> Vec<String> {
            s.lines()
                .map(|l| {
                    assert!(l.starts_with('['), "missing timestamp: {l}");
                    l.split_once("] ").map(|(_, t)| t.to_string()).unwrap_or_default()
                })
                .collect()
        };
        assert_eq!(strip(std::fs::read_to_string(&path).unwrap()), ["one", "two"]);
        std::fs::write(&path, vec![b'x'; (MAX_LOG_BYTES + 1) as usize]).unwrap();
        append_log(&path, "fresh");
        assert_eq!(strip(std::fs::read_to_string(&path).unwrap()), ["fresh"]);
    }
}
