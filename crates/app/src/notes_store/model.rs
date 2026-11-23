//! Pure model of diff notes: the note type, on-disk storage, edits, grouping and the mapping
//! between notes and diff rows. Nothing here touches GPUI.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sich_git::compare::{Comparison, CompareSource, Projection, VisibleRow};

pub(crate) const SCHEMA: u32 = 1;
/// The quoted code of a note is capped so one note cannot bloat the file or the prompt.
pub(crate) const TEXT_CAP: usize = 2048;

/// The diff the note was written on (mirrors `CompareSource`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum NoteSource {
    Unstaged,
    Staged,
    Commit { oid: String },
    Range { base: String, head: String },
    Conflict,
}

impl NoteSource {
    pub(crate) fn from_compare(source: &CompareSource) -> Self {
        match source {
            CompareSource::Working => Self::Unstaged,
            CompareSource::Staged => Self::Staged,
            CompareSource::Commit(oid) => Self::Commit { oid: oid.clone() },
            CompareSource::Range { base, head } => Self::Range { base: base.clone(), head: head.clone() },
            CompareSource::Conflict => Self::Conflict,
        }
    }

    pub(crate) fn to_compare(&self) -> CompareSource {
        match self {
            Self::Unstaged => CompareSource::Working,
            Self::Staged => CompareSource::Staged,
            Self::Commit { oid } => CompareSource::Commit(oid.clone()),
            Self::Range { base, head } => CompareSource::Range { base: base.clone(), head: head.clone() },
            Self::Conflict => CompareSource::Conflict,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DiffNote {
    pub id: String,
    pub worktree: PathBuf,
    pub file_path: String,
    #[serde(default)]
    pub old_path: Option<String>,
    pub source: NoteSource,
    /// First line of a range note; `None` for a single line.
    #[serde(default)]
    pub start_line: Option<u32>,
    /// Last (or only) line; 0 means the whole file.
    #[serde(default)]
    pub line: u32,
    /// The line numbers refer to the old side (a deleted line).
    #[serde(default)]
    pub deleted: bool,
    #[serde(default)]
    pub selected_text: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub created_ms: i64,
    #[serde(default)]
    pub updated_ms: i64,
    #[serde(default)]
    pub sent_ms: Option<i64>,
}

#[derive(Serialize, Deserialize)]
struct NotesFile {
    version: u32,
    #[serde(default)]
    notes: Vec<serde_json::Value>,
}

// ---- storage ---------------------------------------------------------------------------------

/// Slashes unified, no trailing slash, case-folded on Windows: one worktree, one key.
pub(crate) fn normalize_worktree(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let text = text.trim_end_matches('/');
    if cfg!(windows) { text.to_lowercase() } else { text.to_owned() }
}

pub(crate) fn worktree_hash(path: &Path) -> String {
    let digest = Sha256::digest(normalize_worktree(path).as_bytes());
    digest.iter().take(8).map(|byte| format!("{byte:02x}")).collect()
}

pub(crate) fn notes_file(dir: &Path, worktree: &Path) -> PathBuf {
    dir.join(format!("{}.json", worktree_hash(worktree)))
}

/// `<config>/sich/notes` (honors `SICH_CONFIG_DIR`).
pub(crate) fn default_dir() -> Option<PathBuf> {
    crate::settings::config_file("notes")
}

/// Tolerant parse: garbage gives no notes, a note that does not parse is skipped.
pub(crate) fn parse_notes(text: &str) -> Vec<DiffNote> {
    let Ok(file) = serde_json::from_str::<NotesFile>(text) else { return Vec::new() };
    file.notes.into_iter().filter_map(|value| serde_json::from_value(value).ok()).collect()
}

pub(crate) fn serialize_notes(notes: &[DiffNote]) -> String {
    let notes = notes.iter().filter_map(|note| serde_json::to_value(note).ok()).collect();
    serde_json::to_string_pretty(&NotesFile { version: SCHEMA, notes }).unwrap_or_default()
}

pub(crate) fn load_notes(path: &Path) -> Vec<DiffNote> {
    std::fs::read_to_string(path).map(|text| parse_notes(&text)).unwrap_or_default()
}

/// Atomic write (temp file + rename). An empty list removes the file.
pub(crate) fn save_notes(path: &Path, notes: &[DiffNote]) -> std::io::Result<()> {
    if notes.is_empty() {
        return match std::fs::remove_file(path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        };
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serialize_notes(notes))?;
    std::fs::rename(&tmp, path)
}

// ---- edits -----------------------------------------------------------------------------------

pub(crate) fn cap_text(text: &str) -> String {
    if text.len() <= TEXT_CAP {
        return text.to_owned();
    }
    let mut end = TEXT_CAP;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// A new id that is not used by any note in `notes`.
pub(crate) fn fresh_id(notes: &[DiffNote], now_ms: i64, seq: &mut u64) -> String {
    loop {
        *seq += 1;
        let id = format!("n{now_ms:x}-{:x}", *seq);
        if notes.iter().all(|note| note.id != id) {
            return id;
        }
    }
}

/// Editing the text makes the note unsent again. Returns false when the id is unknown or the
/// body did not change.
pub(crate) fn edit_body(notes: &mut [DiffNote], id: &str, body: &str, now_ms: i64) -> bool {
    match notes.iter_mut().find(|note| note.id == id) {
        Some(note) if note.body != body => {
            note.body = body.to_owned();
            note.updated_ms = now_ms;
            note.sent_ms = None;
            true
        }
        _ => false,
    }
}

pub(crate) fn delete_note(notes: &mut Vec<DiffNote>, id: &str) -> bool {
    let before = notes.len();
    notes.retain(|note| note.id != id);
    notes.len() != before
}

pub(crate) fn mark_sent(notes: &mut [DiffNote], ids: &[String], now_ms: i64) {
    for note in notes.iter_mut().filter(|note| ids.contains(&note.id)) {
        note.sent_ms = Some(now_ms);
    }
}

// ---- display ---------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NoteSpan {
    File,
    Line(u32),
    Lines(u32, u32),
}

pub(crate) fn span(note: &DiffNote) -> NoteSpan {
    match (note.line, note.start_line) {
        (0, _) => NoteSpan::File,
        (line, Some(start)) if start != line => NoteSpan::Lines(start.min(line), start.max(line)),
        (line, _) => NoteSpan::Line(line),
    }
}

/// Sort key: file, then position in the file, then creation time.
fn position(note: &DiffNote) -> (u32, bool, i64) {
    (note.start_line.map_or(note.line, |start| start.min(note.line)), note.deleted, note.created_ms)
}

/// Notes grouped by file (sorted by path); inside a file the file-level notes come first, then
/// by line.
pub(crate) fn group_by_file(notes: &[DiffNote]) -> Vec<(String, Vec<&DiffNote>)> {
    let mut groups: Vec<(String, Vec<&DiffNote>)> = Vec::new();
    for note in notes {
        match groups.iter_mut().find(|(path, _)| *path == note.file_path) {
            Some((_, list)) => list.push(note),
            None => groups.push((note.file_path.clone(), vec![note])),
        }
    }
    groups.sort_by(|a, b| a.0.cmp(&b.0));
    for (_, list) in &mut groups {
        list.sort_by_key(|note| position(note));
    }
    groups
}

/// Does `note` belong to the diff of `path` (or its old path when renamed) in `source`?
pub(crate) fn matches_view(note: &DiffNote, path: &str, old_path: Option<&str>, source: &NoteSource) -> bool {
    note.source == *source
        && (note.file_path == path
            || old_path.is_some_and(|old| note.file_path == old)
            || note.old_path.as_deref() == Some(path))
}

/// Which compare source to open for a note: its own while that diff still exists, else the
/// working diff.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LiveFiles {
    pub unstaged: bool,
    pub staged: bool,
    pub conflict: bool,
}

pub(crate) fn open_source(note: &DiffNote, live: LiveFiles) -> CompareSource {
    let available = match note.source {
        NoteSource::Unstaged => live.unstaged,
        NoteSource::Staged => live.staged,
        NoteSource::Conflict => live.conflict,
        NoteSource::Commit { .. } | NoteSource::Range { .. } => true,
    };
    if available { note.source.to_compare() } else { CompareSource::Working }
}

// ---- notes <-> rows --------------------------------------------------------------------------

/// `(line number, on the old side)`.
pub(crate) type LineKey = (u32, bool);

/// Where each note line lives in the current projection.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct RowAnchors {
    /// Line -> (projection row, aligned row). Lines hidden in a fold map to the fold row.
    rows: HashMap<LineKey, (usize, usize)>,
}

impl RowAnchors {
    pub(crate) fn build(comparison: &Comparison, projection: &Projection) -> Self {
        let mut rows = HashMap::new();
        let mut add = |aligned: usize, row: usize, old: bool, new: bool| {
            let Some(pair) = comparison.row(aligned) else { return };
            if new {
                if let Some(right) = pair.right {
                    rows.entry((right.number as u32, false)).or_insert((row, aligned));
                }
            }
            if old {
                if let Some(left) = pair.left.filter(|left| left.kind != sich_git::compare::LineKind::Same) {
                    rows.entry((left.number as u32, true)).or_insert((row, aligned));
                }
            }
        };
        for row in 0..projection.len() {
            match projection.row(row) {
                Some(VisibleRow::Aligned(aligned)) => add(aligned, row, true, true),
                Some(VisibleRow::Removed(aligned)) => add(aligned, row, true, false),
                Some(VisibleRow::Added(aligned)) => add(aligned, row, false, true),
                Some(VisibleRow::Fold(fold)) => {
                    for aligned in fold.start..fold.start + fold.count {
                        add(aligned, row, true, true);
                    }
                }
                None => {}
            }
        }
        Self { rows }
    }

    pub(crate) fn row_of(&self, key: LineKey) -> Option<usize> {
        self.rows.get(&key).map(|&(row, _)| row)
    }

    pub(crate) fn contains(&self, key: LineKey) -> bool {
        self.rows.contains_key(&key)
    }

    /// The text of lines `start..=end` on one side, joined with newlines, capped.
    pub(crate) fn text_of(&self, comparison: &Comparison, start: u32, end: u32, deleted: bool) -> String {
        let lines: Vec<String> = (start.min(end)..=start.max(end))
            .filter_map(|number| {
                let &(_, aligned) = self.rows.get(&(number, deleted))?;
                let row = comparison.row(aligned)?;
                let line = if deleted { row.left } else { row.right }?;
                Some(line.text.trim_end_matches(['\r', '\n']).to_owned())
            })
            .collect();
        cap_text(&lines.join("\n"))
    }
}

/// A line note whose line is not in the diff any more. File-level notes never are.
pub(crate) fn is_outdated(anchors: &RowAnchors, note: &DiffNote) -> bool {
    note.line != 0 && !anchors.contains((note.line, note.deleted))
}

#[cfg(test)]
pub(super) mod tests {
    use std::collections::HashSet;

    use sich_git::compare::compare_text;

    use super::*;

    pub(crate) fn note(id: &str, file: &str, line: u32) -> DiffNote {
        DiffNote {
            id: id.into(),
            worktree: PathBuf::from("/repo"),
            file_path: file.into(),
            old_path: None,
            source: NoteSource::Unstaged,
            start_line: None,
            line,
            deleted: false,
            selected_text: "code".into(),
            body: "body".into(),
            created_ms: 10,
            updated_ms: 10,
            sent_ms: None,
        }
    }

    #[test]
    fn worktree_hash_is_stable_and_normalized() {
        let a = worktree_hash(Path::new("/work/repo"));
        assert_eq!(a, worktree_hash(Path::new("/work/repo/")));
        assert_eq!(a, worktree_hash(Path::new("\\work\\repo")));
        assert_ne!(a, worktree_hash(Path::new("/work/other")));
        assert_eq!(a.len(), 16);
    }

    #[test]
    fn store_round_trip_in_temp_dir() {
        let dir = tempfile::tempdir().unwrap();
        let path = notes_file(&dir.path().join("notes"), Path::new("/repo"));
        assert!(load_notes(&path).is_empty());
        let mut second = note("b", "src/b.rs", 0);
        second.source = NoteSource::Range { base: "aa".into(), head: "bb".into() };
        second.sent_ms = Some(5);
        let notes = vec![note("a", "src/a.rs", 3), second];
        save_notes(&path, &notes).unwrap();
        assert_eq!(load_notes(&path), notes);
        assert!(!path.with_extension("json.tmp").exists());
        save_notes(&path, &[]).unwrap();
        assert!(!path.exists());
        save_notes(&path, &[]).unwrap();
    }

    #[test]
    fn load_is_tolerant_of_garbage_and_foreign_schemas() {
        assert!(parse_notes("not json").is_empty());
        assert!(parse_notes("").is_empty());
        assert!(parse_notes("[1,2]").is_empty());
        assert!(parse_notes(r#"{"version":1}"#).is_empty());
        let good = serialize_notes(&[note("a", "f", 1)]);
        let mut value: serde_json::Value = serde_json::from_str(&good).unwrap();
        value["notes"].as_array_mut().unwrap().push(serde_json::json!({"id": 5}));
        value["version"] = serde_json::json!(99);
        value["extra"] = serde_json::json!(true);
        let parsed = parse_notes(&value.to_string());
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].id, "a");
        let minimal = r#"{"version":0,"notes":[{"id":"x","worktree":"/r","file_path":"f","source":{"kind":"staged"}}]}"#;
        let parsed = parse_notes(minimal);
        assert_eq!(parsed[0].source, NoteSource::Staged);
        assert_eq!((parsed[0].line, parsed[0].body.as_str(), parsed[0].sent_ms), (0, "", None));
    }

    #[test]
    fn source_converts_both_ways() {
        for source in [
            CompareSource::Working,
            CompareSource::Staged,
            CompareSource::Commit("abc".into()),
            CompareSource::Range { base: "a".into(), head: "b".into() },
            CompareSource::Conflict,
        ] {
            assert_eq!(NoteSource::from_compare(&source).to_compare(), source);
        }
    }

    #[test]
    fn ids_are_unique_and_editing_clears_sent() {
        let mut seq = 0;
        let mut notes = vec![note("n1-1", "f", 1)];
        let a = fresh_id(&notes, 1, &mut seq);
        notes.push(note(&a, "f", 2));
        let b = fresh_id(&notes, 1, &mut seq);
        assert_ne!(a, b);
        mark_sent(&mut notes, &[a.clone()], 50);
        assert_eq!(notes[1].sent_ms, Some(50));
        assert!(!edit_body(&mut notes, &a, "body", 60), "same body is not an edit");
        assert_eq!(notes[1].sent_ms, Some(50));
        assert!(edit_body(&mut notes, &a, "new", 60));
        assert_eq!((notes[1].body.as_str(), notes[1].updated_ms, notes[1].sent_ms), ("new", 60, None));
        assert!(!edit_body(&mut notes, "missing", "x", 1));
        assert!(delete_note(&mut notes, &a));
        assert!(!delete_note(&mut notes, &a));
    }

    #[test]
    fn spans_and_grouping() {
        let mut range = note("r", "b.rs", 9);
        range.start_line = Some(7);
        let mut reversed = note("q", "b.rs", 3);
        reversed.start_line = Some(5);
        let mut single = note("s", "b.rs", 4);
        single.start_line = Some(4);
        assert_eq!(span(&range), NoteSpan::Lines(7, 9));
        assert_eq!(span(&reversed), NoteSpan::Lines(3, 5));
        assert_eq!(span(&single), NoteSpan::Line(4));
        assert_eq!(span(&note("f", "a.rs", 0)), NoteSpan::File);
        let notes = vec![range, note("late", "b.rs", 1), note("file", "b.rs", 0), note("a", "a.rs", 8), single];
        let groups = group_by_file(&notes);
        assert_eq!(groups.iter().map(|(path, _)| path.as_str()).collect::<Vec<_>>(), ["a.rs", "b.rs"]);
        let order: Vec<_> = groups[1].1.iter().map(|note| note.id.as_str()).collect();
        assert_eq!(order, ["file", "late", "s", "r"]);
    }

    #[test]
    fn views_match_by_path_rename_and_source() {
        let n = note("a", "new.rs", 1);
        assert!(matches_view(&n, "new.rs", None, &NoteSource::Unstaged));
        assert!(!matches_view(&n, "new.rs", None, &NoteSource::Staged));
        assert!(!matches_view(&n, "other.rs", None, &NoteSource::Unstaged));
        assert!(matches_view(&n, "other.rs", Some("new.rs"), &NoteSource::Unstaged));
    }

    #[test]
    fn open_source_falls_back_to_the_working_diff() {
        let mut n = note("a", "f", 1);
        n.source = NoteSource::Staged;
        assert_eq!(open_source(&n, LiveFiles::default()), CompareSource::Working);
        assert_eq!(open_source(&n, LiveFiles { staged: true, ..Default::default() }), CompareSource::Staged);
        n.source = NoteSource::Commit { oid: "abc".into() };
        assert_eq!(open_source(&n, LiveFiles::default()), CompareSource::Commit("abc".into()));
    }

    #[test]
    fn cap_text_cuts_on_a_char_boundary() {
        let text = "é".repeat(TEXT_CAP);
        let capped = cap_text(&text);
        assert!(capped.len() <= TEXT_CAP);
        assert!(text.starts_with(&capped));
        assert_eq!(cap_text("short"), "short");
    }

    #[test]
    fn anchors_find_rows_text_and_outdated_notes() {
        let old = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\n";
        let new = "a\nb\nc\nd\ne\nF\ng\nh\ni\nj\nk\nl\nm\n";
        let comparison = compare_text(old.into(), new.into());
        let projection = comparison.project(false, &HashSet::new());
        let anchors = RowAnchors::build(&comparison, &projection);
        assert!(anchors.contains((6, false)));
        assert!(anchors.contains((6, true)), "old side of a changed pair");
        assert!(!anchors.contains((7, true)), "unchanged old lines are not anchors");
        assert!(anchors.contains((13, false)));
        assert!(!anchors.contains((14, false)));
        assert_eq!(anchors.text_of(&comparison, 6, 6, false), "F");
        assert_eq!(anchors.text_of(&comparison, 6, 6, true), "f");
        assert_eq!(anchors.text_of(&comparison, 12, 13, false), "l\nm");
        let mut gone = note("g", "f", 40);
        assert!(is_outdated(&anchors, &gone));
        gone.line = 6;
        assert!(!is_outdated(&anchors, &gone));
        gone.line = 0;
        assert!(!is_outdated(&anchors, &gone));
        // A line hidden in a fold maps to the fold row, which is a visible row.
        let folded = anchors.row_of((1, false));
        assert!(folded.is_some_and(|row| row < projection.len()));
        // Inline mode separates the old and the new side of the changed line.
        let inline = comparison.project(true, &HashSet::new());
        let anchors = RowAnchors::build(&comparison, &inline);
        assert_ne!(anchors.row_of((6, true)), anchors.row_of((6, false)));
    }
}
