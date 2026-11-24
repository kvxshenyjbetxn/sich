//! File/folder path detection in terminal text. Pure functions: candidates are found
//! textually, and a candidate only becomes a link once it resolves to something on disk.

use std::io::Read as _;
use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use crate::links::trim_url;

/// A path-looking piece of text (not yet checked against the disk).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PathCandidate {
    /// Byte range in the source text (includes a `:line:col` suffix, excludes quotes).
    pub range: Range<usize>,
    /// The bare path text (no quotes, no line/column suffix).
    pub raw: String,
    pub line: Option<u32>,
    pub col: Option<u32>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PathKind {
    File,
    Dir,
}

/// Characters that end an unquoted path.
fn is_forbidden(c: char) -> bool {
    c.is_whitespace()
        || c.is_control()
        || matches!(c, '<' | '>' | '"' | '\'' | '`' | '|')
        || ('\u{2500}'..='\u{257F}').contains(&c)
}

fn is_sep(c: char) -> bool {
    c == '/' || c == '\\'
}

/// Whether `s` begins like a path: drive, UNC, Unix absolute, home, or relative with a separator.
fn path_start(s: &str) -> bool {
    strict_start(s) || (!s.contains("://") && s.contains(is_sep) && s.chars().any(|c| !is_sep(c)))
}

/// Like `path_start` but without the "relative path with a separator" form.
fn strict_start(s: &str) -> bool {
    if s.contains("://") {
        return false;
    }
    let mut it = s.chars();
    let (Some(a), Some(b)) = (it.next(), it.next()) else { return false };
    let c = it.next();
    if a.is_ascii_alphabetic() && b == ':' && c.is_some_and(is_sep) {
        return true;
    }
    if a == '\\' && b == '\\' {
        return c.is_some_and(|c| !is_sep(c));
    }
    if a == '/' {
        return b != '/';
    }
    a == '~' && is_sep(b)
}

fn all_digits(s: &str) -> Option<u32> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// Splits a trailing `:line`, `:line:col` or `(line,col)` off `s`.
fn strip_position(s: &str) -> (&str, Option<u32>, Option<u32>) {
    if let Some(body) = s.strip_suffix(')')
        && let Some(open) = body.rfind('(')
        && open > 0
    {
        let inner = &body[open + 1..];
        let (l, c) = match inner.split_once(',') {
            Some((l, c)) => (all_digits(l.trim()), all_digits(c.trim())),
            None => (all_digits(inner.trim()), None),
        };
        if l.is_some() && (c.is_some() || !inner.contains(',')) {
            return (&body[..open], l, c);
        }
    }
    if let Some((rest, last)) = s.rsplit_once(':')
        && let Some(last_n) = all_digits(last)
        && !rest.is_empty()
    {
        if let Some((rest2, prev)) = rest.rsplit_once(':')
            && let Some(prev_n) = all_digits(prev)
            && !rest2.is_empty()
        {
            return (rest2, Some(prev_n), Some(last_n));
        }
        return (rest, Some(last_n), None);
    }
    (s, None, None)
}

/// Candidate from a bare token (already cut at forbidden chars) starting at byte `start`.
fn token_candidate(token: &str, start: usize) -> Option<PathCandidate> {
    let mut skip = 0;
    // `--opt=/some/path`
    if let Some(eq) = token.find('=')
        && !token[..eq].contains(is_sep)
        && path_start(&token[eq + 1..])
    {
        skip = eq + 1;
    }
    while let Some(first) = token[skip..].chars().next() {
        if "([{=,;:".contains(first) && path_start(&token[skip + first.len_utf8()..]) {
            skip += first.len_utf8();
        } else {
            break;
        }
    }
    let token = trim_url(&token[skip..]);
    let start = start + skip;
    let (stripped, line, col) = strip_position(token);
    let path = trim_url(stripped);
    if !path_start(path) {
        return None;
    }
    Some(PathCandidate { range: start..start + token.len(), raw: path.to_string(), line, col })
}

/// Candidate from the inside of a quoted region (spaces allowed) starting at byte `start`.
fn quoted_candidate(inner: &str, start: usize, spaced_relative: bool) -> Option<PathCandidate> {
    let trimmed_start = inner.len() - inner.trim_start().len();
    let inner = inner.trim();
    if inner.is_empty() || inner.chars().any(|c| is_forbidden(c) && c != ' ') {
        return None;
    }
    let (path, line, col) = strip_position(inner);
    if !path_start(path) || (!spaced_relative && !strict_start(path) && path.contains(' ')) {
        return None;
    }
    let start = start + trimmed_start;
    Some(PathCandidate { range: start..start + inner.len(), raw: path.to_string(), line, col })
}

/// Path candidates in `text`, in order.
pub fn extract_paths(text: &str) -> Vec<PathCandidate> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut out: Vec<PathCandidate> = Vec::new();
    let mut quoted: Vec<Range<usize>> = Vec::new();

    // Quoted paths first: they may contain spaces.
    let mut i = 0;
    while i < chars.len() {
        let (bi, c) = chars[i];
        if matches!(c, '"' | '\'' | '`')
            && let Some(j) = (i + 1..chars.len()).find(|&j| chars[j].1 == c)
        {
            let (start, end) = (bi + 1, chars[j].0);
            if let Some(cand) = quoted_candidate(&text[start..end], start, c != '\'') {
                quoted.push(bi..end + 1);
                out.push(cand);
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }

    // Unquoted tokens.
    let mut i = 0;
    while i < chars.len() {
        if is_forbidden(chars[i].1) {
            i += 1;
            continue;
        }
        let start = chars[i].0;
        let mut j = i;
        while j < chars.len() && !is_forbidden(chars[j].1) {
            j += 1;
        }
        let end = chars.get(j).map_or(text.len(), |c| c.0);
        i = j;
        if quoted.iter().any(|r| r.contains(&start)) {
            continue;
        }
        if let Some(cand) = token_candidate(&text[start..end], start) {
            out.push(cand);
        }
    }
    out.sort_by_key(|c| c.range.start);
    out
}

/// Lexically removes `.` and resolves `..` where possible.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Absolute path for `raw`: absolute as is, `~` against `home`, relative against `base`.
pub fn resolve(raw: &str, base: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
    if raw == "~" || raw.starts_with("~/") || raw.starts_with("~\\") {
        let rest = raw[1..].trim_start_matches(is_sep);
        let home = home?;
        return Some(if rest.is_empty() { home.to_path_buf() } else { normalize(&home.join(rest)) });
    }
    let p = Path::new(raw);
    if p.is_absolute() || raw.starts_with(is_sep) {
        return Some(p.to_path_buf());
    }
    Some(normalize(&base?.join(p)))
}

pub fn kind_of(path: &Path) -> Option<PathKind> {
    let meta = std::fs::metadata(path).ok()?;
    Some(if meta.is_dir() { PathKind::Dir } else { PathKind::File })
}

/// `resolve` plus an existence check on the real file system.
pub fn resolve_existing(raw: &str, base: Option<&Path>, home: Option<&Path>) -> Option<(PathBuf, PathKind)> {
    let path = resolve(raw, base, home)?;
    let kind = kind_of(&path)?;
    Some((path, kind))
}

const BINARY_PROBE: usize = 8 * 1024;

/// Whether the in-app editor can show `path`: images, or text files within the editor's size limit.
pub fn editor_can_open(path: &Path) -> bool {
    if crate::file_doc::is_image(path) {
        return true;
    }
    let Ok(meta) = std::fs::metadata(path) else { return false };
    if !meta.is_file() || !crate::file_doc::fits(meta.len()) {
        return false;
    }
    let Ok(file) = std::fs::File::open(path) else { return false };
    let mut head = Vec::with_capacity(BINARY_PROBE);
    if file.take(BINARY_PROBE as u64).read_to_end(&mut head).is_err() {
        return false;
    }
    !head.contains(&0)
}

const CACHE_TTL: Duration = Duration::from_secs(1);

/// One-entry cache of the last resolution, so rendering does not stat the disk every frame.
#[derive(Default)]
pub struct ResolveCache {
    last: Option<CacheEntry>,
}

struct CacheEntry {
    raw: String,
    base: Option<PathBuf>,
    at: Instant,
    result: Option<(PathBuf, PathKind)>,
}

impl ResolveCache {
    pub fn get(
        &mut self,
        raw: &str,
        base: Option<&Path>,
        now: Instant,
        resolve: impl FnOnce() -> Option<(PathBuf, PathKind)>,
    ) -> Option<(PathBuf, PathKind)> {
        if let Some(e) = &self.last
            && e.raw == raw
            && e.base.as_deref() == base
            && now.saturating_duration_since(e.at) < CACHE_TTL
        {
            return e.result.clone();
        }
        let result = resolve();
        self.last = Some(CacheEntry { raw: raw.to_string(), base: base.map(Path::to_path_buf), at: now, result: result.clone() });
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raws(s: &str) -> Vec<String> {
        extract_paths(s).into_iter().map(|c| c.raw).collect()
    }

    #[test]
    fn example_jar_path_with_plus() {
        let p = r"E:\dev\game\minecraft\PlanNH\build\libs\plannh-0.3.1-holograms.4+fbaed32d4e.jar";
        let text = format!("Built: {p}");
        let c = extract_paths(&text);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].raw, p);
        assert_eq!(&text[c[0].range.clone()], p);
    }

    #[test]
    fn windows_forms() {
        assert_eq!(raws(r"open C:\Users\me\a.txt now"), [r"C:\Users\me\a.txt"]);
        assert_eq!(raws("open C:/Users/me/a.txt now"), ["C:/Users/me/a.txt"]);
        assert_eq!(raws(r"\\server\share\dir\f.txt"), [r"\\server\share\dir\f.txt"]);
    }

    #[test]
    fn unix_home_and_relative_forms() {
        assert_eq!(raws("see /usr/local/bin/x"), ["/usr/local/bin/x"]);
        assert_eq!(raws("see ~/proj/a.rs and ~\\b\\c"), ["~/proj/a.rs", "~\\b\\c"]);
        assert_eq!(raws("edit src/foo.rs"), ["src/foo.rs"]);
        assert_eq!(raws(r"run .\build\x.jar"), [r".\build\x.jar"]);
        assert_eq!(raws("cd ../a/b"), ["../a/b"]);
    }

    #[test]
    fn plain_words_are_not_paths() {
        assert!(raws("hello world foo.rs 42").is_empty());
        assert!(raws("a lone / slash and // comment").is_empty());
    }

    #[test]
    fn urls_are_not_paths() {
        assert!(raws("see https://example.com/a/b.html ok").is_empty());
        assert!(raws("file:///C:/x/y.txt").is_empty());
    }

    #[test]
    fn quoted_paths_may_contain_spaces() {
        let t = r#"run "C:\Program Files\x\y.exe" now"#;
        let c = extract_paths(t);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].raw, r"C:\Program Files\x\y.exe");
        assert_eq!(&t[c[0].range.clone()], r"C:\Program Files\x\y.exe");
        assert_eq!(raws("open \"my dir/a b.txt\" ok"), ["my dir/a b.txt"]);
        assert_eq!(raws("open '/tmp/a b/c.txt' ok"), ["/tmp/a b/c.txt"]);
        assert_eq!(raws("open `/home/me/My Docs/x` ok"), ["/home/me/My Docs/x"]);
    }

    #[test]
    fn apostrophes_in_prose_do_not_swallow_text() {
        assert!(raws("it's fine, that's all").is_empty());
        assert_eq!(raws("it's at src/a.rs, that's it"), ["src/a.rs"]);
    }

    #[test]
    fn position_suffix_is_stripped() {
        let c = &extract_paths("error in src/main.rs:12:5: bad")[0];
        assert_eq!((c.raw.as_str(), c.line, c.col), ("src/main.rs", Some(12), Some(5)));
        let c = &extract_paths("at src/main.rs:7")[0];
        assert_eq!((c.raw.as_str(), c.line, c.col), ("src/main.rs", Some(7), None));
        let c = &extract_paths("at src/Main.cs(10,3)")[0];
        assert_eq!((c.raw.as_str(), c.line, c.col), ("src/Main.cs", Some(10), Some(3)));
        let c = &extract_paths(r"C:\a\b.rs:40.")[0];
        assert_eq!((c.raw.as_str(), c.line), (r"C:\a\b.rs", Some(40)));
        let t = "x src/main.rs:12:5 y";
        let c = &extract_paths(t)[0];
        assert_eq!(&t[c.range.clone()], "src/main.rs:12:5");
    }

    #[test]
    fn trailing_punctuation_and_wrapping_brackets() {
        assert_eq!(raws("see src/a.rs."), ["src/a.rs"]);
        assert_eq!(raws("see src/a.rs, and"), ["src/a.rs"]);
        assert_eq!(raws("(src/a.rs)"), ["src/a.rs"]);
        assert_eq!(raws("[C:\\x\\a.rs]"), ["C:\\x\\a.rs"]);
        assert_eq!(raws("path=/etc/hosts;"), ["/etc/hosts"]);
        assert_eq!(raws("src/a(1).rs"), ["src/a(1).rs"]);
    }

    #[test]
    fn box_drawing_ends_a_path() {
        assert_eq!(raws("\u{2502} src/a.rs \u{2502}"), ["src/a.rs"]);
        assert_eq!(raws("\u{2502}src/a.rs\u{2502}"), ["src/a.rs"]);
    }

    #[test]
    fn non_ascii_and_special_chars_allowed() {
        assert_eq!(raws(r"C:\Користувач\проєкт\a@b#c_[1].txt"), [r"C:\Користувач\проєкт\a@b#c_[1].txt"]);
    }

    #[test]
    fn relative_without_base_dir_is_none() {
        assert_eq!(resolve("src/a.rs", None, None), None);
        assert_eq!(resolve_existing("src/a.rs", None, None), None);
        assert_eq!(resolve("~/a", None, None), None);
    }

    #[test]
    fn resolves_against_base_and_home() {
        let base = Path::new("/base/dir");
        assert_eq!(resolve("src/a.rs", Some(base), None), Some(PathBuf::from("/base/dir/src/a.rs")));
        assert_eq!(resolve("./x/../y", Some(base), None), Some(PathBuf::from("/base/dir/y")));
        assert_eq!(resolve("../z", Some(base), None), Some(PathBuf::from("/base/z")));
        let home = Path::new("/home/me");
        assert_eq!(resolve("~/p/a", None, Some(home)), Some(PathBuf::from("/home/me/p/a")));
        assert_eq!(resolve("~", None, Some(home)), Some(PathBuf::from("/home/me")));
    }

    #[test]
    fn existing_files_and_dirs_are_classified() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub").join("a.txt"), "hi").unwrap();
        let base = dir.path();
        let (p, k) = resolve_existing("sub/a.txt", Some(base), None).unwrap();
        assert_eq!(k, PathKind::File);
        assert!(p.ends_with("a.txt"));
        assert_eq!(resolve_existing("sub", Some(base), None).unwrap().1, PathKind::Dir);
        assert_eq!(resolve_existing("sub/missing.txt", Some(base), None), None);
        // Absolute paths ignore the base.
        let abs = dir.path().join("sub").join("a.txt");
        assert_eq!(resolve_existing(abs.to_str().unwrap(), None, None).unwrap().1, PathKind::File);
        // `~` against a fake home.
        assert_eq!(resolve_existing("~/sub", None, Some(base)).unwrap().1, PathKind::Dir);
    }

    #[test]
    fn editor_rules_text_binary_and_size() {
        let dir = tempfile::tempdir().unwrap();
        let text = dir.path().join("a.txt");
        std::fs::write(&text, "hello").unwrap();
        assert!(editor_can_open(&text));
        let bin = dir.path().join("a.jar");
        std::fs::write(&bin, b"PK\x03\x04\0\0\0").unwrap();
        assert!(!editor_can_open(&bin));
        let big = dir.path().join("big.txt");
        std::fs::write(&big, vec![b'a'; crate::file_doc::MAX_BYTES as usize + 1]).unwrap();
        assert!(!editor_can_open(&big));
        assert!(!editor_can_open(dir.path()));
        assert!(!editor_can_open(&dir.path().join("missing")));
    }

    #[test]
    fn cache_reuses_result_until_ttl_or_key_change() {
        let mut cache = ResolveCache::default();
        let t0 = Instant::now();
        let mut calls = 0;
        let mut get = |cache: &mut ResolveCache, raw: &str, now: Instant| {
            cache.get(raw, None, now, || {
                calls += 1;
                Some((PathBuf::from(raw), PathKind::File))
            })
        };
        get(&mut cache, "/a", t0);
        get(&mut cache, "/a", t0 + Duration::from_millis(500));
        get(&mut cache, "/b", t0 + Duration::from_millis(600));
        get(&mut cache, "/b", t0 + Duration::from_secs(5));
        assert_eq!(calls, 3);
    }
}
