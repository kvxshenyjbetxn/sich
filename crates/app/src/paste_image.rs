//! Pasting an image from the clipboard into a terminal: the image is saved as a PNG in a temp
//! folder and its path is pasted (agent CLIs such as Claude Code and Codex read images by path).

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gpui_kit::{ClipboardEntry, ClipboardItem, Image, ImageFormat};
use sich_proto::AgentKind;

/// Pasted files older than this are removed on startup.
pub const MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
/// Largest image accepted from the clipboard.
const MAX_BYTES: usize = 64 * 1024 * 1024;

const PREFIX: &str = "paste-";
const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";

pub fn paste_dir() -> PathBuf {
    std::env::temp_dir().join("sich-paste")
}

/// `paste-<unix ms>-<seq>.png`; the sequence number keeps names unique within a millisecond.
pub fn file_name(unix_ms: u128, seq: u32) -> String {
    format!("{PREFIX}{unix_ms}-{seq}.png")
}

/// The clipboard's image, but only when it carries no text (copying a file or text that also
/// has an image preview must paste the text).
pub fn clipboard_image(item: &ClipboardItem) -> Option<&Image> {
    pick_image(item.entries())
}

fn pick_image(entries: &[ClipboardEntry]) -> Option<&Image> {
    let has_text = entries.iter().any(|e| match e {
        ClipboardEntry::String(s) => !s.text().is_empty(),
        ClipboardEntry::ExternalPaths(p) => !p.paths().is_empty(),
        ClipboardEntry::Image(_) => false,
    });
    if has_text {
        return None;
    }
    entries.iter().find_map(|e| match e {
        ClipboardEntry::Image(img) => Some(img),
        _ => None,
    })
}

/// PNG bytes for a clipboard image: PNG data is kept as is, anything else is re-encoded.
pub fn to_png(image: &Image) -> io::Result<Vec<u8>> {
    if image.bytes.is_empty() || image.bytes.len() > MAX_BYTES {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "empty or oversized image"));
    }
    if image.format == ImageFormat::Png && image.bytes.starts_with(PNG_MAGIC) {
        return Ok(image.bytes.clone());
    }
    let img = ::image::load_from_memory(&image.bytes)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    let mut out = Vec::new();
    img.write_to(&mut io::Cursor::new(&mut out), ::image::ImageFormat::Png)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    Ok(out)
}

/// Saves the clipboard image into `dir` and returns the file path.
pub fn save(image: &Image, dir: &Path, unix_ms: u128) -> io::Result<PathBuf> {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let png = to_png(image)?;
    std::fs::create_dir_all(dir)?;
    let path = dir.join(file_name(unix_ms, SEQ.fetch_add(1, Ordering::Relaxed)));
    std::fs::write(&path, png)?;
    Ok(path)
}

pub fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis())
}

/// Deletes our pasted files older than `max_age`; returns how many were removed.
pub fn prune(dir: &Path, now: SystemTime, max_age: Duration) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    let mut removed = 0;
    for e in entries.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if !(name.starts_with(PREFIX) && name.ends_with(".png")) {
            continue;
        }
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age > max_age);
        if old && std::fs::remove_file(e.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Text to paste for a saved image: the path, quoted when it contains whitespace.
pub fn path_text(path: &Path) -> String {
    let s = path.to_string_lossy();
    if s.contains(char::is_whitespace) { format!("\"{s}\"") } else { s.into_owned() }
}

/// Whether the paste must be bracketed even if the program did not enable bracketed paste mode.
/// Claude Code and Codex treat a bracketed path as an attachment; other programs get plain text.
pub fn force_bracketed(agent: Option<AgentKind>) -> bool {
    matches!(agent, Some(AgentKind::Claude | AgentKind::Codex))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut out = Vec::new();
        ::image::RgbaImage::from_pixel(w, h, ::image::Rgba([1, 2, 3, 255]))
            .write_to(&mut io::Cursor::new(&mut out), ::image::ImageFormat::Png)
            .unwrap();
        out
    }

    fn clip_image(format: ImageFormat, bytes: Vec<u8>) -> Image {
        Image { format, bytes, id: 1 }
    }

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sich-test-{tag}-{}-{}", std::process::id(), now_ms()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn file_names_are_unique_and_png() {
        assert_eq!(file_name(1700000000123, 4), "paste-1700000000123-4.png");
        assert_ne!(file_name(1, 1), file_name(1, 2));
        assert!(paste_dir().ends_with("sich-paste"));
    }

    #[test]
    fn image_only_when_the_clipboard_has_no_text() {
        let img = clip_image(ImageFormat::Png, png(2, 2));
        assert!(clipboard_image(&ClipboardItem::new_image(&img)).is_some());
        assert!(clipboard_image(&ClipboardItem::new_string("hello".into())).is_none());
        let mixed = [
            ClipboardEntry::String(gpui_kit::ClipboardString::new("text".into())),
            ClipboardEntry::Image(img.clone()),
        ];
        assert!(pick_image(&mixed).is_none(), "text wins over an image preview");
        let empty_text = [
            ClipboardEntry::String(gpui_kit::ClipboardString::new(String::new())),
            ClipboardEntry::Image(img),
        ];
        assert!(pick_image(&empty_text).is_some());
    }

    #[test]
    fn png_is_kept_and_other_formats_are_reencoded() {
        let p = png(3, 3);
        assert_eq!(to_png(&clip_image(ImageFormat::Png, p.clone())).unwrap(), p);
        let mut bmp = Vec::new();
        ::image::RgbImage::from_pixel(2, 2, ::image::Rgb([9, 9, 9]))
            .write_to(&mut io::Cursor::new(&mut bmp), ::image::ImageFormat::Bmp)
            .unwrap();
        let out = to_png(&clip_image(ImageFormat::Bmp, bmp)).unwrap();
        assert!(out.starts_with(PNG_MAGIC));
        // A "PNG" that is not one is decoded or rejected rather than trusted.
        assert!(to_png(&clip_image(ImageFormat::Png, b"junk".to_vec())).is_err());
        assert!(to_png(&clip_image(ImageFormat::Png, Vec::new())).is_err());
    }

    #[test]
    fn save_writes_a_unique_png_per_call() {
        let dir = temp("save");
        let img = clip_image(ImageFormat::Png, png(2, 2));
        let a = save(&img, &dir, 5).unwrap();
        let b = save(&img, &dir, 5).unwrap();
        assert_ne!(a, b);
        assert!(a.starts_with(&dir) && a.extension().unwrap() == "png");
        assert!(std::fs::read(&a).unwrap().starts_with(PNG_MAGIC));
        assert!(save(&clip_image(ImageFormat::Png, vec![1, 2]), &dir, 5).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn prune_removes_only_old_paste_files() {
        let dir = temp("prune");
        let old = dir.join(file_name(1, 1));
        let other = dir.join("keep.txt");
        let fresh = dir.join(file_name(2, 2));
        for f in [&old, &other, &fresh] {
            std::fs::write(f, b"x").unwrap();
        }
        let old_time = SystemTime::now() - Duration::from_secs(48 * 3600);
        std::fs::File::options().write(true).open(&old).unwrap().set_modified(old_time).unwrap();
        std::fs::File::options().write(true).open(&other).unwrap().set_modified(old_time).unwrap();
        assert_eq!(prune(&dir, SystemTime::now(), MAX_AGE), 1);
        assert!(!old.exists() && other.exists() && fresh.exists());
        assert_eq!(prune(&dir.join("missing"), SystemTime::now(), MAX_AGE), 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn path_text_quotes_only_when_needed() {
        assert_eq!(path_text(Path::new("C:\\t\\a.png")), "C:\\t\\a.png");
        assert_eq!(path_text(Path::new("C:\\My Dir\\a.png")), "\"C:\\My Dir\\a.png\"");
    }

    #[test]
    fn bracketed_paste_is_forced_for_claude_and_codex_only() {
        assert!(force_bracketed(Some(AgentKind::Claude)));
        assert!(force_bracketed(Some(AgentKind::Codex)));
        assert!(!force_bracketed(Some(AgentKind::Gemini)));
        assert!(!force_bracketed(Some(AgentKind::Other)));
        assert!(!force_bracketed(None));
    }
}
