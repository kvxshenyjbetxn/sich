//! Link detection in the terminal grid: OSC 8 hyperlinks and plain-text URLs.

use std::path::PathBuf;
use std::sync::OnceLock;

use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::Term;
use alacritty_terminal::term::cell::Flags;
use regex::Regex;

use crate::path_links::{PathCandidate, PathKind, extract_paths};

/// Cells `c0..c1` of one viewport row.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Span {
    pub row: usize,
    pub c0: usize,
    pub c1: usize,
}

/// What a link points at.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum LinkTarget {
    Url(String),
    /// An existing file or folder (absolute path); `line` is kept for future use.
    Path { path: PathBuf, kind: PathKind, line: Option<u32> },
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Link {
    pub target: LinkTarget,
    pub spans: Vec<Span>,
}

/// Resolves a path candidate to an existing file/folder (`None` = not a link).
pub type PathResolver<'a> = &'a mut dyn FnMut(&PathCandidate) -> Option<(PathBuf, PathKind)>;

/// Max number of wrapped rows joined above/below the hovered row.
const MAX_WRAP: usize = 8;

fn url_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?i)\b(?:(?:https?|ftp|file)://|www\.)[^\s<>"`\x{2500}-\x{257F}]+"#)
            .expect("url regex")
    })
}

/// Only these URLs may be opened.
pub fn is_allowed(url: &str) -> bool {
    let l = url.to_ascii_lowercase();
    ["http://", "https://", "ftp://", "mailto:", "file://"].iter().any(|p| l.starts_with(p))
}

/// Strips trailing punctuation and unbalanced closing brackets.
pub fn trim_url(mut s: &str) -> &str {
    loop {
        let Some(last) = s.chars().next_back() else { return s };
        let strip = match last {
            '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '"' => true,
            ')' => s.matches(')').count() > s.matches('(').count(),
            ']' => s.matches(']').count() > s.matches('[').count(),
            '}' => s.matches('}').count() > s.matches('{').count(),
            _ => false,
        };
        if !strip {
            return s;
        }
        s = &s[..s.len() - last.len_utf8()];
    }
}

/// URLs in `text` as (byte range of the trimmed match, openable URL).
pub fn extract_urls(text: &str) -> Vec<(std::ops::Range<usize>, String)> {
    let mut out = Vec::new();
    for m in url_re().find_iter(text) {
        let trimmed = trim_url(m.as_str());
        let lower = trimmed.to_ascii_lowercase();
        // Nothing after the scheme / "www." means no real URL.
        let min = if lower.starts_with("www.") {
            4
        } else {
            lower.find("://").map_or(0, |i| i + 3)
        };
        if trimmed.len() <= min {
            continue;
        }
        let url = if lower.starts_with("www.") { format!("https://{trimmed}") } else { trimmed.to_string() };
        out.push((m.start()..m.start() + trimmed.len(), url));
    }
    out
}

/// Link under viewport cell (`col`, `row`), if any. URLs win over paths; `paths` decides
/// whether a path-looking text really exists.
pub fn link_at<T>(
    term: &Term<T>,
    display_offset: usize,
    col: usize,
    row: usize,
    paths: PathResolver<'_>,
) -> Option<Link> {
    let grid = term.grid();
    let top = -(grid.history_size() as i32);
    let bottom = grid.screen_lines() as i32;
    let width = grid.columns();
    let line_of = |r: i32| r - display_offset as i32;
    let hover_line = line_of(row as i32);
    if hover_line < top || hover_line >= bottom || col >= width {
        return None;
    }

    // OSC 8 hyperlink on the cell (same row only).
    if let Some(h) = grid[Line(hover_line)][Column(col)].hyperlink() {
        let uri = h.uri().to_string();
        if !is_allowed(&uri) {
            return None;
        }
        let same = |c: usize| grid[Line(hover_line)][Column(c)].hyperlink().is_some_and(|x| x == h);
        let (mut c0, mut c1) = (col, col + 1);
        while c0 > 0 && same(c0 - 1) {
            c0 -= 1;
        }
        while c1 < width && same(c1) {
            c1 += 1;
        }
        return Some(Link { target: LinkTarget::Url(uri), spans: vec![Span { row, c0, c1 }] });
    }

    // Plain text: join wrapped rows around the hovered one.
    let wraps = |line: i32| grid[Line(line)][Column(width - 1)].flags.contains(Flags::WRAPLINE);
    let mut first = hover_line;
    let mut n = 0;
    while first - 1 >= top && wraps(first - 1) && n < MAX_WRAP {
        first -= 1;
        n += 1;
    }
    let mut last = hover_line;
    n = 0;
    while last + 1 < bottom && wraps(last) && n < MAX_WRAP {
        last += 1;
        n += 1;
    }

    let mut text = String::new();
    let mut cells: Vec<(i32, usize)> = Vec::new(); // per char: (line, col)
    let mut byte_starts: Vec<usize> = Vec::new();
    for line in first..=last {
        for c in 0..width {
            let cell = &grid[Line(line)][Column(c)];
            if cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                continue;
            }
            let ch = if cell.c == '\0' || cell.c == '\t' { ' ' } else { cell.c };
            byte_starts.push(text.len());
            text.push(ch);
            cells.push((line, c));
        }
    }
    // Index of the hovered char (a wide char's spacer maps to the char before it).
    let k = cells.iter().rposition(|&(l, c)| l == hover_line && c <= col)?;
    let k_byte = byte_starts[k];
    let target;
    let range;
    if let Some((r, url)) = extract_urls(&text).into_iter().find(|(r, _)| r.contains(&k_byte)) {
        if !is_allowed(&url) {
            return None;
        }
        range = r;
        target = LinkTarget::Url(url);
    } else {
        let cand = extract_paths(&text).into_iter().find(|c| c.range.contains(&k_byte))?;
        let (path, kind) = paths(&cand)?;
        range = cand.range.clone();
        target = LinkTarget::Path { path, kind, line: cand.line };
    }
    let from = byte_starts.partition_point(|&b| b < range.start);
    let to = byte_starts.partition_point(|&b| b < range.end);
    let mut spans: Vec<Span> = Vec::new();
    for &(line, c) in &cells[from..to] {
        let vrow = (line + display_offset as i32) as usize;
        let wide = grid[Line(line)][Column(c)].flags.contains(Flags::WIDE_CHAR);
        let end = c + if wide { 2 } else { 1 };
        match spans.last_mut() {
            Some(s) if s.row == vrow && s.c1 == c => s.c1 = end,
            _ => spans.push(Span { row: vrow, c0: c, c1: end }),
        }
    }
    Some(Link { target, spans })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn urls(s: &str) -> Vec<String> {
        extract_urls(s).into_iter().map(|(_, u)| u).collect()
    }

    #[test]
    fn trailing_punctuation() {
        assert_eq!(urls("see https://example.com/a."), ["https://example.com/a"]);
        assert_eq!(urls("(https://example.com/a), ok"), ["https://example.com/a"]);
        assert_eq!(urls("\"http://x.org/p?q=1\";"), ["http://x.org/p?q=1"]);
    }

    #[test]
    fn parens() {
        assert_eq!(
            urls("https://en.wikipedia.org/wiki/Rust_(language)"),
            ["https://en.wikipedia.org/wiki/Rust_(language)"]
        );
        assert_eq!(urls("[https://a.io/x]"), ["https://a.io/x"]);
        assert_eq!(urls("(see https://a.io/x)."), ["https://a.io/x"]);
    }

    #[test]
    fn www_prefix() {
        assert_eq!(urls("go to www.example.com, now"), ["https://www.example.com"]);
    }

    #[test]
    fn bare_scheme_ignored() {
        assert!(urls("http:// and www. alone").is_empty());
    }

    #[test]
    fn allowed_schemes() {
        assert!(is_allowed("https://a"));
        assert!(is_allowed("mailto:a@b.c"));
        assert!(!is_allowed("javascript:alert(1)"));
        assert!(!is_allowed("calc.exe"));
    }

    // ---- link_at over a real grid ----

    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::term::Config;
    use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

    struct Dims(usize, usize);
    impl alacritty_terminal::grid::Dimensions for Dims {
        fn total_lines(&self) -> usize {
            self.1
        }
        fn screen_lines(&self) -> usize {
            self.1
        }
        fn columns(&self) -> usize {
            self.0
        }
    }

    fn term_with(cols: usize, rows: usize, text: &str) -> Term<VoidListener> {
        let mut term = Term::new(Config::default(), &Dims(cols, rows), VoidListener);
        let mut proc: Processor<StdSyncHandler> = Processor::new();
        proc.advance(&mut term, text.as_bytes());
        term
    }

    fn fake_exists(c: &PathCandidate) -> Option<(PathBuf, PathKind)> {
        c.raw.ends_with(".jar").then(|| (PathBuf::from(&c.raw), PathKind::File))
    }

    #[test]
    fn path_link_is_found_and_spans_cover_it() {
        let term = term_with(80, 5, r"Built: E:\dev\x\a+b.jar done");
        let link = link_at(&term, 0, 12, 0, &mut fake_exists).unwrap();
        assert!(matches!(&link.target, LinkTarget::Path { kind: PathKind::File, .. }));
        assert_eq!(link.spans, vec![Span { row: 0, c0: 7, c1: 7 + r"E:\dev\x\a+b.jar".len() }]);
        // Outside the path, and for a missing file: nothing.
        assert!(link_at(&term, 0, 2, 0, &mut fake_exists).is_none());
        assert!(link_at(&term, 0, 12, 0, &mut |_| None).is_none());
    }

    #[test]
    fn wrapped_path_link_spans_two_rows() {
        // 20 columns: the 26-char path wraps onto the next row.
        let term = term_with(20, 5, "/very/long/dir/name/file.jar");
        let link = link_at(&term, 0, 3, 1, &mut fake_exists).unwrap();
        assert_eq!(link.spans.len(), 2);
        assert_eq!((link.spans[0].row, link.spans[1].row), (0, 1));
    }

    #[test]
    fn url_wins_over_path_and_skips_the_resolver() {
        let term = term_with(80, 3, "go https://example.com/a/b.jar now");
        let link = link_at(&term, 0, 10, 0, &mut |_| panic!("resolver must not run")).unwrap();
        assert_eq!(link.target, LinkTarget::Url("https://example.com/a/b.jar".into()));
    }
}
