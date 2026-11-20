//! Content hash of everything that determines daemon behavior. Included by `build.rs` via
//! `#[path]` and (for tests) by `src/lib.rs`, so it must stay dependency-free.
#![allow(dead_code)]

use std::fs;
use std::io;
use std::path::Path;

/// Files/dirs (relative to the workspace root) the daemon's behavior depends on.
/// The daemon links only `sich-proto` from the workspace.
pub const DAEMON_INPUTS: [&str; 4] = [
    "crates/daemon/src",
    "crates/daemon/Cargo.toml",
    "crates/proto/src",
    "crates/proto/Cargo.toml",
    // Not Cargo.lock: it changes whenever any crate in the workspace gains a dependency,
    // which made every UI update look like a daemon update.
];

/// Replaces every CRLF with LF so line-ending churn does not change the hash.
pub fn normalize_crlf(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\r' && bytes.get(i + 1) == Some(&b'\n') {
            i += 1;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

fn fnv1a(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
}

/// 16-hex-char FNV-1a id over `(relative path, contents)` entries. Entries are sorted by path
/// first, contents are CRLF-normalized, and fields are length-prefixed so boundaries matter.
pub fn hash_entries(entries: &[(String, Vec<u8>)]) -> String {
    let mut sorted: Vec<&(String, Vec<u8>)> = entries.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for (path, content) in sorted {
        let content = normalize_crlf(content);
        fnv1a(&mut hash, &(path.len() as u64).to_le_bytes());
        fnv1a(&mut hash, path.as_bytes());
        fnv1a(&mut hash, &(content.len() as u64).to_le_bytes());
        fnv1a(&mut hash, &content);
    }
    format!("{hash:016x}")
}

/// Reads `rel` (file or directory, recursively, in sorted order) under `root` into `out`,
/// labelling entries by their `/`-separated path relative to `root`.
pub fn collect(root: &Path, rel: &str, out: &mut Vec<(String, Vec<u8>)>) -> io::Result<()> {
    let path = root.join(rel);
    if path.is_dir() {
        let mut names: Vec<String> = fs::read_dir(&path)?
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        for name in names {
            collect(root, &format!("{rel}/{name}"), out)?;
        }
    } else {
        out.push((rel.to_string(), fs::read(&path)?));
    }
    Ok(())
}

/// Id of the daemon-relevant sources under the workspace `root`. Missing inputs hash as absent.
pub fn daemon_build_id(root: &Path) -> String {
    let mut entries = Vec::new();
    for rel in DAEMON_INPUTS {
        let _ = collect(root, rel, &mut entries);
    }
    hash_entries(&entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(path: &str, body: &str) -> (String, Vec<u8>) {
        (path.to_string(), body.as_bytes().to_vec())
    }

    #[test]
    fn same_content_same_id_and_16_hex() {
        let a = hash_entries(&[e("a.rs", "fn a() {}"), e("b.rs", "fn b() {}")]);
        let b = hash_entries(&[e("a.rs", "fn a() {}"), e("b.rs", "fn b() {}")]);
        assert_eq!(a, b);
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn crlf_equals_lf() {
        let lf = hash_entries(&[e("a.rs", "one\ntwo\n")]);
        let crlf = hash_entries(&[e("a.rs", "one\r\ntwo\r\n")]);
        assert_eq!(lf, crlf);
    }

    #[test]
    fn lone_cr_is_not_normalized() {
        assert_ne!(hash_entries(&[e("a", "x\ry")]), hash_entries(&[e("a", "x\ny")]));
    }

    #[test]
    fn changed_byte_or_path_changes_id() {
        let base = hash_entries(&[e("a.rs", "fn a() {}")]);
        assert_ne!(base, hash_entries(&[e("a.rs", "fn a() { }")]));
        assert_ne!(base, hash_entries(&[e("b.rs", "fn a() {}")]));
    }

    #[test]
    fn field_boundaries_matter() {
        assert_ne!(
            hash_entries(&[e("a", "bc")]),
            hash_entries(&[e("ab", "c")])
        );
    }

    #[test]
    fn entry_order_does_not_matter() {
        let a = hash_entries(&[e("a.rs", "1"), e("b.rs", "2")]);
        let b = hash_entries(&[e("b.rs", "2"), e("a.rs", "1")]);
        assert_eq!(a, b);
    }

    #[test]
    fn collect_walks_sorted_and_hash_tracks_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src/sub")).unwrap();
        fs::write(root.join("src/b.rs"), "b\r\n").unwrap();
        fs::write(root.join("src/a.rs"), "a").unwrap();
        fs::write(root.join("src/sub/c.rs"), "c").unwrap();
        let mut out = Vec::new();
        collect(root, "src", &mut out).unwrap();
        let names: Vec<&str> = out.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["src/a.rs", "src/b.rs", "src/sub/c.rs"]);
        let before = hash_entries(&out);
        fs::write(root.join("src/b.rs"), "b\n").unwrap();
        let mut again = Vec::new();
        collect(root, "src", &mut again).unwrap();
        assert_eq!(before, hash_entries(&again));
        fs::write(root.join("src/sub/c.rs"), "C").unwrap();
        let mut changed = Vec::new();
        collect(root, "src", &mut changed).unwrap();
        assert_ne!(before, hash_entries(&changed));
    }

    #[test]
    fn real_workspace_id_is_stable() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        assert_eq!(daemon_build_id(&root), daemon_build_id(&root));
    }
}
