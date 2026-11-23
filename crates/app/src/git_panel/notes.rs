//! The "Notes" shelf of the Git tab: review notes written on diff lines, grouped by file, with
//! "Send to agent" and "Copy all".

use gpui_kit::assets::IconName as Ic;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, ClipboardItem, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _, div, px,
};

use super::render::{chevron, tool_button};
use super::*;
use crate::diff_view::split_file_label;
use crate::notes_store::model::{LiveFiles, NoteSource, group_by_file, open_source, span};
use crate::notes_store::report::{agent_line, badge_text, file_count, note_block, notes_text, report};
use crate::notes_store::{DiffNote, NotesStore, Reveal};
use crate::sidebar::now_unix_ms;

/// State of the shelf inside the Git panel.
pub(super) struct NotesUi {
    pub(super) store: Entity<NotesStore>,
    collapsed: bool,
}

impl NotesUi {
    pub(super) fn new(cx: &mut App) -> Self {
        Self { store: NotesStore::global(cx), collapsed: false }
    }
}

/// `<sich data dir>/sich/agent-prompts`, honoring `SICH_DATA_DIR`.
fn prompts_dir() -> Option<PathBuf> {
    sich_daemon::data_local_dir().map(|dir| dir.join("sich").join("agent-prompts"))
}

impl GitPanel {
    pub(super) fn notes_section(&self, lang: Language, cx: &mut Context<Self>) -> Option<AnyElement> {
        let worktree = self.worktree.clone()?;
        let notes = self.notes.store.update(cx, |store, _| store.notes(&worktree));
        if notes.is_empty() {
            return None;
        }
        let theme = cx.theme().clone();
        let collapsed = self.notes.collapsed;
        let panel = cx.weak_entity();
        let clear_panel = panel.clone();
        let header = h_flex()
            .id("git-notes")
            .h(px(26.))
            .gap_1()
            .px_2()
            .items_center()
            .cursor_pointer()
            .bg(theme.list_head)
            .hover(|style| style.bg(theme.list_hover))
            .on_click(cx.listener(|panel, _, _, cx| {
                panel.notes.collapsed = !panel.notes.collapsed;
                cx.notify();
            }))
            .child(chevron(!collapsed))
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .truncate()
                    .text_size(px(12.))
                    .child(format!("{} {}", tr(lang, Tr::NotesTitle), notes.len())),
            )
            .child(tool_button("git-notes-send", Ic::Send, tr(lang, Tr::NotesSend)).on_click(cx.listener(
                |panel, _, _, cx| {
                    panel.send_notes(cx);
                    cx.stop_propagation();
                },
            )))
            .child(tool_button("git-notes-copy", Ic::Copy, tr(lang, Tr::NotesCopyAll)).on_click(cx.listener(
                |panel, _, _, cx| {
                    panel.copy_notes(cx);
                    cx.stop_propagation();
                },
            )))
            .child(
                tool_button("git-notes-more", Ic::Ellipsis, tr(lang, Tr::MoreActions)).dropdown_menu(
                    move |menu, _, _| {
                        let panel = clear_panel.clone();
                        menu.item(PopupMenuItem::new(tr(lang, Tr::NotesClearAll)).on_click(
                            move |_, window, cx| {
                                let _ = panel.update(cx, |panel, cx| {
                                    panel.confirm(
                                        Tr::NotesClearAll,
                                        tr(lang, Tr::NotesClearConfirm).to_owned(),
                                        true,
                                        lang,
                                        |panel, _, cx| panel.clear_notes(cx),
                                        window,
                                        cx,
                                    )
                                });
                            },
                        ))
                    },
                ),
            );
        let body = (!collapsed).then(|| {
            let groups = group_by_file(&notes);
            v_flex().children(groups.into_iter().enumerate().map(|(index, (path, list))| {
                self.notes_file_group(index, &path, &list, lang, cx)
            }))
        });
        Some(v_flex().id("git-notes-section").child(header).children(body).into_any_element())
    }

    fn notes_file_group(
        &self,
        index: usize,
        path: &str,
        notes: &[&DiffNote],
        lang: Language,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let (name, directory) = split_file_label(path);
        let first = notes.first().map(|note| (*note).clone());
        v_flex()
            .child(
                h_flex()
                    .id(("git-notes-file", index))
                    .h(px(24.))
                    .px_2()
                    .gap_1()
                    .items_center()
                    .cursor_pointer()
                    .hover(|style| style.bg(theme.list_hover))
                    .on_click(cx.listener(move |panel, _, _, cx| {
                        if let Some(note) = &first {
                            panel.open_note(note, cx);
                        }
                    }))
                    .child(div().min_w_0().truncate().text_size(px(12.)).child(name.to_owned()))
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .text_size(px(11.))
                            .text_color(theme.muted_foreground)
                            .child(directory.to_owned()),
                    ),
            )
            .children(notes.iter().map(|note| self.note_row(note, lang, cx)))
            .into_any_element()
    }

    fn note_row(&self, note: &DiffNote, lang: Language, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let group = SharedString::from(format!("git-note-{}", note.id));
        let (open, copy, delete) = (note.clone(), note.clone(), note.id.clone());
        let badge = |text: String, color| {
            div()
                .flex_shrink_0()
                .px_1()
                .rounded(px(3.))
                .text_size(px(10.))
                .text_color(color)
                .bg(theme.muted_foreground.opacity(0.15))
                .child(text)
        };
        h_flex()
            .id(SharedString::from(format!("{group}-row")))
            .group(group.clone())
            .w_full()
            .min_w_0()
            .gap_1()
            .pl_5()
            .pr_2()
            .py_1()
            .items_start()
            .cursor_pointer()
            .hover(|style| style.bg(theme.list_hover))
            .on_click(cx.listener(move |panel, _, _, cx| panel.open_note(&open, cx)))
            .child(badge(badge_text(lang, span(note)), theme.muted_foreground))
            .children(note.sent_ms.map(|_| badge(tr(lang, Tr::NotesSent).to_owned(), theme.green)))
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .whitespace_normal()
                    .text_size(px(12.))
                    .child(note.body.clone()),
            )
            .child(
                h_flex()
                    .flex_shrink_0()
                    .invisible()
                    .group_hover(group.clone(), |style| style.visible())
                    .child(
                        tool_button(SharedString::from(format!("{group}-copy")), Ic::Copy, tr(lang, Tr::NotesCopy))
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(note_block(&copy)));
                                cx.stop_propagation();
                            })),
                    )
                    .child(
                        tool_button(SharedString::from(format!("{group}-del")), Ic::Trash, tr(lang, Tr::NotesDelete))
                            .on_click(cx.listener(move |panel, _, _, cx| {
                                panel.delete_note(&delete, cx);
                                cx.stop_propagation();
                            })),
                    ),
            )
            .into_any_element()
    }

    /// Opens the diff of `note`'s file and scrolls to its line: its own diff while that is still
    /// available, else the working diff.
    fn open_note(&mut self, note: &DiffNote, cx: &mut Context<Self>) {
        let Some(worktree) = self.worktree.clone() else { return };
        let live = self.status.as_ref().map_or_else(LiveFiles::default, |status| {
            let has = |files: &[FileChange]| files.iter().any(|file| file.path == note.file_path);
            LiveFiles {
                unstaged: has(&status.unstaged),
                staged: has(&status.staged),
                conflict: has(&status.conflicted),
            }
        });
        let source = open_source(note, live);
        let key = CompareKey { path: note.file_path.clone(), source: source.clone() };
        let original = self.diff_rename(&worktree, &key).flatten();
        if note.line != 0 && NoteSource::from_compare(&source) == note.source {
            let reveal = Reveal {
                worktree,
                file_path: note.file_path.clone(),
                source: note.source.clone(),
                line: note.line,
                deleted: note.deleted,
            };
            self.notes.store.update(cx, |store, cx| store.request_reveal(reveal, cx));
        }
        self.open_diff(key, original, cx);
    }

    fn delete_note(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(worktree) = self.worktree.clone() {
            self.notes.store.update(cx, |store, cx| store.delete(&worktree, id, cx));
        }
    }

    fn clear_notes(&mut self, cx: &mut Context<Self>) {
        if let Some(worktree) = self.worktree.clone() {
            self.notes.store.update(cx, |store, cx| store.clear(&worktree, cx));
        }
    }

    fn copy_notes(&mut self, cx: &mut Context<Self>) {
        let Some(worktree) = self.worktree.clone() else { return };
        let notes = self.notes.store.update(cx, |store, _| store.notes(&worktree));
        if !notes.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(notes_text(&notes)));
        }
    }

    /// Writes the report file and pastes one line pointing at it into the agent terminal.
    fn send_notes(&mut self, cx: &mut Context<Self>) {
        let Some(worktree) = self.worktree.clone() else { return };
        let notes = self.notes.store.update(cx, |store, _| store.notes(&worktree));
        if notes.is_empty() {
            return;
        }
        let now = now_unix_ms();
        let written = prompts_dir()
            .ok_or_else(|| "No local data directory for the report".to_owned())
            .and_then(|dir| {
                sich_git::forge::write_report(&dir, 0, u128::from(now), &report(&worktree, &notes))
                    .map_err(|err| format!("Could not write the report to {}: {err}", dir.display()))
            });
        match written {
            Ok(path) => {
                let text = agent_line(notes.len(), file_count(&notes), &path);
                let ids: Vec<String> = notes.iter().map(|note| note.id.clone()).collect();
                self.notes
                    .store
                    .update(cx, |store, cx| store.mark_sent(&worktree, &ids, now as i64, cx));
                cx.emit(GitPanelEvent::SendToAgent { worktree, text });
            }
            Err(message) => self.pr_report_error(&worktree, message, cx),
        }
    }
}
