//! Images uploaded from the phone: validated by magic bytes and saved as temp files whose paths
//! are pasted into the agent's prompt.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime};

/// Largest accepted upload.
pub const MAX_BYTES: usize = 15 * 1024 * 1024;
/// Saved uploads older than this are removed on start.
pub const MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const PREFIX: &str = "phone-";

/// Same folder as the desktop's pasted images.
pub fn default_dir() -> PathBuf {
    std::env::temp_dir().join("sich-paste")
}

/// The extension for a supported image type, decided by magic bytes only.
pub fn sniff(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("jpg")
    } else if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        Some("gif")
    } else if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        Some("webp")
    } else {
        None
    }
}

pub fn file_name(unix_ms: u64, seq: u32, ext: &str) -> String {
    format!("{PREFIX}{unix_ms}-{seq}.{ext}")
}

/// Validates and saves an image. Returns the path; its file name doubles as the upload id.
pub fn save(dir: &Path, data: &[u8], unix_ms: u64) -> Result<PathBuf, String> {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    if data.is_empty() {
        return Err("empty image".into());
    }
    if data.len() > MAX_BYTES {
        return Err(format!("image is larger than {} MiB", MAX_BYTES / (1024 * 1024)));
    }
    let ext = sniff(data).ok_or("unsupported image type (png, jpeg, webp and gif only)")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let path = dir.join(file_name(unix_ms, SEQ.fetch_add(1, Ordering::Relaxed), ext));
    std::fs::write(&path, data).map_err(|e| format!("cannot save image: {e}"))?;
    Ok(path)
}

/// Deletes our uploads older than `max_age`; returns how many were removed.
pub fn prune(dir: &Path, now: SystemTime, max_age: Duration) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    let mut removed = 0;
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with(PREFIX) {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age > max_age);
        if old && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png() -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
        v.extend_from_slice(&[0; 16]);
        v
    }

    #[test]
    fn sniffs_supported_types_by_magic_bytes() {
        assert_eq!(sniff(&png()), Some("png"));
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0, 0]), Some("jpg"));
        assert_eq!(sniff(b"GIF89a...."), Some("gif"));
        assert_eq!(sniff(b"GIF87a...."), Some("gif"));
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some("webp"));
    }

    #[test]
    fn rejects_other_content() {
        assert_eq!(sniff(b""), None);
        assert_eq!(sniff(b"<svg></svg>"), None);
        assert_eq!(sniff(b"MZ\x90\x00"), None);
        assert_eq!(sniff(b"RIFF\0\0\0\0WAVEfmt "), None);
        assert_eq!(sniff(b"RIFF"), None);
    }

    #[test]
    fn save_writes_a_unique_file_with_the_sniffed_extension() {
        let dir = tempfile::tempdir().unwrap();
        let a = save(dir.path(), &png(), 1234).unwrap();
        let b = save(dir.path(), &png(), 1234).unwrap();
        assert_ne!(a, b);
        let name = a.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("phone-1234-") && name.ends_with(".png"), "{name}");
        assert_eq!(std::fs::read(&a).unwrap(), png());
    }

    #[test]
    fn save_refuses_empty_oversized_and_unknown() {
        let dir = tempfile::tempdir().unwrap();
        assert!(save(dir.path(), b"", 1).is_err());
        assert!(save(dir.path(), b"plain text", 1).is_err());
        let mut big = png();
        big.resize(MAX_BYTES + 1, 0);
        assert!(save(dir.path(), &big, 1).unwrap_err().contains("larger"));
        let mut max = png();
        max.resize(MAX_BYTES, 0);
        assert!(save(dir.path(), &max, 1).is_ok());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn prune_removes_only_old_phone_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("phone-1-0.png"), b"x").unwrap();
        std::fs::write(dir.path().join("paste-1-0.png"), b"x").unwrap();
        // Everything counts as old when "now" is far in the future.
        let future = SystemTime::now() + Duration::from_secs(8 * 24 * 3600);
        assert_eq!(prune(dir.path(), future, MAX_AGE), 1);
        assert!(dir.path().join("paste-1-0.png").exists());
        assert_eq!(prune(dir.path(), SystemTime::now(), MAX_AGE), 0);
        assert_eq!(prune(&dir.path().join("missing"), SystemTime::now(), MAX_AGE), 0);
    }
}
