//! Installs one managed module in omp's official user extension directory.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context as _, ensure};
use tokio::process::Command;

const NAME: &str = "sich-omp-status.js";
const MARKER: &[u8] = b"// @sich-managed-omp-hook";
const SOURCE: &[u8] = include_bytes!("omp_status.js");
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    NotInstalled,
    NotFound,
    TooOld,
    Installed,
}

fn check_version(output: &str) -> Option<bool> {
    let mut parts = output.trim().strip_prefix("omp/")?.splitn(3, '.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next()?.parse().ok()?;
    let patch: u32 = parts.next()?.split(['-', '+']).next()?.parse().ok()?;
    Some((major, minor, patch) >= (19, 0, 1))
}

async fn query(args: &[&str]) -> anyhow::Result<Option<String>> {
    #[cfg(windows)]
    let mut cmd = {
        let Some(path) = std::env::var_os("PATH") else { return Ok(None) };
        let Some(exe) = std::env::split_paths(&path)
            .filter(|dir| dir.is_absolute())
            .flat_map(|dir| [dir.join("omp.exe"), dir.join("omp.cmd")])
            .find(|exe| exe.is_file())
        else {
            return Ok(None);
        };
        // Use an absolute PATH entry: cmd.exe also searches the current directory.
        let mut cmd = Command::new(exe);
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        cmd
    };
    #[cfg(not(windows))]
    let mut cmd = Command::new("omp");
    cmd.args(args).stdin(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true);
    match tokio::time::timeout(PROBE_TIMEOUT, cmd.output()).await.context("omp CLI timed out")? {
        Ok(out) if out.status.success() => Ok(Some(String::from_utf8(out.stdout).context("omp CLI returned non-UTF-8 output")?)),
        Ok(_) => Ok(None),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).context("run omp CLI"),
    }
}

fn recorded_path(receipt: &Path) -> anyhow::Result<Option<PathBuf>> {
    match fs::read(receipt) {
        Ok(bytes) => {
            let path: PathBuf = serde_json::from_slice(&bytes).context("read Sich's omp extension location")?;
            ensure!(path.is_absolute() && path.ends_with(Path::new("extensions").join(NAME)), "invalid Sich omp extension location");
            Ok(Some(path))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Reject an unrelated file or link at the managed path instead of replacing it.
fn read_owned(path: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    match fs::symlink_metadata(path) {
        Ok(meta) => ensure!(meta.is_file(), "not a Sich extension file: {}", path.display()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let mut bytes = Vec::new();
    File::open(path)?.take(SOURCE.len() as u64 + 1).read_to_end(&mut bytes)?;
    let owned = bytes.strip_prefix(MARKER)
        .is_some_and(|rest| rest.starts_with(b"\n") || rest.starts_with(b"\r\n"));
    ensure!(owned, "not a Sich extension file: {}", path.display());
    Ok(Some(bytes))
}

fn remove_owned(path: &Path) -> anyhow::Result<()> {
    if read_owned(path)?.is_some() {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn install(agent_dir: &Path, receipt: &Path) -> anyhow::Result<()> {
    ensure!(agent_dir.is_absolute(), "omp config path did not return an absolute directory");
    let dir = agent_dir.join("extensions");
    let path = dir.join(NAME);
    let current = read_owned(&path)?;
    let previous = recorded_path(receipt)?;
    if previous.as_ref() != Some(&path) {
        if let Some(previous) = previous {
            remove_owned(&previous)?;
        }
        // Record where to uninstall even if omp is later removed or changes profile.
        super::statusline::write_json(receipt, &path)?;
    }
    if current.as_deref() == Some(SOURCE) {
        return Ok(());
    }
    fs::create_dir_all(&dir)?;
    let mut temp = tempfile::NamedTempFile::new_in(&dir)?;
    temp.write_all(SOURCE)?;
    if current.is_some() {
        temp.persist(&path)?;
    } else {
        temp.persist_noclobber(&path)?;
    }
    Ok(())
}

fn uninstall(receipt: &Path) -> anyhow::Result<()> {
    if let Some(path) = recorded_path(receipt)? {
        remove_owned(&path)?;
        fs::remove_file(receipt)?;
    }
    Ok(())
}

/// Reads local installation state only; does not launch omp.
pub fn current(receipt: &Path) -> anyhow::Result<Status> {
    Ok(if installed_extension(receipt)?.is_some() { Status::Installed } else { Status::NotInstalled })
}

/// The installer-owned extension, if still present. Never returns a foreign file.
pub fn installed_extension(receipt: &Path) -> anyhow::Result<Option<PathBuf>> {
    let Some(path) = recorded_path(receipt)? else { return Ok(None) };
    Ok(read_owned(&path)?.is_some().then_some(path))
}

/// Probes the official CLI without opening a session. `None` means no recognized omp binary.
pub async fn supports_hooks() -> anyhow::Result<Option<bool>> {
    Ok(query(&["--version"]).await?.as_deref().and_then(check_version))
}

/// Resolves the active profile through the documented CLI. Disabling needs no omp binary.
pub fn configure(enable: bool, receipt: &Path) -> anyhow::Result<Status> {
    if !enable {
        uninstall(receipt)?;
        return Ok(Status::NotInstalled);
    }
    tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(async {
        match supports_hooks().await? {
            None => return Ok(Status::NotFound),
            Some(false) => return Ok(Status::TooOld),
            Some(true) => {}
        }
        let dir = query(&["config", "path"]).await?.context("cannot resolve omp's user extension directory")?;
        install(Path::new(dir.trim_end_matches(['\r', '\n'])), receipt)?;
        Ok(Status::Installed)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_supported_omp_versions_only() {
        assert_eq!(check_version("omp/19.0.0\r\n"), Some(false));
        assert_eq!(check_version("omp/19.0.1\r\n"), Some(true));
        assert_eq!(check_version("omp/20.0.0"), Some(true));
        assert_eq!(check_version("pi/20.0.0"), None);
        assert_eq!(check_version("omp/19.0.beta"), None);
    }

    #[test]
    fn installation_follows_profile_and_leaves_other_files_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let receipt = dir.path().join("settings/omp-hook.json");
        let agent = dir.path().join("old profile/agent");
        let other = agent.join("extensions/other.js");
        fs::create_dir_all(other.parent().unwrap()).unwrap();
        fs::write(&other, "user extension").unwrap();
        assert_eq!(current(&receipt).unwrap(), Status::NotInstalled);
        install(&agent, &receipt).unwrap();
        install(&agent, &receipt).unwrap();
        assert_eq!(current(&receipt).unwrap(), Status::Installed);
        let old = other.with_file_name(NAME);
        assert_eq!(fs::read(&old).unwrap(), SOURCE);
        let new = dir.path().join("new profile/agent");
        install(&new, &receipt).unwrap();
        assert!(!old.exists());
        let installed = new.join("extensions").join(NAME);
        assert_eq!(fs::read(&installed).unwrap(), SOURCE);
        assert_eq!(configure(false, &receipt).unwrap(), Status::NotInstalled);
        assert!(!installed.exists());
        assert_eq!(fs::read_to_string(other).unwrap(), "user extension");
    }

    #[test]
    fn upgrades_and_removes_managed_extensions_with_windows_line_endings() {
        let dir = tempfile::tempdir().unwrap();
        let receipt = dir.path().join("receipt.json");
        let path = dir.path().join("extensions").join(NAME);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let old = b"// @sich-managed-omp-hook\r\n// previous version\r\n";
        fs::write(&path, old).unwrap();
        install(dir.path(), &receipt).unwrap();
        assert_eq!(fs::read(&path).unwrap(), SOURCE);
        fs::write(&path, old).unwrap();
        assert_eq!(current(&receipt).unwrap(), Status::Installed);
        uninstall(&receipt).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn unowned_files_are_never_overwritten_or_removed() {
        let dir = tempfile::tempdir().unwrap();
        let receipt = dir.path().join("receipt.json");
        let agent = dir.path().join("agent");
        let path = agent.join("extensions").join(NAME);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "user extension").unwrap();
        assert!(install(&agent, &receipt).is_err());
        assert!(!receipt.exists());
        super::super::statusline::write_json(&receipt, &path).unwrap();
        assert!(uninstall(&receipt).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "user extension");
    }
}
