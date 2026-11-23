//! Non-UI entry points: `sich daemon` and `sich daemon stop`, plus daemon auto-start.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use futures::SinkExt as _;
use sich_proto::{ClientMsg, encode, frame_codec};
use tokio_util::codec::Framed;

use crate::gateway_host;

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

/// `sich gateway [--takeover]`: runs the phone gateway in this process until a control client
/// sends `Shutdown`.
pub fn run_gateway(base: &str, takeover: bool) -> anyhow::Result<()> {
    let _guard = match sich_gateway::init_logging() {
        Ok(g) => Some(g),
        Err(e) => {
            eprintln!("warning: logging disabled: {e:#}");
            None
        }
    };
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let opts = sich_gateway::Opts::from_env(base.to_string(), takeover, sich_app::build_id())?;
    let res = rt.block_on(sich_gateway::run(opts));
    if let Err(e) = &res {
        tracing::error!("gateway failed: {e:#}");
    }
    res
}

/// Makes sure a phone gateway of this build is reachable, starting a detached one if needed.
/// A gateway of another build is replaced (`--takeover`). Waits up to ~5 s (10 s when replacing).
/// The GUI calls this too.
pub async fn ensure_gateway(base: &str) -> anyhow::Result<()> {
    let own = sich_app::build_id();
    let data_dir = sich_daemon::data_local_dir();
    let stable = gateway_host::plan(gateway_host::no_relocate(), data_dir.as_deref());
    let mut takeover = false;
    match sich_gateway::ControlClient::connect_as(base, own).await {
        Ok(running) if running.gateway_build_id() == own => return Ok(()),
        Ok(mut running) => {
            if stable.is_some() {
                // The stable copy is locked while a gateway runs from it: stop the old one first.
                running.shutdown().await?;
                drop(running);
                wait_gateway_gone(base, own).await?;
            } else {
                takeover = true;
            }
        }
        Err(_) => {}
    }
    match stable {
        Some(target) => spawn_stable_gateway(base, &target).await?,
        None => spawn_detached_gateway(base, takeover)?,
    }
    let limit = if takeover { START_TIMEOUT * 2 } else { START_TIMEOUT };
    let deadline = Instant::now() + limit;
    loop {
        if let Ok(running) = sich_gateway::ControlClient::connect_as(base, own).await {
            if running.gateway_build_id() == own {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            anyhow::bail!("gateway did not start within {} s", limit.as_secs());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Waits (up to 5 s) until nothing answers on the gateway control pipe any more.
async fn wait_gateway_gone(base: &str, own: &str) -> anyhow::Result<()> {
    let deadline = Instant::now() + START_TIMEOUT;
    while sich_gateway::ControlClient::connect_as(base, own).await.is_ok() {
        if Instant::now() >= deadline {
            anyhow::bail!("the previous gateway did not stop within {} s", START_TIMEOUT.as_secs());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(())
}

/// Copies this build to the stable gateway path and runs it from there. If the copy fails the
/// gateway still starts from the per-build copy (the firewall just asks again).
async fn spawn_stable_gateway(base: &str, target: &std::path::Path) -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("locate current executable")?;
    let (src, dst) = (exe.clone(), target.to_path_buf());
    let installed = tokio::task::spawn_blocking(move || gateway_host::install(&src, &dst)).await?;
    match installed {
        Ok(()) => match spawn_detached_args(target, &["gateway"], Some(base)) {
            Ok(()) => Ok(()),
            Err(err) => {
                tracing::warn!("could not launch the stable gateway: {err:#}");
                spawn_detached_gateway(base, false)
            }
        },
        Err(err) => {
            tracing::warn!("could not install the stable gateway executable: {err}");
            spawn_detached_gateway(base, false)
        }
    }
}

fn spawn_detached_gateway(base: &str, takeover: bool) -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("locate current executable")?;
    let host = crate::daemon_host::spawn_path(&exe);
    let args: &[&str] = if takeover { &["gateway", "--takeover"] } else { &["gateway"] };
    match spawn_detached_args(&host, args, Some(base)) {
        Err(err) if host != exe => {
            tracing::warn!("could not launch relocated gateway: {err:#}");
            spawn_detached_args(&exe, args, Some(base))
        }
        result => result,
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
    spawn_detached_args(exe, &[arg], base)
}

fn spawn_detached_args(exe: &std::path::Path, args: &[&str], base: Option<&str>) -> anyhow::Result<()> {
    let arg = args.join(" ");
    let mut cmd = Command::new(exe);
    // Never leak the launcher marker: a replacement UI must relocate itself, and terminals
    // spawned by the daemon must not inherit a stale value.
    cmd.args(args).env_remove(crate::daemon_host::LAUNCHER_ENV).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
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
