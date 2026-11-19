//! Bounded, best-effort scrollback snapshots shared by the daemon and the UI.

use std::fs::{self, File};
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use sich_proto::TermId;

pub(crate) const TAIL_BYTES: usize = 512 * 1024;
pub(crate) const MAX_FILES: usize = 128;
const MAX_BYTES: u64 = (TAIL_BYTES * MAX_FILES) as u64;
const MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

pub fn directory() -> Option<PathBuf> {
    Some(crate::config_dir()?.join("sich").join("scrollback"))
}

fn file_name(id: TermId, started_ms: u64) -> String {
    format!("{id}-{started_ms}.bin")
}

fn snapshot_path(dir: &Path, name: &str) -> Option<PathBuf> {
    let (id, ms) = name.strip_suffix(".bin")?.split_once('-')?;
    (file_name(id.parse().ok()?, ms.parse().ok()?) == name).then(|| dir.join(name))
}

pub(crate) fn save(dir: &Path, id: TermId, started_ms: u64, bytes: &[u8]) -> io::Result<String> {
    let name = file_name(id, started_ms);
    fs::create_dir_all(dir)?;
    let mut tmp = tempfile::Builder::new().prefix(".sich-").tempfile_in(dir)?;
    tmp.write_all(&bytes[bytes.len().saturating_sub(TAIL_BYTES)..])?;
    tmp.persist(dir.join(&name))?;
    Ok(name)
}

/// Loads a snapshot without consuming it. The caller removes it once replay succeeds.
pub fn load(dir: &Path, name: &str) -> io::Result<Vec<u8>> {
    let path = snapshot_path(dir, name).ok_or(io::ErrorKind::InvalidInput)?;
    if !fs::symlink_metadata(&path)?.is_file() {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let mut bytes = Vec::new();
    File::open(path)?
        .take((TAIL_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > TAIL_BYTES {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(bytes)
}

pub fn remove(dir: &Path, name: &str) -> io::Result<()> {
    fs::remove_file(snapshot_path(dir, name).ok_or(io::ErrorKind::InvalidInput)?)
}

pub fn prune(dir: &Path, now: SystemTime) {
    prune_with_limits(dir, now, MAX_BYTES, MAX_FILES);
}

fn prune_with_limits(dir: &Path, now: SystemTime, max_bytes: u64, max_files: usize) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut snapshots: Vec<_> = entries
        .flatten()
        .filter_map(|entry| {
            snapshot_path(dir, entry.file_name().to_str()?)?;
            let meta = entry.metadata().ok()?;
            meta.is_file()
                .then_some((meta.modified().ok()?, meta.len(), entry.path()))
        })
        .collect();
    snapshots.sort_unstable_by(|a, b| b.cmp(a));
    let (mut total, mut count) = (0_u64, 0);
    for (modified, len, path) in snapshots {
        let expired = now.duration_since(modified).is_ok_and(|age| age > MAX_AGE);
        if expired
            || len > TAIL_BYTES as u64
            || count >= max_files
            || total.saturating_add(len) > max_bytes
        {
            let _ = fs::remove_file(path);
        } else {
            count += 1;
            total += len;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_and_load_only_the_tail_then_consume_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut bytes = vec![0xFF; TAIL_BYTES + 30];
        bytes[..30].fill(0);
        let name = save(dir.path(), 5, 123, &bytes).unwrap();
        assert_eq!(name, "5-123.bin");
        assert_eq!(load(dir.path(), &name).unwrap(), vec![0xFF; TAIL_BYTES]);
        assert_eq!(save(dir.path(), 5, 123, b"updated").unwrap(), name);
        assert_eq!(load(dir.path(), &name).unwrap(), b"updated");
        remove(dir.path(), &name).unwrap();
        assert_eq!(
            load(dir.path(), &name).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn refuses_non_snapshot_paths_and_oversize_files() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "../1-2.bin",
            "1-2.bin/3-4.bin",
            "1-2.bin.txt",
            "+1-2.bin",
            "01-2.bin",
            "1--2.bin",
            "",
        ] {
            assert_eq!(
                load(dir.path(), name).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
            assert_eq!(
                remove(dir.path(), name).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
        fs::write(dir.path().join("1-2.bin"), vec![0; TAIL_BYTES + 1]).unwrap();
        assert_eq!(
            load(dir.path(), "1-2.bin").unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn prunes_oldest_to_age_count_and_size_limits() {
        let dir = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        for (id, age) in [
            (1, MAX_AGE * 2),
            (2, Duration::from_secs(60)),
            (3, Duration::from_secs(30)),
            (4, Duration::ZERO),
        ] {
            let name = save(dir.path(), id, 2, b"out").unwrap();
            File::options()
                .write(true)
                .open(dir.path().join(name))
                .unwrap()
                .set_modified(now - age)
                .unwrap();
        }
        fs::write(dir.path().join("keep"), b"keep").unwrap();
        prune_with_limits(dir.path(), now, 6, 10);
        assert!(!dir.path().join("1-2.bin").exists());
        assert!(!dir.path().join("2-2.bin").exists());
        assert!(dir.path().join("3-2.bin").exists());
        assert!(dir.path().join("4-2.bin").exists());
        prune_with_limits(dir.path(), now, 20, 1);
        assert!(!dir.path().join("3-2.bin").exists());
        prune(dir.path(), now + MAX_AGE * 2);
        assert!(!dir.path().join("4-2.bin").exists());
        assert!(dir.path().join("keep").exists());
    }
}
