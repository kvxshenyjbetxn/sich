//! Sich daemon: owns the PTYs and serves clients over a local socket.

mod agent;
mod codex_rollout;
mod env;
mod git;
mod hub;
mod ipc;
mod manager;
mod osc;
mod projects;
mod ring;
pub mod scrollback;
mod server;
mod shell;
mod terminal;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context as _;
use interprocess::local_socket::ListenerOptions;
use interprocess::local_socket::traits::tokio::Listener as _;

pub use interprocess::local_socket::tokio::Stream as ClientStream;
pub use ipc::{DEFAULT_SOCKET_NAME, connect, socket_name_from_env};
pub use ring::RingBuffer;

/// Base directory for the daemon executable copies and logs.
pub fn data_local_dir() -> Option<PathBuf> {
    std::env::var_os("SICH_DATA_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(dirs::data_local_dir)
}

pub(crate) fn config_dir() -> Option<PathBuf> {
    std::env::var_os("SICH_CONFIG_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(dirs::config_dir)
}

/// Starts file logging to `<data_local_dir>/sich/logs/daemon.log`. Keep the guard alive.
pub fn init_logging() -> anyhow::Result<tracing_appender::non_blocking::WorkerGuard> {
    let dir = data_local_dir().context("no local data dir")?.join("sich").join("logs");
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let appender = tracing_appender::rolling::never(&dir, "daemon.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_writer(writer)
        .with_ansi(false)
        .with_env_filter(filter)
        .try_init()
        .map_err(|e| anyhow::anyhow!("init logging: {e}"))?;
    Ok(guard)
}

/// Runs the daemon until a client sends `Shutdown`.
pub async fn run(socket_name: &str) -> anyhow::Result<()> {
    run_with_build(socket_name, env!("CARGO_PKG_VERSION")).await
}

/// Runs the daemon with the identifier supplied by its executable.
pub async fn run_with_build(socket_name: &str, build_id: &str) -> anyhow::Result<()> {
    let name = ipc::to_name(socket_name).context("invalid socket name")?;
    let listener = match ListenerOptions::new().name(name).create_tokio() {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            anyhow::bail!("another daemon is already running (socket `{socket_name}` is in use)")
        }
        Err(e) => return Err(e).with_context(|| format!("bind socket `{socket_name}`")),
    };
    tracing::info!(pid = std::process::id(), socket = socket_name, "daemon started");

    let mgr = manager::Manager::new(socket_name, build_id);
    if let Some(dir) = scrollback::directory() {
        scrollback::prune(&dir, std::time::SystemTime::now());
    }
    loop {
        tokio::select! {
            _ = mgr.shutdown.cancelled() => break,
            accepted = listener.accept() => match accepted {
                Ok(stream) => {
                    tokio::spawn(server::handle_connection(stream, mgr.clone()));
                }
                Err(e) => {
                    tracing::warn!("accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            },
        }
    }

    tracing::info!("daemon shutting down");
    mgr.save_scrollback().await;
    mgr.kill_all();
    tokio::time::sleep(Duration::from_millis(100)).await;
    Ok(())
}
