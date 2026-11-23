//! Review notes written on diff lines. One GPUI entity (a global, shared by every diff tab and
//! the Git tab) keeps the notes of each worktree, loaded lazily from
//! `<config>/sich/notes/<hash>.json` and saved after every change.

pub(crate) mod model;
pub(crate) mod report;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use gpui_kit::{App, AppContext as _, Context, Entity, Global};

pub(crate) use self::model::{DiffNote, NoteSource};
use self::model::{delete_note, edit_body, fresh_id, load_notes, mark_sent, notes_file, save_notes};

/// A request to show a note's line, handed from the Git tab to the diff tab that opens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Reveal {
    pub worktree: PathBuf,
    pub file_path: String,
    pub source: NoteSource,
    pub line: u32,
    pub deleted: bool,
}

/// The notes of every worktree touched so far, backed by one directory. No GPUI: tested directly.
pub(crate) struct NotesBook {
    dir: Option<PathBuf>,
    notes: HashMap<PathBuf, Vec<DiffNote>>,
    seq: u64,
}

impl NotesBook {
    pub(crate) fn new(dir: Option<PathBuf>) -> Self {
        Self { dir, notes: HashMap::new(), seq: 0 }
    }

    fn file(&self, worktree: &Path) -> Option<PathBuf> {
        self.dir.as_deref().map(|dir| notes_file(dir, worktree))
    }

    fn list(&mut self, worktree: &Path) -> &mut Vec<DiffNote> {
        if !self.notes.contains_key(worktree) {
            let loaded = self.file(worktree).map(|path| load_notes(&path)).unwrap_or_default();
            self.notes.insert(worktree.to_path_buf(), loaded);
        }
        self.notes.get_mut(worktree).expect("just inserted")
    }

    pub(crate) fn notes(&mut self, worktree: &Path) -> &[DiffNote] {
        self.list(worktree)
    }

    fn save(&self, worktree: &Path) {
        let (Some(path), Some(notes)) = (self.file(worktree), self.notes.get(worktree)) else { return };
        if let Err(error) = save_notes(&path, notes) {
            tracing::warn!("could not save notes to {}: {error}", path.display());
        }
    }

    /// Adds `note` (its `id` is replaced by a fresh one) and returns the id.
    pub(crate) fn add(&mut self, worktree: &Path, mut note: DiffNote, now_ms: i64) -> String {
        let mut seq = self.seq;
        let list = self.list(worktree);
        note.id = fresh_id(list, now_ms, &mut seq);
        note.worktree = worktree.to_path_buf();
        note.created_ms = now_ms;
        note.updated_ms = now_ms;
        note.sent_ms = None;
        let id = note.id.clone();
        list.push(note);
        self.seq = seq;
        self.save(worktree);
        id
    }

    pub(crate) fn edit(&mut self, worktree: &Path, id: &str, body: &str, now_ms: i64) -> bool {
        let changed = edit_body(self.list(worktree), id, body, now_ms);
        if changed {
            self.save(worktree);
        }
        changed
    }

    pub(crate) fn delete(&mut self, worktree: &Path, id: &str) -> bool {
        let changed = delete_note(self.list(worktree), id);
        if changed {
            self.save(worktree);
        }
        changed
    }

    pub(crate) fn clear(&mut self, worktree: &Path) {
        self.list(worktree).clear();
        self.save(worktree);
    }

    pub(crate) fn mark_sent(&mut self, worktree: &Path, ids: &[String], now_ms: i64) {
        mark_sent(self.list(worktree), ids, now_ms);
        self.save(worktree);
    }
}

pub(crate) struct NotesStore {
    book: NotesBook,
    reveal: Option<Reveal>,
}

struct NotesHandle(Entity<NotesStore>);

impl Global for NotesHandle {}

impl NotesStore {
    /// The shared store (created on first use).
    pub(crate) fn global(cx: &mut App) -> Entity<Self> {
        if let Some(handle) = cx.try_global::<NotesHandle>() {
            return handle.0.clone();
        }
        let store = cx.new(|_| Self { book: NotesBook::new(model::default_dir()), reveal: None });
        cx.set_global(NotesHandle(store.clone()));
        store
    }

    /// A copy of the notes of `worktree`.
    pub(crate) fn notes(&mut self, worktree: &Path) -> Vec<DiffNote> {
        self.book.notes(worktree).to_vec()
    }

    pub(crate) fn add(&mut self, worktree: &Path, note: DiffNote, now_ms: i64, cx: &mut Context<Self>) {
        self.book.add(worktree, note, now_ms);
        cx.notify();
    }

    pub(crate) fn edit(&mut self, worktree: &Path, id: &str, body: &str, now_ms: i64, cx: &mut Context<Self>) {
        if self.book.edit(worktree, id, body, now_ms) {
            cx.notify();
        }
    }

    pub(crate) fn delete(&mut self, worktree: &Path, id: &str, cx: &mut Context<Self>) {
        if self.book.delete(worktree, id) {
            cx.notify();
        }
    }

    pub(crate) fn clear(&mut self, worktree: &Path, cx: &mut Context<Self>) {
        self.book.clear(worktree);
        cx.notify();
    }

    pub(crate) fn mark_sent(&mut self, worktree: &Path, ids: &[String], now_ms: i64, cx: &mut Context<Self>) {
        self.book.mark_sent(worktree, ids, now_ms);
        cx.notify();
    }

    pub(crate) fn request_reveal(&mut self, reveal: Reveal, cx: &mut Context<Self>) {
        self.reveal = Some(reveal);
        cx.notify();
    }

    pub(crate) fn pending_reveal(&self) -> Option<&Reveal> {
        self.reveal.as_ref()
    }

    pub(crate) fn take_reveal(&mut self) -> Option<Reveal> {
        self.reveal.take()
    }
}

#[cfg(test)]
mod tests {
    use super::model::tests::note;
    use super::*;

    #[test]
    fn book_persists_across_instances_and_is_isolated_per_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (Path::new("/work/a"), Path::new("/work/b"));
        let mut book = NotesBook::new(Some(dir.path().to_path_buf()));
        let id = book.add(a, note("ignored", "f.rs", 3), 100);
        let other = book.add(a, note("ignored", "g.rs", 4), 100);
        assert_ne!(id, other);
        let only_b = book.add(b, note("x", "h.rs", 1), 100);
        assert_eq!(book.notes(a).len(), 2);
        assert_eq!(book.notes(a)[0].worktree, a);

        let mut again = NotesBook::new(Some(dir.path().to_path_buf()));
        assert_eq!(again.notes(a).len(), 2);
        assert_eq!(again.notes(b).len(), 1);
        again.mark_sent(a, &[id.clone()], 200);
        assert!(again.edit(a, &other, "changed", 300));
        assert!(again.delete(b, &only_b));

        let mut third = NotesBook::new(Some(dir.path().to_path_buf()));
        assert_eq!(third.notes(a).iter().find(|n| n.id == id).unwrap().sent_ms, Some(200));
        assert_eq!(third.notes(a).iter().find(|n| n.id == other).unwrap().body, "changed");
        assert!(third.notes(b).is_empty());
        assert!(!notes_file(dir.path(), b).exists(), "an emptied worktree leaves no file");
        third.clear(a);
        assert!(NotesBook::new(Some(dir.path().to_path_buf())).notes(a).is_empty());
    }

    #[test]
    fn editing_after_sending_makes_the_note_unsent() {
        let dir = tempfile::tempdir().unwrap();
        let wt = Path::new("/w");
        let mut book = NotesBook::new(Some(dir.path().to_path_buf()));
        let id = book.add(wt, note("x", "f", 1), 1);
        book.mark_sent(wt, &[id.clone()], 2);
        assert_eq!(book.notes(wt)[0].sent_ms, Some(2));
        assert!(book.edit(wt, &id, "other", 3));
        assert_eq!(book.notes(wt)[0].sent_ms, None);
    }

    #[test]
    fn book_without_a_directory_works_in_memory() {
        let mut book = NotesBook::new(None);
        book.add(Path::new("/w"), note("x", "f", 1), 1);
        assert_eq!(book.notes(Path::new("/w")).len(), 1);
    }
}
