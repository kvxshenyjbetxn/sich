//! Images seen in a Claude Code session: parses the `*.img.json` manifests written by the Claude
//! Code mod, matches terminals to them, and turns each entry into a viewable file (inline base64
//! data is decoded once to a file next to its `.b64`). Pure logic + fail-soft loading; the UI lives
//! in `images_panel`.
//!
//! The format is produced by Sich itself and may evolve: unknown fields are ignored, missing optional
//! fields are fine, malformed entries are skipped and nothing here panics on bad input.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use base64::Engine as _;
use serde_json::Value;

use crate::claude_ctx::{self, Candidate, Target};

pub(crate) const MANIFEST_SUFFIX: &str = ".img.json";
/// Sub-directory of the data directory with the decoded image files, one directory per session.
pub(crate) const IMG_DIR: &str = "img";
const SUPPORTED_VERSION: u64 = 1;
/// Refuse to parse absurdly large manifests and to decode absurdly large inline images.
const MAX_MANIFEST_BYTES: u64 = 8 * 1024 * 1024;
const MAX_B64_BYTES: u64 = 48 * 1024 * 1024;
/// Extensions we write (and look for) for decoded inline images.
const DECODED_EXTS: [&str; 5] = ["png", "jpg", "gif", "webp", "bmp"];

// ---------------------------------------------------------------------------------------------
// Manifest
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    Read,
    Paste,
    Mcp,
    /// A source word this version does not know (shown without a source label).
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ImageEntry {
    pub id: String,
    pub name: String,
    pub source: Source,
    /// An image file on disk.
    pub path: Option<PathBuf>,
    /// A text file with the standard base64 of the image bytes.
    pub data: Option<PathBuf>,
    pub mime: Option<String>,
    pub at: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Manifest {
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub term_id: Option<String>,
    pub socket: Option<String>,
    pub updated_ms: u64,
    /// In the file's order (oldest first).
    pub images: Vec<ImageEntry>,
}

fn text(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(String::from)
}

fn millis(value: &Value, key: &str) -> u64 {
    let v = &value[key];
    v.as_u64().or_else(|| v.as_f64().filter(|f| f.is_finite() && *f >= 0.).map(|f| f as u64)).unwrap_or(0)
}

fn parse_entry(value: &Value) -> Option<ImageEntry> {
    let id = text(value, "id")?;
    let path = text(value, "path").map(PathBuf::from);
    let data = text(value, "data").map(PathBuf::from);
    if path.is_none() && data.is_none() {
        return None;
    }
    let source = match text(value, "source").as_deref() {
        Some("read") => Source::Read,
        Some("paste") => Source::Paste,
        Some("mcp") => Source::Mcp,
        _ => Source::Other,
    };
    let name = text(value, "name")
        .or_else(|| path.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| id.clone());
    Some(ImageEntry { id, name, source, path, data, mime: text(value, "mime"), at: millis(value, "at") })
}

/// `None` for anything that is not a version-1 manifest object; bad entries are dropped.
pub(crate) fn parse_manifest(text_in: &str) -> Option<Manifest> {
    let value: Value = serde_json::from_str(text_in).ok()?;
    if !value.is_object() || value["version"].as_u64() != Some(SUPPORTED_VERSION) {
        return None;
    }
    let images = value["images"].as_array().map(|list| list.iter().filter_map(parse_entry).collect()).unwrap_or_default();
    Some(Manifest {
        session_id: text(&value, "sessionId"),
        cwd: text(&value, "cwd"),
        term_id: text(&value, "termId"),
        socket: text(&value, "socket"),
        updated_ms: millis(&value, "updatedAt"),
        images,
    })
}

// ---------------------------------------------------------------------------------------------
// Entry -> file
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ImageState {
    /// A file that exists and can be opened.
    Ready(PathBuf),
    /// The file is gone.
    Missing,
    /// The data cannot be turned into an image (bad base64, unusable location).
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ImageRow {
    pub id: String,
    pub name: String,
    pub source: Source,
    pub at: u64,
    pub state: ImageState,
}

/// All rows of a session, newest first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ImageList {
    pub session: String,
    pub updated_ms: u64,
    pub rows: Vec<ImageRow>,
}

pub(crate) fn ext_for_mime(mime: &str) -> Option<&'static str> {
    let essence = mime.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    Some(match essence.as_str() {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" | "image/pjpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/bmp" | "image/x-ms-bmp" => "bmp",
        _ => return None,
    })
}

/// The extension of the image whose first bytes are `bytes`.
pub(crate) fn sniff_ext(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if bytes.starts_with(b"\xFF\xD8\xFF") {
        Some("jpg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("webp")
    } else if bytes.starts_with(b"BM") {
        Some("bmp")
    } else {
        None
    }
}

/// Extension of a decoded inline image: from its mime type, else its magic bytes, else `png`.
pub(crate) fn decoded_ext(mime: Option<&str>, bytes: &[u8]) -> &'static str {
    mime.and_then(ext_for_mime).or_else(|| sniff_ext(bytes)).unwrap_or("png")
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DecodeError {
    /// The `.b64` file is gone.
    Missing,
    Bad,
}

/// The already decoded file of `b64`, if there is one.
fn existing_decoded(b64: &Path, mime: Option<&str>) -> Option<PathBuf> {
    match mime.and_then(ext_for_mime) {
        Some(ext) => Some(b64.with_extension(ext)).filter(|p| p.is_file()),
        None => DECODED_EXTS.iter().map(|ext| b64.with_extension(ext)).find(|p| p.is_file()),
    }
}

/// Decodes the base64 text file `b64` into `<same name>.<ext>` next to it, once: an existing
/// decoded file is returned as is.
pub(crate) fn decode_to_file(b64: &Path, mime: Option<&str>) -> Result<PathBuf, DecodeError> {
    if b64.extension().is_none_or(|e| e != "b64") {
        return Err(DecodeError::Bad);
    }
    if let Some(done) = existing_decoded(b64, mime) {
        return Ok(done);
    }
    let meta = std::fs::metadata(b64).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound { DecodeError::Missing } else { DecodeError::Bad }
    })?;
    if !meta.is_file() || meta.len() > MAX_B64_BYTES {
        return Err(DecodeError::Bad);
    }
    let raw = std::fs::read(b64).map_err(|_| DecodeError::Bad)?;
    let compact: Vec<u8> = raw.into_iter().filter(|b| !b.is_ascii_whitespace()).collect();
    let bytes = base64::engine::general_purpose::STANDARD.decode(compact).map_err(|_| DecodeError::Bad)?;
    if bytes.is_empty() {
        return Err(DecodeError::Bad);
    }
    let target = b64.with_extension(decoded_ext(mime, &bytes));
    let tmp = b64.with_extension("tmp");
    std::fs::write(&tmp, &bytes).map_err(|_| DecodeError::Bad)?;
    if std::fs::rename(&tmp, &target).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return Err(DecodeError::Bad);
    }
    Ok(target)
}

/// Resolves one entry. `bad` remembers inline data that failed to decode so it is not retried.
pub(crate) fn resolve(entry: &ImageEntry, bad: &mut HashSet<PathBuf>) -> ImageState {
    if let Some(path) = &entry.path {
        return if path.is_file() { ImageState::Ready(path.clone()) } else { ImageState::Missing };
    }
    let Some(data) = &entry.data else { return ImageState::Unavailable };
    if bad.contains(data) {
        return ImageState::Unavailable;
    }
    match decode_to_file(data, entry.mime.as_deref()) {
        Ok(path) => ImageState::Ready(path),
        Err(DecodeError::Missing) => ImageState::Missing,
        Err(DecodeError::Bad) => {
            bad.insert(data.clone());
            ImageState::Unavailable
        }
    }
}

/// Rows newest first (stable for equal times: the later entry of the file first).
pub(crate) fn build_list(session: &str, manifest: &Manifest, bad: &mut HashSet<PathBuf>) -> ImageList {
    let mut rows: Vec<ImageRow> = manifest
        .images
        .iter()
        .map(|entry| ImageRow {
            id: entry.id.clone(),
            name: entry.name.clone(),
            source: entry.source,
            at: entry.at,
            state: resolve(entry, bad),
        })
        .collect();
    rows.reverse();
    rows.sort_by(|a, b| b.at.cmp(&a.at));
    ImageList { session: session.to_string(), updated_ms: manifest.updated_ms, rows }
}

// ---------------------------------------------------------------------------------------------
// Prune
// ---------------------------------------------------------------------------------------------

/// `abc.img.json` -> "abc".
pub(crate) fn manifest_stem(name: &str) -> Option<&str> {
    name.strip_suffix(MANIFEST_SUFFIX).filter(|s| !s.is_empty())
}

pub(crate) fn is_manifest_name(name: &str) -> bool {
    manifest_stem(name).is_some()
}

/// An image directory goes when its manifest is gone or it has not changed for `max_age`.
pub(crate) fn should_prune_dir(manifest_exists: bool, modified: SystemTime, now: SystemTime, max_age: Duration) -> bool {
    !manifest_exists || now.duration_since(modified).is_ok_and(|age| age > max_age)
}

/// Removes stale `<data dir>/img/<session>/` directories. Only direct, real sub-directories of
/// `<data dir>/img` are ever touched.
pub(crate) fn prune_img_dirs(dir: &Path, now: SystemTime, max_age: Duration) -> usize {
    let Ok(entries) = std::fs::read_dir(dir.join(IMG_DIR)) else { return 0 };
    entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let manifest = dir.join(format!("{name}{MANIFEST_SUFFIX}"));
            let Some(modified) = e.metadata().ok().and_then(|m| m.modified().ok()) else { return false };
            should_prune_dir(manifest.is_file(), modified, now, max_age)
        })
        .filter(|e| std::fs::remove_dir_all(e.path()).is_ok())
        .count()
}

// ---------------------------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------------------------

/// Incremental reader: remembers parsed manifests by modification time.
#[derive(Default)]
pub(crate) struct ImageLoader {
    cache: HashMap<PathBuf, (SystemTime, Option<Manifest>)>,
    bad: HashSet<PathBuf>,
}

impl ImageLoader {
    /// The image list per terminal (by GUI key) of the given terminals. Decodes inline images that
    /// are not decoded yet: call from a background thread.
    pub(crate) fn load(&mut self, dir: &Path, targets: &[Target]) -> HashMap<u64, ImageList> {
        let mut listing: HashMap<String, (PathBuf, SystemTime)> = HashMap::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let Some(stem) = manifest_stem(&name) else { continue };
                let Some(modified) =
                    entry.metadata().ok().filter(|m| m.len() <= MAX_MANIFEST_BYTES).and_then(|m| m.modified().ok())
                else {
                    continue;
                };
                listing.insert(stem.to_string(), (entry.path(), modified));
            }
        }
        self.cache.retain(|path, _| listing.values().any(|(p, _)| p == path));

        let resolved = claude_ctx::pick_sessions(
            targets,
            |id| listing.contains_key(id),
            || {
                let stems: Vec<String> = listing.keys().cloned().collect();
                stems
                    .into_iter()
                    .map(|stem| {
                        let (path, modified) = listing[&stem].clone();
                        let manifest = self.manifest(&path, modified);
                        Candidate {
                            session: stem,
                            term_id: manifest.as_ref().and_then(|m| m.term_id.clone()),
                            socket: manifest.as_ref().and_then(|m| m.socket.clone()),
                            updated_ms: manifest.as_ref().map_or(0, |m| m.updated_ms),
                        }
                    })
                    .collect()
            },
        );

        let mut lists: HashMap<String, Option<ImageList>> = HashMap::new();
        let mut out = HashMap::new();
        for (target, stem) in resolved {
            let list = lists.entry(stem.clone()).or_insert_with(|| {
                let (path, modified) = listing.get(&stem)?.clone();
                let manifest = self.manifest(&path, modified)?;
                Some(build_list(&stem, &manifest, &mut self.bad))
            });
            if let Some(list) = list {
                out.insert(target.key, list.clone());
            }
        }
        out
    }

    fn manifest(&mut self, path: &Path, modified: SystemTime) -> Option<Manifest> {
        if let Some((at, manifest)) = self.cache.get(path) {
            if *at == modified {
                return manifest.clone();
            }
        }
        // A file caught mid-write or just removed: do not cache, retry next poll.
        let text = std::fs::read_to_string(path).ok()?;
        let manifest = parse_manifest(&text);
        self.cache.insert(path.to_path_buf(), (modified, manifest.clone()));
        manifest
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG_1X1_B64: &str =
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

    fn manifest_json(images: &str) -> String {
        format!(
            r#"{{ "version":1, "sessionId":"abc", "cwd":"C:/w", "termId":"3", "socket":"sock", "updatedAt":5000,
               "future":true, "images":[{images}] }}"#
        )
    }

    fn entry(id: &str, at: u64, path: Option<&str>, data: Option<&str>) -> ImageEntry {
        ImageEntry {
            id: id.into(),
            name: id.into(),
            source: Source::Read,
            path: path.map(PathBuf::from),
            data: data.map(PathBuf::from),
            mime: None,
            at,
        }
    }

    #[test]
    fn parses_a_valid_manifest() {
        let json = manifest_json(
            r#"{"id":"a","name":"shot.png","source":"read","path":"C:/w/shot.png","data":null,"mime":null,"at":10},
               {"id":"b","name":"Pasted","source":"paste","path":null,"data":"C:/c/img/abc/b.b64","mime":"image/png","at":20, "extra":1},
               {"id":"c","name":"x","source":"mcp","path":"C:/x.jpg","at":30}"#,
        );
        let m = parse_manifest(&json).unwrap();
        assert_eq!((m.session_id.as_deref(), m.term_id.as_deref(), m.socket.as_deref()), (Some("abc"), Some("3"), Some("sock")));
        assert_eq!(m.cwd.as_deref(), Some("C:/w"));
        assert_eq!(m.updated_ms, 5000);
        assert_eq!(m.images.len(), 3);
        assert_eq!(m.images[0].path, Some(PathBuf::from("C:/w/shot.png")));
        assert_eq!(m.images[0].data, None);
        assert_eq!(m.images[1].source, Source::Paste);
        assert_eq!(m.images[1].data, Some(PathBuf::from("C:/c/img/abc/b.b64")));
        assert_eq!(m.images[1].mime.as_deref(), Some("image/png"));
        assert_eq!(m.images[2].source, Source::Mcp);
    }

    #[test]
    fn parsing_is_lenient_about_entries_and_strict_about_the_envelope() {
        // Bad entries are skipped, the good ones survive.
        let json = manifest_json(
            r#"42, "x", {}, {"id":"noloc"}, {"id":"","path":"/a.png"}, {"path":"/a.png"},
               {"id":"odd","path":"/o.png","source":"future","at":"soon"}, {"id":"ok","path":"/ok.png"}"#,
        );
        let m = parse_manifest(&json).unwrap();
        let ids: Vec<_> = m.images.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, ["odd", "ok"]);
        assert_eq!(m.images[0].source, Source::Other);
        assert_eq!(m.images[0].at, 0);
        assert_eq!(m.images[1].name, "ok.png");
        // Both path and data given: both kept, `path` wins at resolve time.
        let both = parse_manifest(&manifest_json(r#"{"id":"a","path":"/a.png","data":"/a.b64"}"#)).unwrap();
        assert!(both.images[0].path.is_some() && both.images[0].data.is_some());
        // Missing optional envelope fields and a missing list are fine.
        let bare = parse_manifest(r#"{"version":1}"#).unwrap();
        assert_eq!((bare.session_id, bare.term_id, bare.updated_ms, bare.images.len()), (None, None, 0, 0));
        assert!(parse_manifest(r#"{"version":1,"images":"nope"}"#).unwrap().images.is_empty());
        // Wrong or missing version, garbage, non-objects.
        for bad in [r#"{"version":2,"images":[]}"#, r#"{"images":[]}"#, r#"{"version":"1"}"#, "", "nope", "[1]", "{\"version\":1,"] {
            assert!(parse_manifest(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn mime_and_magic_bytes_pick_the_extension() {
        assert_eq!(ext_for_mime("image/png"), Some("png"));
        assert_eq!(ext_for_mime("IMAGE/JPEG; charset=x"), Some("jpg"));
        assert_eq!(ext_for_mime("image/jpg"), Some("jpg"));
        assert_eq!(ext_for_mime("image/gif"), Some("gif"));
        assert_eq!(ext_for_mime("image/webp"), Some("webp"));
        assert_eq!(ext_for_mime("image/bmp"), Some("bmp"));
        assert_eq!(ext_for_mime("image/svg+xml"), None);
        assert_eq!(ext_for_mime(""), None);

        assert_eq!(sniff_ext(b"\x89PNG\r\n\x1a\nrest"), Some("png"));
        assert_eq!(sniff_ext(b"\xFF\xD8\xFF\xE0xx"), Some("jpg"));
        assert_eq!(sniff_ext(b"GIF89a.."), Some("gif"));
        assert_eq!(sniff_ext(b"GIF87a.."), Some("gif"));
        assert_eq!(sniff_ext(b"RIFF\x00\x00\x00\x00WEBPVP8 "), Some("webp"));
        assert_eq!(sniff_ext(b"RIFF\x00\x00\x00\x00WAVEfmt "), None);
        assert_eq!(sniff_ext(b"BM\x00\x00"), Some("bmp"));
        assert_eq!(sniff_ext(b"hello"), None);
        assert_eq!(sniff_ext(b""), None);

        // Mime first, then magic bytes, then png.
        assert_eq!(decoded_ext(Some("image/gif"), b"\x89PNG\r\n\x1a\n"), "gif");
        assert_eq!(decoded_ext(Some("application/x"), b"GIF89a"), "gif");
        assert_eq!(decoded_ext(None, b"\xFF\xD8\xFF"), "jpg");
        assert_eq!(decoded_ext(None, b"???"), "png");
    }

    #[test]
    fn inline_data_is_decoded_once_next_to_the_source() {
        let dir = tempfile::tempdir().unwrap();
        let b64 = dir.path().join("a.b64");
        // Line breaks inside the base64 are tolerated.
        let (head, tail) = PNG_1X1_B64.split_at(20);
        std::fs::write(&b64, format!("{head}\n{tail}\n")).unwrap();
        let out = decode_to_file(&b64, Some("image/png")).unwrap();
        assert_eq!(out, dir.path().join("a.png"));
        let bytes = std::fs::read(&out).unwrap();
        assert_eq!(sniff_ext(&bytes), Some("png"));
        assert!(!dir.path().join("a.tmp").exists());

        // Second call: the decoded file is reused, not rewritten (even with the source gone).
        std::fs::write(&out, b"marker").unwrap();
        assert_eq!(decode_to_file(&b64, Some("image/png")).unwrap(), out);
        assert_eq!(std::fs::read(&out).unwrap(), b"marker");
        std::fs::remove_file(&b64).unwrap();
        assert_eq!(decode_to_file(&b64, Some("image/png")).unwrap(), out);
        assert_eq!(decode_to_file(&b64, None).unwrap(), out);
    }

    #[test]
    fn inline_data_without_mime_is_sniffed_and_failures_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let gif = dir.path().join("g.b64");
        std::fs::write(&gif, base64::engine::general_purpose::STANDARD.encode(b"GIF89a-body")).unwrap();
        assert_eq!(decode_to_file(&gif, None).unwrap(), dir.path().join("g.gif"));
        let unknown = dir.path().join("u.b64");
        std::fs::write(&unknown, base64::engine::general_purpose::STANDARD.encode(b"just bytes")).unwrap();
        assert_eq!(decode_to_file(&unknown, None).unwrap(), dir.path().join("u.png"));

        let bad = dir.path().join("bad.b64");
        std::fs::write(&bad, "!!! not base64 !!!").unwrap();
        assert_eq!(decode_to_file(&bad, None), Err(DecodeError::Bad));
        assert!(!dir.path().join("bad.png").exists());
        let empty = dir.path().join("empty.b64");
        std::fs::write(&empty, "  \n").unwrap();
        assert_eq!(decode_to_file(&empty, None), Err(DecodeError::Bad));
        assert_eq!(decode_to_file(&dir.path().join("none.b64"), None), Err(DecodeError::Missing));
        // Only `.b64` files are decoded (never overwrite arbitrary files).
        let other = dir.path().join("x.txt");
        std::fs::write(&other, PNG_1X1_B64).unwrap();
        assert_eq!(decode_to_file(&other, None), Err(DecodeError::Bad));
        assert_eq!(decode_to_file(&dir.path().join("noext"), None), Err(DecodeError::Bad));
    }

    #[test]
    fn entries_resolve_to_ready_missing_or_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("pic.png");
        std::fs::write(&file, b"x").unwrap();
        let b64 = dir.path().join("p.b64");
        std::fs::write(&b64, PNG_1X1_B64).unwrap();
        let bad = dir.path().join("bad.b64");
        std::fs::write(&bad, "@@@").unwrap();
        let mut known_bad = HashSet::new();

        let path_of = |p: &Path| p.to_str().unwrap().to_string();
        let ready = entry("a", 1, Some(&path_of(&file)), None);
        assert_eq!(resolve(&ready, &mut known_bad), ImageState::Ready(file.clone()));
        let gone = entry("b", 1, Some(&path_of(&dir.path().join("gone.png"))), None);
        assert_eq!(resolve(&gone, &mut known_bad), ImageState::Missing);
        let inline = entry("c", 1, None, Some(&path_of(&b64)));
        assert_eq!(resolve(&inline, &mut known_bad), ImageState::Ready(dir.path().join("p.png")));
        let no_b64 = entry("d", 1, None, Some(&path_of(&dir.path().join("none.b64"))));
        assert_eq!(resolve(&no_b64, &mut known_bad), ImageState::Missing);
        let broken = entry("e", 1, None, Some(&path_of(&bad)));
        assert_eq!(resolve(&broken, &mut known_bad), ImageState::Unavailable);
        assert!(known_bad.contains(&bad));
        // Known-bad data is not retried, even if the file is fixed later.
        std::fs::write(&bad, PNG_1X1_B64).unwrap();
        assert_eq!(resolve(&broken, &mut known_bad), ImageState::Unavailable);
        assert_eq!(resolve(&entry("f", 1, None, None), &mut known_bad), ImageState::Unavailable);
    }

    #[test]
    fn list_is_newest_first() {
        let manifest = Manifest {
            updated_ms: 9,
            images: vec![
                entry("old", 10, Some("/no/old.png"), None),
                entry("new", 30, Some("/no/new.png"), None),
                entry("mid", 20, Some("/no/mid.png"), None),
                entry("tie-a", 5, Some("/no/a.png"), None),
                entry("tie-b", 5, Some("/no/b.png"), None),
            ],
            ..Default::default()
        };
        let list = build_list("abc", &manifest, &mut HashSet::new());
        let ids: Vec<_> = list.rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["new", "mid", "old", "tie-b", "tie-a"]);
        assert_eq!((list.session.as_str(), list.updated_ms), ("abc", 9));
        assert!(list.rows.iter().all(|r| r.state == ImageState::Missing));
    }

    #[test]
    fn manifest_names() {
        assert_eq!(manifest_stem("abc.img.json"), Some("abc"));
        assert_eq!(manifest_stem(".img.json"), None);
        assert_eq!(manifest_stem("abc.mod.json"), None);
        assert!(is_manifest_name("x-1_y.img.json"));
        assert!(!is_manifest_name("x.img.json.bak"));
    }

    #[test]
    fn prune_decision() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000_000);
        let day = Duration::from_secs(86_400);
        let age = claude_ctx::PRUNE_AGE;
        assert!(should_prune_dir(false, now, now, age));
        assert!(!should_prune_dir(true, now - day, now, age));
        assert!(should_prune_dir(true, now - day * 8, now, age));
        assert!(!should_prune_dir(true, now + day, now, age));
    }

    #[test]
    fn prune_img_dirs_removes_orphans_and_old_dirs_only() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("claude-ctx");
        let img = dir.join(IMG_DIR);
        for session in ["live", "orphan"] {
            std::fs::create_dir_all(img.join(session)).unwrap();
            std::fs::write(img.join(session).join("a.b64"), "x").unwrap();
        }
        std::fs::write(dir.join("live.img.json"), "{}").unwrap();
        // A loose file in img/ and everything outside img/ stay.
        std::fs::write(img.join("loose.txt"), "x").unwrap();
        std::fs::create_dir_all(dir.join("other")).unwrap();
        std::fs::write(root.path().join("outside.txt"), "x").unwrap();

        assert_eq!(prune_img_dirs(&dir, SystemTime::now(), claude_ctx::PRUNE_AGE), 1);
        assert!(img.join("live").join("a.b64").exists());
        assert!(!img.join("orphan").exists());
        assert!(img.join("loose.txt").exists());
        assert!(dir.join("other").exists());
        assert!(root.path().join("outside.txt").exists());

        // Far in the future even the live one is stale.
        let later = SystemTime::now() + Duration::from_secs(8 * 86_400);
        assert_eq!(prune_img_dirs(&dir, later, claude_ctx::PRUNE_AGE), 1);
        assert!(!img.join("live").exists());
        assert!(dir.join("live.img.json").exists());
        assert!(img.join("loose.txt").exists());
        // No img dir at all is fine.
        assert_eq!(prune_img_dirs(&root.path().join("none"), later, claude_ctx::PRUNE_AGE), 0);
    }

    fn target(key: u64, session: Option<&str>, raw: &str, socket: &str) -> Target {
        Target { key, session_id: session.map(Into::into), raw_id: raw.into(), socket: socket.into(), title: String::new(), worktree: None }
    }

    fn write_manifest(dir: &Path, stem: &str, term: &str, socket: &str, updated: u64, image: &str) {
        let json = format!(
            r#"{{"version":1,"sessionId":"{stem}","termId":"{term}","socket":"{socket}","updatedAt":{updated},
                "images":[{{"id":"i-{stem}","name":"{stem}.png","source":"read","path":"{image}","at":{updated}}}]}}"#
        );
        std::fs::write(dir.join(format!("{stem}.img.json")), json).unwrap();
    }

    #[test]
    fn loader_matches_by_own_session_then_term_and_socket_newest_wins() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        let shot = p.join("shot.png").to_str().unwrap().replace('\\', "/");
        write_manifest(p, "old", "3", "sock", 10, &shot);
        write_manifest(p, "new", "3", "sock", 20, &shot);
        write_manifest(p, "elsewhere", "3", "other", 99, &shot);
        std::fs::write(p.join("shot.png"), b"x").unwrap();
        std::fs::write(p.join("broken.img.json"), "garbage").unwrap();
        std::fs::write(p.join("abc.mod.json"), "{}").unwrap();

        let mut loader = ImageLoader::default();
        let targets = [
            target(1, Some("old"), "9", "x"),
            target(2, None, "3", "sock"),
            target(3, Some("gone"), "3", "sock"),
            target(4, None, "3", "nope"),
            target(5, Some("broken"), "8", "sock"),
        ];
        let out = loader.load(p, &targets);
        assert_eq!(out.len(), 3);
        assert_eq!(out[&1].session, "old");
        assert_eq!(out[&2].session, "new");
        assert_eq!(out[&3].session, "new");
        assert_eq!(out[&2].rows[0].state, ImageState::Ready(PathBuf::from(&shot)));

        // A changed manifest is noticed through its modification time.
        write_manifest(p, "new", "3", "sock", 30, &shot);
        let later = SystemTime::now() + Duration::from_secs(5);
        std::fs::File::options().write(true).open(p.join("new.img.json")).unwrap().set_modified(later).unwrap();
        assert_eq!(loader.load(p, &targets)[&2].updated_ms, 30);

        // A removed image file turns into Missing; removed manifests drop the lists; a missing dir is fine.
        std::fs::remove_file(p.join("shot.png")).unwrap();
        assert_eq!(loader.load(p, &targets)[&2].rows[0].state, ImageState::Missing);
        for stem in ["old", "new", "elsewhere"] {
            std::fs::remove_file(p.join(format!("{stem}.img.json"))).unwrap();
        }
        assert!(loader.load(p, &targets).is_empty());
        assert!(loader.load(&p.join("none"), &targets).is_empty());
    }

    #[test]
    fn loader_decodes_inline_images_into_the_session_directory() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        let session_dir = p.join(IMG_DIR).join("abc");
        std::fs::create_dir_all(&session_dir).unwrap();
        let b64 = session_dir.join("p1.b64");
        std::fs::write(&b64, PNG_1X1_B64).unwrap();
        let json = format!(
            r#"{{"version":1,"sessionId":"abc","termId":"1","socket":"s","updatedAt":1,
                "images":[{{"id":"p1","name":"Pasted","source":"paste","data":{},"mime":"image/png","at":7}}]}}"#,
            serde_json::to_string(b64.to_str().unwrap()).unwrap()
        );
        std::fs::write(p.join("abc.img.json"), json).unwrap();
        let out = ImageLoader::default().load(p, &[target(1, Some("abc"), "1", "s")]);
        assert_eq!(out[&1].rows[0].state, ImageState::Ready(session_dir.join("p1.png")));
        assert!(session_dir.join("p1.png").is_file());
    }
}
