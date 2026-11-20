//! Non-UI entry points: `sich daemon` and `sich daemon stop`, plus daemon auto-start.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use futures::SinkExt as _;
use sich_proto::{ClientMsg, encode, frame_codec};
use tokio_util::codec::Framed;

const START_TIMEOUT: Duration = Duration::from_secs(5);

/// `sich daemon`: runs the daemon in this process until `Shutdown`.
pub fn run_daemon(socket: &str) -> anyhow::Result<()> {
    let _guard = match sich_daemon::init_logging() {
        Ok(g) => Some(g),
        Err(e) => {
            eprintln!("warning: logging disabled: {e:#}");
            None
        }
    };
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    crate::daemon_host::prune_old_hosts();
    let res = rt.block_on(sich_daemon::run_generation(socket, sich_app::daemon_build_id()));
    if let Err(e) = &res {
        tracing::error!("daemon failed: {e:#}");
    }
    res
}

/// `sich daemon stop`: asks a running daemon to shut down.
pub fn stop_daemon(base: &str) -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    rt.block_on(async {
        if std::env::var_os("SICH_TERM_ID").is_none() {
            let socket = sich_daemon::generation::socket(base, sich_app::daemon_build_id());
            if sich_daemon::connect(&socket).await.is_ok() {
                return shutdown(&socket).await;
            }
        }
        shutdown(base).await
    })
}

pub async fn shutdown(socket: &str) -> anyhow::Result<()> {
    let stream = sich_daemon::connect(socket).await.context("daemon is not running")?;
    let mut framed = Framed::new(stream, frame_codec());
    // `Shutdown` is accepted before `Hello` by every daemon version.
    framed.send(encode(&ClientMsg::Shutdown)?).await?;
    Ok(())
}

/// Starts or adopts the daemon for the CLI's daemon start command.
pub fn start_daemon(socket: &str) -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt().with_writer(std::io::stderr).try_init();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    rt.block_on(ensure_daemon(socket))
}

/// Makes sure a daemon is reachable, starting a detached one if needed (waits up to ~5 s).
pub async fn ensure_daemon(base: &str) -> anyhow::Result<()> {
    let socket = sich_daemon::generation::socket(base, sich_app::daemon_build_id());
    if sich_daemon::connect(&socket).await.is_ok() {
        return Ok(());
    }
    spawn_detached_daemon(base)?;
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        if sich_daemon::connect(&socket).await.is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!("daemon did not start within {} s", START_TIMEOUT.as_secs());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

pub fn spawn_replacement_ui() -> anyhow::Result<()> {
    // The user-facing exe, so a freshly built one is picked up (it relocates itself).
    let exe = crate::daemon_host::launcher_exe().context("locate sich executable")?;
    spawn_detached(&exe, "--takeover", None)
}

fn spawn_detached_daemon(base: &str) -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("locate current executable")?;
    let host = crate::daemon_host::spawn_path(&exe);
    match spawn_detached(&host, "daemon", Some(base)) {
        Err(err) if host != exe => {
            tracing::warn!("could not launch relocated daemon: {err:#}");
            spawn_detached(&exe, "daemon", Some(base))
        }
        result => result,
    }
}

fn spawn_detached(exe: &std::path::Path, arg: &str, base: Option<&str>) -> anyhow::Result<()> {
    let mut cmd = Command::new(exe);
    // Never leak the launcher marker: a replacement UI must relocate itself, and terminals
    // spawned by the daemon must not inherit a stale value.
    cmd.arg(arg).env_remove(crate::daemon_host::LAUNCHER_ENV).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    if let Some(base) = base {
        cmd.env("SICH_SOCKET", base);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    }
    cmd.spawn().with_context(|| format!("spawn {} {arg}", exe.display()))?;
    Ok(())
}
