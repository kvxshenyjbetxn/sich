//! Non-UI entry points: `sich daemon` and `sich daemon stop`, plus daemon auto-start.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use futures::SinkExt as _;
use futures::StreamExt as _;
use sich_proto::{ClientMsg, PROTO_VERSION, ServerMsg, decode, encode, frame_codec};
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
    let res = rt.block_on(sich_daemon::run(socket));
    if let Err(e) = &res {
        tracing::error!("daemon failed: {e:#}");
    }
    res
}

/// `sich daemon stop`: asks a running daemon to shut down.
pub fn stop_daemon(socket: &str) -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    rt.block_on(async {
        let stream = sich_daemon::connect(socket).await.context("daemon is not running")?;
        let mut framed = Framed::new(stream, frame_codec());
        framed
            .send(encode(&ClientMsg::Hello {
                proto_version: PROTO_VERSION,
                client_name: "sich-cli".into(),
            })?)
            .await?;
        match framed.next().await {
            Some(Ok(frame)) => match decode::<ServerMsg>(&frame)? {
                ServerMsg::Welcome { .. } => {}
                ServerMsg::Error { message } => anyhow::bail!("daemon refused: {message}"),
                other => anyhow::bail!("unexpected reply: {other:?}"),
            },
            Some(Err(e)) => return Err(e).context("read reply"),
            None => anyhow::bail!("daemon closed the connection"),
        }
        framed.send(encode(&ClientMsg::Shutdown)?).await?;
        Ok(())
    })
}

/// Makes sure a daemon is reachable, starting a detached one if needed (waits up to ~5 s).
pub async fn ensure_daemon(socket: &str) -> anyhow::Result<()> {
    if sich_daemon::connect(socket).await.is_ok() {
        return Ok(());
    }
    spawn_detached_daemon()?;
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        if sich_daemon::connect(socket).await.is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!("daemon did not start within {} s", START_TIMEOUT.as_secs());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn spawn_detached_daemon() -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("locate current executable")?;
    let mut cmd = Command::new(exe);
    cmd.arg("daemon").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    }
    cmd.spawn().context("spawn daemon process")?;
    Ok(())
}
