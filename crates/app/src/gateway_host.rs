//! Stable executable path for the phone gateway: `<data>/sich/gateway-host/sich.exe`.
//!
//! The gateway listens on UDP, and Windows Firewall asks "allow sich.exe" once per program
//! path. A per-build path would prompt after every update, so the gateway always runs from this
//! one copy, which also lets a program-based firewall rule match. The copy is refreshed only
//! while no gateway runs from it (a running exe is locked).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

const EXE: &str = if cfg!(windows) { "sich.exe" } else { "sich" };
const STAGING: &str = if cfg!(windows) { "sich.exe.new" } else { "sich.new" };
/// Rename attempts while a just-stopped gateway still holds the file.
const RENAME_TRIES: u32 = 50;
const RENAME_DELAY: Duration = Duration::from_millis(100);

pub fn host_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("sich").join("gateway-host")
}

pub fn stable_exe(data_dir: &Path) -> PathBuf {
    host_dir(data_dir).join(EXE)
}

/// Stable path under the real (or `SICH_DATA_DIR`) data dir.
pub fn stable_exe_path() -> Option<PathBuf> {
    sich_daemon::data_local_dir().map(|d| stable_exe(&d))
}

/// Where the gateway should run from: the stable copy, or `None` to keep the legacy behavior
/// (per-build `daemon-host` copy) when relocation is disabled or there is no data dir.
pub fn plan(no_relocate: bool, data_dir: Option<&Path>) -> Option<PathBuf> {
    if no_relocate { None } else { data_dir.map(stable_exe) }
}

pub fn no_relocate() -> bool {
    std::env::var_os(crate::daemon_host::NO_RELOCATE_ENV).is_some_and(|v| !v.is_empty() && v != "0")
}

/// Pure copy decision: copy unless the target has the same size and (checked lazily) bytes.
pub fn needs_copy(source_len: u64, target_len: Option<u64>, same_bytes: impl FnOnce() -> bool) -> bool {
    match target_len {
        Some(len) if len == source_len => !same_bytes(),
        _ => true,
    }
}

/// Publishes `source` as `target` (staged as `sich.exe.new`, then renamed over). Blocking: retries
/// the rename for ~5 s because the previous gateway may still be exiting. Skips identical copies.
pub fn install(source: &Path, target: &Path) -> io::Result<()> {
    install_with(source, target, RENAME_TRIES, RENAME_DELAY)
}

fn install_with(source: &Path, target: &Path, tries: u32, delay: Duration) -> io::Result<()> {
    let source_len = fs::metadata(source)?.len();
    let target_len = fs::metadata(target).ok().map(|m| m.len());
    if !needs_copy(source_len, target_len, || crate::daemon_host::same_contents(source, target).unwrap_or(false)) {
        return Ok(());
    }
    let dir = target.parent().ok_or_else(|| io::Error::other("gateway target has no parent"))?;
    fs::create_dir_all(dir)?;
    let staging = dir.join(STAGING);
    fs::copy(source, &staging)?;
    let mut last = None;
    for attempt in 0..tries.max(1) {
        if attempt > 0 {
            std::thread::sleep(delay);
        }
        match fs::rename(&staging, target) {
            Ok(()) => return Ok(()),
            Err(err) => last = Some(err),
        }
    }
    let _ = fs::remove_file(&staging);
    Err(last.expect("at least one attempt"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_path_is_independent_of_the_build() {
        let dir = Path::new("data");
        assert_eq!(stable_exe(dir), dir.join("sich").join("gateway-host").join(EXE));
        assert_ne!(host_dir(dir), dir.join("sich").join("daemon-host"));
    }

    #[test]
    fn plan_uses_the_stable_path_unless_relocation_is_off() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(plan(false, Some(dir.path())), Some(stable_exe(dir.path())));
        assert_eq!(plan(true, Some(dir.path())), None);
        assert_eq!(plan(false, None), None);
    }

    #[test]
    fn copy_decision() {
        assert!(needs_copy(10, None, || unreachable!()));
        assert!(needs_copy(10, Some(9), || unreachable!()));
        assert!(needs_copy(10, Some(10), || false));
        assert!(!needs_copy(10, Some(10), || true));
    }

    #[test]
    fn install_copies_then_skips_identical_and_replaces_changed() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("src.exe");
        let target = stable_exe(dir.path());
        fs::write(&source, b"build one").unwrap();
        install_with(&source, &target, 1, Duration::ZERO).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"build one");
        let modified = fs::metadata(&target).unwrap().modified().unwrap();
        install_with(&source, &target, 1, Duration::ZERO).unwrap();
        assert_eq!(fs::metadata(&target).unwrap().modified().unwrap(), modified);
        // Same size, different bytes: still replaced.
        fs::write(&source, b"build two").unwrap();
        install_with(&source, &target, 1, Duration::ZERO).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"build two");
        // No staging file left behind.
        assert_eq!(fs::read_dir(target.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn install_reports_a_missing_source_and_leaves_the_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = stable_exe(dir.path());
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, b"old").unwrap();
        assert!(install_with(&dir.path().join("missing"), &target, 1, Duration::ZERO).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"old");
    }

    #[test]
    fn daemon_host_pruning_root_does_not_contain_the_gateway_host() {
        let dir = Path::new("data");
        assert!(!stable_exe(dir).starts_with(dir.join("sich").join("daemon-host")));
    }
}
