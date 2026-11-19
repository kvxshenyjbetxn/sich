//! Replaces an incompatible daemon: kills the old process and starts a fresh one.
//!
//! Killing uses `sysinfo` (TerminateProcess / SIGKILL), so no console window ever appears.

use std::time::{Duration, Instant};

use anyhow::Context as _;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

const GONE_TIMEOUT: Duration = Duration::from_secs(4);

/// True for a `sich daemon` process that is not us.
pub fn is_old_daemon_process(name: &str, cmd: &[String], pid: u32, self_pid: u32) -> bool {
    if pid == self_pid {
        return false;
    }
    let name = name.to_ascii_lowercase();
    if name != "sich.exe" && name != "sich" {
        return false;
    }
    cmd.iter().skip(1).any(|a| a == "daemon")
}

/// Kills the daemon with `pid`, or (when unknown) every other `sich daemon` process.
/// Returns how many processes were killed.
pub fn kill_old_daemons(pid: Option<u32>) -> anyhow::Result<usize> {
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        ProcessRefreshKind::new().with_cmd(UpdateKind::Always),
    );
    let self_pid = std::process::id();
    let mut killed = 0;
    match pid {
        Some(pid) => {
            anyhow::ensure!(pid != self_pid, "refusing to kill ourselves");
            let p = sys.process(Pid::from_u32(pid)).context("daemon process not found")?;
            anyhow::ensure!(p.kill(), "could not kill daemon process {pid}");
            killed += 1;
        }
        None => {
            for (p_pid, p) in sys.processes() {
                let name = p.name().to_string_lossy().into_owned();
                let cmd: Vec<String> =
                    p.cmd().iter().map(|a| a.to_string_lossy().into_owned()).collect();
                if is_old_daemon_process(&name, &cmd, p_pid.as_u32(), self_pid) && p.kill() {
                    killed += 1;
                }
            }
            anyhow::ensure!(killed > 0, "no running daemon process found");
        }
    }
    Ok(killed)
}

/// Kills the old daemon, waits until its socket is gone, then starts a new daemon.
pub async fn restart(socket: &str, pid: Option<u32>) -> anyhow::Result<()> {
    tokio::task::spawn_blocking(move || kill_old_daemons(pid))
        .await
        .context("kill task failed")??;
    let deadline = Instant::now() + GONE_TIMEOUT;
    while sich_daemon::connect(socket).await.is_ok() {
        anyhow::ensure!(Instant::now() < deadline, "old daemon is still running");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    crate::cli::ensure_daemon(socket).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn matches_sich_daemon_processes() {
        assert!(is_old_daemon_process("sich.exe", &cmd(&["C:\\x\\sich.exe", "daemon"]), 10, 1));
        assert!(is_old_daemon_process("Sich.EXE", &cmd(&["sich", "daemon"]), 10, 1));
        assert!(is_old_daemon_process("sich", &cmd(&["/bin/sich", "daemon"]), 10, 1));
    }

    #[test]
    fn rejects_ui_other_programs_and_self() {
        assert!(!is_old_daemon_process("sich.exe", &cmd(&["sich.exe"]), 10, 1));
        assert!(!is_old_daemon_process("pwsh.exe", &cmd(&["pwsh.exe", "daemon"]), 10, 1));
        assert!(!is_old_daemon_process("sich.exe", &cmd(&["sich.exe", "daemon"]), 1, 1));
        assert!(!is_old_daemon_process("sich.exe", &[], 10, 1));
        // The program path itself containing "daemon" is not enough.
        assert!(!is_old_daemon_process("sich.exe", &cmd(&["C:\\daemon\\sich.exe"]), 10, 1));
    }
}
