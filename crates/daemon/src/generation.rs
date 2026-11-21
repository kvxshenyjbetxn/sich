//! Discovery and lifetime of daemons that share a base endpoint.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub const RETIRE_CLIENT: &str = "sich-app-retire-when-empty";
pub const IDLE_LIMIT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub pid: u32,
    pub socket: String,
    pub daemon_build_id: String,
    pub proto_version: u32,
    pub started_ms: u64,
    #[serde(default)]
    pub omp_status: bool,
    #[serde(default)]
    pub subagent_activity: bool,
    #[serde(default)]
    pub subagent_models: bool,
}

pub fn socket(base: &str, build_id: &str) -> String {
    format!("{base}-{build_id}")
}

impl Entry {
    pub fn base(&self) -> Option<&str> {
        self.socket.strip_suffix(&format!("-{}", self.daemon_build_id))
    }
}

pub fn directory() -> Option<PathBuf> {
    Some(crate::data_local_dir()?.join("sich").join("daemons"))
}

pub fn entries() -> Vec<Entry> {
    directory()
        .map(|dir| read(&dir, pid_alive))
        .transpose()
        .unwrap_or_else(|err| {
            tracing::warn!("could not read daemon registry: {err}");
            None
        })
        .unwrap_or_default()
}

/// Terminals inherit the exact endpoint. Resolve it to its base using the registry so a
/// legacy base that happens to end in a build-like suffix stays unambiguous.
pub fn base_for<'a>(endpoint: &'a str, entries: &[Entry]) -> &'a str {
    entries.iter().find(|entry| entry.socket == endpoint)
        .and_then(|entry| endpoint.strip_suffix(&format!("-{}", entry.daemon_build_id)))
        .unwrap_or(endpoint)
}

pub fn newest_compatible<'a>(base: &str, entries: &'a [Entry]) -> Option<&'a Entry> {
    entries.iter()
        .filter(|entry| entry.base() == Some(base) && entry.proto_version == sich_proto::PROTO_VERSION)
        .max_by_key(|entry| (entry.started_ms, &entry.daemon_build_id))
}

pub fn should_exit_idle(current: &Entry, entries: &[Entry], idle: Option<Duration>) -> bool {
    idle.is_some_and(|idle| idle >= IDLE_LIMIT)
        && entries.iter().any(|other| {
            other.base() == current.base()
                && other.proto_version == current.proto_version
                && (other.started_ms, &other.daemon_build_id) > (current.started_ms, &current.daemon_build_id)
        })
}

fn lock(dir: &Path) -> io::Result<File> {
    let file = OpenOptions::new().create(true).truncate(false).read(true).write(true)
        .open(dir.join(".lock"))?;
    file.lock()?;
    Ok(file)
}

fn read(dir: &Path, is_alive: impl Fn(u32) -> bool) -> io::Result<Vec<Entry>> {
    // Hold the lock from read to prune so a replacement entry cannot be removed.
    let _lock = match lock(dir) {
        Ok(lock) => lock,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    let mut result = Vec::new();
    for item in fs::read_dir(dir)?.filter_map(Result::ok) {
        let path = item.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Ok(entry) = read_entry(&path) else { continue };
        if is_alive(entry.pid) {
            result.push(entry);
        } else if let Err(err) = fs::remove_file(&path) {
            tracing::warn!("could not prune daemon registry entry {}: {err}", path.display());
        }
    }
    result.sort_by(|a, b| a.socket.cmp(&b.socket));
    Ok(result)
}

fn read_entry(path: &Path) -> io::Result<Entry> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

pub(crate) struct Registration {
    entry: Entry,
    path: PathBuf,
}

impl Registration {
    pub(crate) fn write(dir: &Path, entry: Entry) -> io::Result<Self> {
        let id = &entry.daemon_build_id;
        if id.is_empty() || !id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_') {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid daemon build id"));
        }
        fs::create_dir_all(dir)?;
        let _lock = lock(dir)?;
        let path = dir.join(format!("{id}.json"));
        let mut temp = tempfile::NamedTempFile::new_in(dir)?;
        temp.write_all(&serde_json::to_vec(&entry)?)?;
        temp.persist(&path)?;
        Ok(Self { entry, path })
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        let Some(dir) = self.path.parent() else { return };
        let Ok(_lock) = lock(dir) else { return };
        if read_entry(&self.path).is_ok_and(|entry| entry == self.entry) {
            if let Err(err) = fs::remove_file(&self.path) {
                tracing::warn!("could not remove daemon registry entry {}: {err}", self.path.display());
            }
        }
    }
}

#[cfg(windows)]
fn pid_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject};
    const SYNCHRONIZE: u32 = 0x0010_0000;
    if pid == 0 {
        return false;
    }
    // Waiting on a process handle observes exit even if another process still holds it open.
    unsafe {
        let handle = OpenProcess(SYNCHRONIZE, 0, pid);
        if handle.is_null() {
            return io::Error::last_os_error().raw_os_error() != Some(ERROR_INVALID_PARAMETER as i32);
        }
        let result = WaitForSingleObject(handle, 0);
        CloseHandle(handle);
        result == WAIT_TIMEOUT || result == u32::MAX
    }
}

#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else { return false };
    pid > 0 && (unsafe { libc::kill(pid, 0) } == 0
        || io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn generation(base: &str, build: &str, pid: u32, started_ms: u64) -> Entry {
        Entry { pid, socket: socket(base, build), daemon_build_id: build.into(), proto_version: 7, started_ms, omp_status: true, subagent_activity: true, subagent_models: true }
    }

    #[test]
    fn generation_suffix_preserves_the_base() {
        let entry = generation("test-pipe-abcd", "a12b", 1, 0);
        assert_eq!(entry.socket, "test-pipe-abcd-a12b");
        assert_eq!(entry.base(), Some("test-pipe-abcd"));
        assert!(crate::ipc::to_name(&entry.socket).is_ok());
    }

    #[test]
    fn writes_reads_prunes_and_unregisters() {
        let dir = tempfile::tempdir().unwrap();
        let live = generation("isolated", "live", std::process::id(), 1);
        let dead = generation("isolated", "dead", 0, 0);
        let registered = Registration::write(dir.path(), live.clone()).unwrap();
        let _dead = Registration::write(dir.path(), dead).unwrap();
        assert_eq!(read_entry(&dir.path().join("live.json")).unwrap(), live);
        assert!(dir.path().join("dead.json").exists());
        assert_eq!(read(dir.path(), pid_alive).unwrap(), vec![live]);
        assert!(!dir.path().join("dead.json").exists());
        drop(registered);
        assert!(read(dir.path(), pid_alive).unwrap().is_empty());
    }

    #[test]
    fn idle_exit_requires_thirty_seconds_empty_and_a_newer_compatible_peer() {
        let old = generation("isolated", "old", 1, 0);
        let newest = generation("isolated", "new", 2, 1);
        let other_base = generation("other", "newer", 3, 2);
        let other_proto = Entry { proto_version: 8, ..generation("isolated", "future", 4, 3) };
        assert!(!should_exit_idle(&old, &[other_base, other_proto], Some(IDLE_LIMIT)));
        assert!(!should_exit_idle(&old, std::slice::from_ref(&newest), None));
        assert!(!should_exit_idle(&old, std::slice::from_ref(&newest), Some(IDLE_LIMIT - Duration::from_millis(1))));
        assert!(should_exit_idle(&old, std::slice::from_ref(&newest), Some(IDLE_LIMIT)));
        assert!(!should_exit_idle(&newest, &[old], Some(IDLE_LIMIT)));
    }
}

#[cfg(test)]
mod selection_tests {
    use super::*;

    fn generation(base: &str, build: &str, started_ms: u64, proto_version: u32) -> Entry {
        Entry { pid: 1, socket: socket(base, build), daemon_build_id: build.into(), proto_version, started_ms, omp_status: true, subagent_activity: true, subagent_models: true }
    }

    #[test]
    fn old_v7_registry_entries_do_not_advertise_new_capabilities() {
        let entry: Entry = serde_json::from_str(
            r#"{"pid":1,"socket":"sich-before","daemon_build_id":"before","proto_version":7,"started_ms":1,"omp_status":true}"#,
        ).unwrap();
        assert!(entry.omp_status);
        assert!(!entry.subagent_activity);
        assert!(!entry.subagent_models);
    }

    #[test]
    fn newest_compatible_prefers_the_latest_start_of_the_same_base_and_protocol() {
        let entries = [
            generation("b", "old", 1, sich_proto::PROTO_VERSION),
            generation("b", "new", 5, sich_proto::PROTO_VERSION),
            generation("b", "future", 9, sich_proto::PROTO_VERSION + 1),
            generation("other", "newest", 99, sich_proto::PROTO_VERSION),
        ];
        assert_eq!(newest_compatible("b", &entries).map(|e| e.daemon_build_id.as_str()), Some("new"));
        assert!(newest_compatible("missing", &entries).is_none());
    }

    #[test]
    fn base_for_resolves_registered_generations_and_leaves_legacy_names_alone() {
        let entries = [generation("sich-daemon", "abc123", 1, sich_proto::PROTO_VERSION)];
        assert_eq!(base_for("sich-daemon-abc123", &entries), "sich-daemon");
        assert_eq!(base_for("sich-daemon", &entries), "sich-daemon");
        assert_eq!(base_for("custom-pipe-abc123", &entries), "custom-pipe-abc123");
    }

    #[test]
    fn rejects_unsafe_build_ids_in_registry_file_names() {
        let dir = tempfile::tempdir().unwrap();
        for id in ["", "../x", "a/b", "a b"] {
            assert!(Registration::write(dir.path(), generation("b", id, 0, 7)).is_err(), "{id:?}");
        }
    }

    #[test]
    fn unreadable_registry_files_are_skipped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let mut live = generation("b", "live", 1, 7);
        live.pid = std::process::id();
        let _live = Registration::write(dir.path(), live).unwrap();
        fs::write(dir.path().join("garbage.json"), b"{not json").unwrap();
        fs::write(dir.path().join("note.txt"), b"ignored").unwrap();
        let found = read(dir.path(), pid_alive).unwrap();
        assert_eq!(found.len(), 1);
    }
}
