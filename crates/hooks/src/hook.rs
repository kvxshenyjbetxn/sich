//! The hook command. Hard rules: always exits 0, never prints to stdout (Claude injects hook
//! stdout into the conversation for some events), finishes within a 2 s budget.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context as _;
use futures::{SinkExt as _, StreamExt as _};
use sich_proto::{AgentKind, ClientMsg, PROTO_VERSION, encode, frame_codec};
use tokio_util::codec::Framed;

/// The hook's stdin is read up to this many bytes.
pub const MAX_STDIN_BYTES: usize = 1024 * 1024;
const TOTAL_BUDGET: Duration = Duration::from_secs(2);
const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
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

/// Real delivery: connects with a 500 ms timeout, sends the frames and closes.
pub fn send_to_daemon(socket: &str, msgs: Vec<ClientMsg>) -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    rt.block_on(async {
        let stream = tokio::time::timeout(CONNECT_TIMEOUT, sich_daemon::connect(socket))
            .await
            .context("connect timed out")?
            .context("connect")?;
        let mut framed = Framed::new(stream, frame_codec());
        for m in &msgs {
            framed.send(encode(m)?).await.context("send")?;
        }
        // Give the daemon a moment to take the frames before the pipe closes.
        let _ = tokio::time::timeout(WELCOME_WAIT, framed.next()).await;
        Ok(())
    })
}

/// `%LOCALAPPDATA%\sich\logs\hook.log` (or the platform equivalent).
pub fn log_path() -> Option<PathBuf> {
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
        writeln!(opts.open(path)?, "{line}")
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
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\ntwo\n");
        std::fs::write(&path, vec![b'x'; (MAX_LOG_BYTES + 1) as usize]).unwrap();
        append_log(&path, "fresh");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fresh\n");
    }
}
