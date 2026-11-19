//! Locations and ids. The config dir honors `SICH_CONFIG_DIR` like the rest of the app.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher as _, Hasher as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// `<config>/sich`.
pub fn sich_dir() -> Option<PathBuf> {
    let base = match std::env::var_os("SICH_CONFIG_DIR") {
        Some(d) if !d.is_empty() => Some(PathBuf::from(d)),
        _ => dirs::config_dir(),
    };
    base.map(|d| d.join("sich"))
}

pub fn db_path() -> Option<PathBuf> {
    sich_dir().map(|d| d.join("orchestration.db"))
}

pub fn tasks_dir() -> Option<PathBuf> {
    sich_dir().map(|d| d.join("orch").join("tasks"))
}

pub fn task_file(task_id: &str) -> Option<PathBuf> {
    tasks_dir().map(|d| d.join(format!("{task_id}.md")))
}

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// 8 random-looking hex digits (std's randomly seeded hasher mixed with time, pid and a counter).
fn hex8() -> String {
    let mut h = RandomState::new().build_hasher();
    h.write_u128(SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
    h.write_u32(std::process::id());
    h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    format!("{:08x}", h.finish() as u32)
}

pub fn new_run_id() -> String {
    format!("r_{}", hex8())
}

pub fn new_task_id() -> String {
    format!("t_{}", hex8())
}

pub fn new_dispatch_id() -> String {
    format!("d_{}", hex8())
}

/// True for ids shaped like `<prefix>_<8 hex>`.
pub fn is_id(prefix: char, text: &str) -> bool {
    let mut chars = text.chars();
    chars.next() == Some(prefix)
        && chars.next() == Some('_')
        && text.len() == 10
        && text[2..].chars().all(|c| c.is_ascii_hexdigit())
}

/// Atomically writes `contents` (tmp file in the same dir + rename).
pub fn write_atomic(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_have_short_readable_shapes_and_are_unique() {
        let (t, d, r) = (new_task_id(), new_dispatch_id(), new_run_id());
        assert!(is_id('t', &t) && is_id('d', &d) && is_id('r', &r), "{t} {d} {r}");
        assert!(!is_id('t', &d));
        assert!(!is_id('t', "t_xyz"));
        let all: std::collections::HashSet<_> = (0..500).map(|_| new_task_id()).collect();
        assert_eq!(all.len(), 500);
    }

    #[test]
    fn write_atomic_creates_parents_and_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a").join("b.md");
        write_atomic(&file, b"one").unwrap();
        write_atomic(&file, b"two").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "two");
    }
}
