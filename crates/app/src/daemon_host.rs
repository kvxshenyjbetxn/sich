//! Starts the daemon from a private copy so it does not hold the UI executable open.

use std::fs::{self, File};
use std::io::{self, BufRead as _, BufReader};
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

const EXE: &str = if cfg!(windows) { "sich.exe" } else { "sich" };

fn host_root(data_dir: &Path) -> PathBuf {
    data_dir.join("sich").join("daemon-host")
}

fn executable(root: &Path, build_id: &str) -> PathBuf {
    root.join(build_id).join(EXE)
}

/// Env var carrying the user-facing exe path from the launcher to the relocated GUI copy.
pub const LAUNCHER_ENV: &str = "SICH_LAUNCHER_EXE";
/// Env var that disables GUI relocation (run in place).
pub const NO_RELOCATE_ENV: &str = "SICH_NO_RELOCATE";

/// The user-facing executable: `SICH_LAUNCHER_EXE` when set (the GUI runs from a relocated copy),
/// else the current exe. Use this for anything the user or other programs should launch
/// (hook commands, tray restart, messages).
pub fn launcher_exe() -> Option<PathBuf> {
    resolve_launcher(std::env::var_os(LAUNCHER_ENV), std::env::current_exe().ok())
}

fn resolve_launcher(env: Option<std::ffi::OsString>, current: Option<PathBuf>) -> Option<PathBuf> {
    match env.filter(|v| !v.is_empty()) {
        // Never hand out a `\\?\` verbatim path: shells (Git Bash runs agent hooks) can't run it.
        Some(v) => Some(dunce::simplified(Path::new(&v)).to_path_buf()),
        None => current.map(|p| dunce::simplified(&p).to_path_buf()),
    }
}

/// Only the plain GUI start (optionally `--takeover`) is relocated; every other subcommand runs
/// in place.
fn is_gui_args(args: &[String]) -> bool {
    matches!(args.iter().map(String::as_str).collect::<Vec<_>>().as_slice(), [] | ["--takeover"])
}

fn same_path(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| {
        let s = p.to_string_lossy().into_owned();
        if cfg!(windows) { s.to_lowercase() } else { s }
    };
    norm(a) == norm(b)
}

/// Pure decision: should this process hand over to the relocated copy?
fn should_relocate_gui(
    args: &[String],
    no_relocate: bool,
    current: &Path,
    host: &Path,
    launcher_env: Option<&Path>,
) -> bool {
    if no_relocate || !is_gui_args(args) || same_path(current, host) {
        return false;
    }
    // A launcher pointing elsewhere means we are already the spawned copy (loop guard).
    !launcher_env.is_some_and(|l| !same_path(l, current))
}

/// Arguments for the relocated copy: the original ones, unchanged.
fn gui_spawn_args(args: &[String]) -> Vec<String> {
    args.to_vec()
}

fn canonical_or(path: &Path) -> PathBuf {
    dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Launcher step for the GUI: copies this build to the host dir, spawns it with the same args,
/// cwd and env plus `SICH_LAUNCHER_EXE`, and returns true when the caller should exit. Returns
/// false (running in place) when not applicable or on any failure.
pub fn relocate_gui(args: &[String]) -> bool {
    let no_relocate = std::env::var_os(NO_RELOCATE_ENV).is_some_and(|v| !v.is_empty() && v != "0");
    if no_relocate || !is_gui_args(args) {
        return false;
    }
    let Ok(current) = std::env::current_exe() else {
        return false;
    };
    let Some(data_dir) = sich_daemon::data_local_dir() else {
        return false;
    };
    let host = executable(&host_root(&data_dir), sich_app::build_id());
    let current = canonical_or(&current);
    let launcher_env = std::env::var_os(LAUNCHER_ENV).filter(|v| !v.is_empty()).map(PathBuf::from);
    if !should_relocate_gui(args, no_relocate, &current, &canonical_or(&host), launcher_env.as_deref()) {
        return false;
    }
    let spawned = relocate(&current, &host_root(&data_dir), sich_app::build_id()).and_then(|copy| {
        let mut cmd = std::process::Command::new(copy);
        cmd.args(gui_spawn_args(args)).env(LAUNCHER_ENV, &current);
        cmd.spawn().map(|_| ())
    });
    match spawned {
        Ok(()) => true,
        Err(err) => {
            eprintln!("warning: could not relocate the GUI executable, running in place: {err}");
            false
        }
    }
}

/// The daemon must run from the copy matching the GUI's build id. The GUI itself normally is
/// that copy, so relocating from `current_exe()` reuses it (identical bytes); when the GUI runs
/// in place (`SICH_NO_RELOCATE` or a failed relocation) this copies the in-place exe.
pub fn spawn_path(source: &Path) -> PathBuf {
    select_path(source, || {
        let data_dir = sich_daemon::data_local_dir().context("no local data dir")?;
        Ok(relocate(
            source,
            &host_root(&data_dir),
            sich_app::build_id(),
        )?)
    })
}

fn select_path(source: &Path, copy: impl FnOnce() -> anyhow::Result<PathBuf>) -> PathBuf {
    copy().unwrap_or_else(|err| {
        tracing::warn!("could not relocate daemon executable: {err:#}");
        source.to_path_buf()
    })
}

fn same_contents(a: &Path, b: &Path) -> io::Result<bool> {
    let (a, b) = (File::open(a)?, File::open(b)?);
    if a.metadata()?.len() != b.metadata()?.len() {
        return Ok(false);
    }
    let (mut a, mut b) = (BufReader::new(a), BufReader::new(b));
    loop {
        let (left, right) = (a.fill_buf()?, b.fill_buf()?);
        let count = left.len().min(right.len());
        if count == 0 {
            return Ok(left.is_empty() && right.is_empty());
        }
        if left[..count] != right[..count] {
            return Ok(false);
        }
        a.consume(count);
        b.consume(count);
    }
}

fn relocate(source: &Path, root: &Path, build_id: &str) -> io::Result<PathBuf> {
    let target = executable(root, build_id);
    if same_contents(source, &target).unwrap_or(false) {
        return Ok(target);
    }
    let dir = target.parent().expect("daemon copy has a parent");
    fs::create_dir_all(dir)?;
    let mut src = File::open(source)?;
    let mut temp = tempfile::NamedTempFile::new_in(dir)?;
    io::copy(&mut src, temp.as_file_mut())?;
    temp.as_file()
        .set_permissions(src.metadata()?.permissions())?;
    match temp.persist_noclobber(&target) {
        Ok(_) => Ok(target),
        Err(err) if same_contents(source, &target).unwrap_or(false) => {
            // Another launch published the same build while the copy was being staged.
            drop(err);
            Ok(target)
        }
        Err(err) => Err(err.error),
    }
}

pub fn prune_old_hosts() {
    let Some(dir) = sich_daemon::data_local_dir() else {
        return;
    };
    let mut processes = System::new();
    processes.refresh_processes_specifics(
        ProcessesToUpdate::All,
        ProcessRefreshKind::new().with_exe(UpdateKind::Always),
    );
    prune_hosts(&host_root(&dir), sich_app::build_id(), |exe| {
        let canonical = exe.canonicalize().ok();
        processes
            .processes()
            .values()
            .filter_map(|p| p.exe())
            .any(|path| {
                path.eq(exe)
                    || canonical
                        .as_ref()
                        .is_some_and(|exe| path.canonicalize().as_ref().ok() == Some(exe))
            })
    });
}

fn prune_hosts(root: &Path, current: &str, in_use: impl Fn(&Path) -> bool) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name() == current || !entry.file_type().is_ok_and(|ty| ty.is_dir()) {
            continue;
        }
        let exe = entry.path().join(EXE);
        if in_use(&exe) {
            continue;
        }
        match fs::remove_file(&exe) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => continue,
            _ => {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn relocation_decision() {
        let cur = Path::new("C:/repo/target/release/sich.exe");
        let host = Path::new("C:/data/sich/daemon-host/b/sich.exe");
        assert!(should_relocate_gui(&[], false, cur, host, None));
        assert!(should_relocate_gui(&s(&["--takeover"]), false, cur, host, None));
        // Subcommands and unknown args run in place.
        for a in [&["daemon"][..], &["daemon", "stop"], &["hook", "claude"], &["--bogus"]] {
            assert!(!should_relocate_gui(&s(a), false, cur, host, None));
        }
        assert!(!should_relocate_gui(&[], true, cur, host, None));
        // Already the host copy.
        assert!(!should_relocate_gui(&[], false, host, host, None));
        if cfg!(windows) {
            let upper = Path::new("C:/DATA/Sich/daemon-host/B/SICH.EXE");
            assert!(!should_relocate_gui(&[], false, upper, host, None));
        }
        // Launcher env pointing elsewhere: already a spawned copy.
        assert!(!should_relocate_gui(&[], false, cur, host, Some(Path::new("D:/other/sich.exe"))));
        // Launcher env equal to ourselves does not block relocation.
        assert!(should_relocate_gui(&[], false, cur, host, Some(cur)));
    }

    #[test]
    fn launcher_resolution_and_args() {
        let cur = Some(PathBuf::from("cur"));
        assert_eq!(resolve_launcher(Some("l".into()), cur.clone()), Some(PathBuf::from("l")));
        assert_eq!(resolve_launcher(Some("".into()), cur.clone()), cur);
        assert_eq!(resolve_launcher(None, cur.clone()), cur);
        assert_eq!(resolve_launcher(None, None), None);
        // Regression: a `\\?\` verbatim path broke hook commands run by Git Bash.
        #[cfg(windows)]
        {
            let plain = Some(PathBuf::from(r"E:\dev\sich.exe"));
            assert_eq!(resolve_launcher(Some(r"\\?\E:\dev\sich.exe".into()), None), plain);
            assert_eq!(resolve_launcher(None, Some(PathBuf::from(r"\\?\E:\dev\sich.exe"))), plain);
        }
        assert_eq!(gui_spawn_args(&s(&["--takeover"])), s(&["--takeover"]));
        assert!(gui_spawn_args(&[]).is_empty());
    }

    #[test]
    fn name_and_path_follow_the_build_id() {
        let dir = Path::new("local");
        assert_eq!(
            executable(&host_root(dir), "0.1.0-abcdef-123"),
            dir.join("sich")
                .join("daemon-host")
                .join("0.1.0-abcdef-123")
                .join(EXE),
        );
        assert!(
            sich_app::build_id()
                .rsplit_once('-')
                .unwrap()
                .1
                .parse::<u64>()
                .is_ok()
        );
    }

    #[test]
    fn publishes_and_reuses_identical_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        fs::write(&source, b"new build").unwrap();
        let root = host_root(dir.path());
        let target = relocate(&source, &root, "build").unwrap();
        let original = fs::metadata(&target).unwrap().modified().unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"new build");
        assert_eq!(relocate(&source, &root, "build").unwrap(), target);
        assert_eq!(fs::metadata(target).unwrap().modified().unwrap(), original);
        assert_eq!(fs::read_dir(root.join("build")).unwrap().count(), 1);
    }

    #[test]
    fn failed_copy_falls_back_without_replacing_another_build() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let root = host_root(dir.path());
        fs::write(&source, b"first").unwrap();
        let target = relocate(&source, &root, "build").unwrap();
        fs::write(&source, b"second").unwrap();
        assert_eq!(
            select_path(&source, || Ok(relocate(&source, &root, "build")?)),
            source
        );
        assert_eq!(fs::read(&target).unwrap(), b"first");
        assert_eq!(fs::read_dir(target.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn pruning_keeps_current_and_running_builds() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["current", "running", "old"] {
            let target = executable(dir.path(), name);
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            fs::write(target, b"exe").unwrap();
        }
        prune_hosts(dir.path(), "current", |exe| {
            exe == executable(dir.path(), "running")
        });
        assert!(executable(dir.path(), "current").exists());
        assert!(executable(dir.path(), "running").exists());
        assert!(!dir.path().join("old").exists());
    }
}
