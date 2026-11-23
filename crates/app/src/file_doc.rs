//! File I/O and the pure decisions behind the file tab (`file_view`): size limit, line endings,
//! BOM, atomic save, dirty and reload rules. No UI here, so all of it is unit-tested.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Larger files are not opened in the editor.
pub(crate) const MAX_BYTES: u64 = 2 * 1024 * 1024;
const BOM: &[u8] = b"\xEF\xBB\xBF";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Eol {
    Lf,
    Crlf,
}

/// A loaded file: text with `\n` line breaks (the editor's form) plus what is needed to write it back unchanged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Loaded {
    pub text: String,
    pub eol: Eol,
    pub bom: bool,
    pub stamp: Stamp,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LoadError {
    TooLarge,
    NotUtf8,
    Io(String),
}

/// Modification time and length: enough to notice that a file changed on disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Stamp {
    pub modified: Option<SystemTime>,
    pub len: u64,
}

pub(crate) fn stamp_of(path: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some(Stamp { modified: meta.modified().ok(), len: meta.len() })
}

/// Whether a file of `len` bytes may be opened in the editor.
pub(crate) fn fits(len: u64) -> bool {
    len <= MAX_BYTES
}

/// CRLF when the file's breaks are mostly CRLF, LF otherwise (also for files without breaks).
pub(crate) fn detect_eol(text: &str) -> Eol {
    let crlf = text.matches("\r\n").count();
    let lf = text.matches('\n').count() - crlf;
    if crlf > 0 && crlf >= lf { Eol::Crlf } else { Eol::Lf }
}

/// Decodes file bytes. A leading BOM is remembered and stripped.
pub(crate) fn decode(bytes: &[u8]) -> Result<(String, Eol, bool), LoadError> {
    let (body, bom) = match bytes.strip_prefix(BOM) {
        Some(rest) => (rest, true),
        None => (bytes, false),
    };
    let text = std::str::from_utf8(body).map_err(|_| LoadError::NotUtf8)?;
    let eol = detect_eol(text);
    Ok((text.replace("\r\n", "\n"), eol, bom))
}

/// The bytes to write: the editor text with the original line endings and BOM.
pub(crate) fn encode(text: &str, eol: Eol, bom: bool) -> Vec<u8> {
    let unified = text.replace("\r\n", "\n");
    let body = match eol {
        Eol::Lf => unified,
        Eol::Crlf => unified.replace('\n', "\r\n"),
    };
    let mut out = Vec::with_capacity(body.len() + 3);
    if bom {
        out.extend_from_slice(BOM);
    }
    out.extend_from_slice(body.as_bytes());
    out
}

pub(crate) fn load(path: &Path) -> Result<Loaded, LoadError> {
    use std::io::Read as _;
    let io = |e: std::io::Error| LoadError::Io(e.to_string());
    let file = std::fs::File::open(path).map_err(io)?;
    let meta = file.metadata().map_err(io)?;
    if !fits(meta.len()) {
        return Err(LoadError::TooLarge);
    }
    // The limit is applied while reading too: the file may grow after the metadata call.
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes).map_err(io)?;
    if !fits(bytes.len() as u64) {
        return Err(LoadError::TooLarge);
    }
    let (text, eol, bom) = decode(&bytes)?;
    Ok(Loaded { text, eol, bom, stamp: Stamp { modified: meta.modified().ok(), len: meta.len() } })
}

/// Writes `bytes` to a temp file next to `path`, then renames it over the target.
pub(crate) fn save_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    // Write through a symlink instead of replacing it.
    let target = dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let dir = target.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into());
    let tmp = dir.join(format!(".{name}.sich-{}.tmp", std::process::id()));
    let result = (|| {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        if let Ok(meta) = std::fs::metadata(&target) {
            let _ = std::fs::set_permissions(&tmp, meta.permissions());
        }
        std::fs::rename(&tmp, &target)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// What the periodic disk check should do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DiskAction {
    Nothing,
    /// Not edited: take the new content silently.
    Reload,
    /// Edited: keep the user's text and offer a reload.
    Notice,
}

pub(crate) fn disk_action(saved: Option<Stamp>, now: Option<Stamp>, dirty: bool) -> DiskAction {
    match (saved, now) {
        (Some(saved), Some(now)) if saved != now => {
            if dirty { DiskAction::Notice } else { DiskAction::Reload }
        }
        _ => DiskAction::Nothing,
    }
}

/// The editor text differs from what is on disk.
pub(crate) fn is_dirty(current: &str, saved: &str) -> bool {
    current != saved
}

/// Transcripts (`.jsonl`) and files flagged by the caller are never edited here.
pub(crate) fn is_read_only(path: &Path, forced: bool) -> bool {
    forced || path.extension().is_some_and(|e| e.eq_ignore_ascii_case("jsonl"))
}

/// Highlighting language of the editor for a file name.
pub(crate) fn language_for(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "md" | "markdown" => Some("markdown"),
        "json" | "jsonl" => Some("json"),
        _ => None,
    }
}

pub(crate) fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned())
}

/// Cuts the middle of a long path: `C:\Users\…\skills\x\SKILL.md`.
pub(crate) fn middle_ellipsis(text: &str, max_chars: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_chars || max_chars < 3 {
        return text.to_string();
    }
    let keep = max_chars - 1;
    let tail = keep / 2;
    let head = keep - tail;
    format!("{}…{}", chars[..head].iter().collect::<String>(), chars[chars.len() - tail..].iter().collect::<String>())
}

/// The key of a file tab: the same file always maps to the same tab.
pub(crate) fn tab_path(path: &Path) -> PathBuf {
    dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(len: u64, secs: u64) -> Stamp {
        Stamp { modified: Some(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs)), len }
    }

    #[test]
    fn eol_detection() {
        assert_eq!(detect_eol(""), Eol::Lf);
        assert_eq!(detect_eol("one line"), Eol::Lf);
        assert_eq!(detect_eol("a\nb\n"), Eol::Lf);
        assert_eq!(detect_eol("a\r\nb\r\n"), Eol::Crlf);
        assert_eq!(detect_eol("a\r\nb\nc\r\n"), Eol::Crlf);
        assert_eq!(detect_eol("a\nb\nc\r\n"), Eol::Lf);
    }

    #[test]
    fn decode_and_encode_round_trip_keeps_endings_and_bom() {
        for (bytes, eol, bom) in [
            (&b"a\nb\n"[..], Eol::Lf, false),
            (&b"a\r\nb\r\n"[..], Eol::Crlf, false),
            (&b"\xEF\xBB\xBFa\r\nb"[..], Eol::Crlf, true),
            (&b"\xEF\xBB\xBFa\nb"[..], Eol::Lf, true),
            (&b""[..], Eol::Lf, false),
        ] {
            let (text, got_eol, got_bom) = decode(bytes).unwrap();
            assert_eq!((got_eol, got_bom), (eol, bom));
            assert!(!text.contains('\r'));
            assert_eq!(encode(&text, got_eol, got_bom), bytes, "{bytes:?}");
        }
    }

    #[test]
    fn encode_applies_the_original_ending_to_edited_text() {
        assert_eq!(encode("x\ny\n", Eol::Crlf, false), b"x\r\ny\r\n");
        assert_eq!(encode("x\r\ny", Eol::Crlf, false), b"x\r\ny");
        assert_eq!(encode("x\r\ny", Eol::Lf, false), b"x\ny");
        assert_eq!(encode("é", Eol::Lf, true), b"\xEF\xBB\xBF\xC3\xA9");
    }

    #[test]
    fn decode_rejects_binary() {
        assert_eq!(decode(b"\xFF\xFE\x00"), Err(LoadError::NotUtf8));
    }

    #[test]
    fn size_limit_decision() {
        assert!(fits(0));
        assert!(fits(MAX_BYTES));
        assert!(!fits(MAX_BYTES + 1));
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big.md");
        std::fs::write(&big, vec![b'a'; MAX_BYTES as usize + 1]).unwrap();
        assert_eq!(load(&big), Err(LoadError::TooLarge));
        let ok = dir.path().join("ok.md");
        std::fs::write(&ok, vec![b'a'; MAX_BYTES as usize]).unwrap();
        assert_eq!(load(&ok).unwrap().text.len(), MAX_BYTES as usize);
        assert!(matches!(load(&dir.path().join("missing.md")), Err(LoadError::Io(_))));
    }

    #[test]
    fn load_reports_text_endings_and_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.md");
        std::fs::write(&path, b"\xEF\xBB\xBFhi\r\nthere\r\n").unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.text, "hi\nthere\n");
        assert_eq!((loaded.eol, loaded.bom), (Eol::Crlf, true));
        assert_eq!(Some(loaded.stamp), stamp_of(&path));
        assert_eq!(stamp_of(&dir.path().join("none")), None);
    }

    #[test]
    fn atomic_save_replaces_content_and_leaves_no_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.md");
        std::fs::write(&path, b"old\r\ncontent\r\n").unwrap();
        let loaded = load(&path).unwrap();
        let edited = format!("{}more\n", loaded.text);
        save_atomic(&path, &encode(&edited, loaded.eol, loaded.bom)).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"old\r\ncontent\r\nmore\r\n");
        let names: Vec<_> = std::fs::read_dir(dir.path()).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(names, [std::ffi::OsString::from("note.md")]);
        // Saving a file that does not exist yet creates it.
        let fresh = dir.path().join("fresh.md");
        save_atomic(&fresh, b"x").unwrap();
        assert_eq!(std::fs::read(&fresh).unwrap(), b"x");
    }

    #[test]
    fn failed_save_cleans_up_and_keeps_the_original() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("a.md");
        // The folder does not exist: the temp file cannot be created.
        assert!(save_atomic(&path, b"x").is_err());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        // A directory in place of the file cannot be replaced; no temp file stays behind.
        let as_dir = dir.path().join("d.md");
        std::fs::create_dir(&as_dir).unwrap();
        std::fs::write(as_dir.join("keep"), "k").unwrap();
        assert!(save_atomic(&as_dir, b"x").is_err());
        assert!(as_dir.join("keep").exists());
        let stray: Vec<_> = std::fs::read_dir(dir.path()).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(stray, [std::ffi::OsString::from("d.md")]);
    }

    #[test]
    fn disk_change_decisions() {
        let a = Some(stamp(10, 1));
        let b = Some(stamp(10, 2));
        assert_eq!(disk_action(a, a, false), DiskAction::Nothing);
        assert_eq!(disk_action(a, a, true), DiskAction::Nothing);
        assert_eq!(disk_action(a, b, false), DiskAction::Reload);
        assert_eq!(disk_action(a, b, true), DiskAction::Notice);
        assert_eq!(disk_action(a, Some(stamp(11, 1)), false), DiskAction::Reload);
        // A vanished file is not a change to act on.
        assert_eq!(disk_action(a, None, false), DiskAction::Nothing);
        assert_eq!(disk_action(None, b, false), DiskAction::Nothing);
    }

    #[test]
    fn dirty_and_read_only_rules() {
        assert!(!is_dirty("a", "a"));
        assert!(is_dirty("a", "b"));
        assert!(is_read_only(Path::new("x/session.jsonl"), false));
        assert!(is_read_only(Path::new("X.JSONL"), false));
        assert!(!is_read_only(Path::new("CLAUDE.md"), false));
        assert!(is_read_only(Path::new("CLAUDE.md"), true));
    }

    #[test]
    fn languages_and_names() {
        assert_eq!(language_for(Path::new("a/CLAUDE.md")), Some("markdown"));
        assert_eq!(language_for(Path::new("a/x.JSON")), Some("json"));
        assert_eq!(language_for(Path::new("a/x.jsonl")), Some("json"));
        assert_eq!(language_for(Path::new("a/x.rs")), None);
        assert_eq!(language_for(Path::new("Makefile")), None);
        assert_eq!(file_name(Path::new("a/b/SKILL.md")), "SKILL.md");
    }

    #[test]
    fn middle_ellipsis_keeps_both_ends() {
        assert_eq!(middle_ellipsis("short", 10), "short");
        assert_eq!(middle_ellipsis("abcdefghij", 10), "abcdefghij");
        assert_eq!(middle_ellipsis("abcdefghijk", 10), "abcde…hijk");
        let long = "C:\\Users\\me\\.claude\\skills\\deploy\\SKILL.md";
        let cut = middle_ellipsis(long, 20);
        assert_eq!(cut.chars().count(), 20);
        assert!(cut.starts_with("C:\\Users") && cut.ends_with("SKILL.md"));
        assert_eq!(middle_ellipsis("ééééééééé", 5).chars().count(), 5);
        assert_eq!(middle_ellipsis("abcdef", 2), "abcdef");
    }

    #[test]
    fn tab_path_is_stable_for_existing_and_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.md");
        std::fs::write(&file, "x").unwrap();
        assert_eq!(tab_path(&file), tab_path(&dir.path().join(".").join("a.md")));
        let missing = dir.path().join("none.md");
        assert_eq!(tab_path(&missing), missing);
    }
}
