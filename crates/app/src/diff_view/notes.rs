//! Review notes inside a diff tab: the "+" in the gutter, the inline composer, the note cards
//! under their line and the strip of file-level and outdated notes at the top.

use std::collections::HashMap;
use std::sync::Arc;

use gpui_kit::assets::IconName as Ic;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, Theme, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, Context, ElementId, Entity, InteractiveElement as _, IntoElement, ListOffset,
    ParentElement as _, ScrollStrategy, SharedString, StatefulInteractiveElement as _, Styled as _,
    Focusable as _, Subscription, WeakEntity, Window, div, point, px,
};

use super::render::{GUTTER_W, ROW_H};
use super::*;
use crate::notes_store::model::{LineKey, NoteSpan, RowAnchors, is_outdated, matches_view, span};
use crate::notes_store::report::badge_text;
use crate::notes_store::{DiffNote, NoteSource};
use crate::sidebar::now_unix_ms;

/// What the diff needs to draw one note.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct NoteCard {
    pub id: String,
    pub body: String,
    pub span: NoteSpan,
    pub sent: bool,
    pub outdated: bool,
}

impl NoteCard {
    fn new(note: &DiffNote, outdated: bool) -> Self {
        Self { id: note.id.clone(), body: note.body.clone(), span: span(note), sent: note.sent_ms.is_some(), outdated }
    }
}

/// Where the notes (and the composer) of one diff go.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct NoteLayout {
    pub by_row: HashMap<usize, Vec<NoteCard>>,
    /// File-level and outdated notes, drawn above the code.
    pub top: Vec<NoteCard>,
    /// `Some(Some(row))`: the composer sits under `row`; `Some(None)`: at the top.
    pub composer: Option<Option<usize>>,
}

impl NoteLayout {
    /// `editing` is the note shown as the composer instead of a card; `composer` is the line the
    /// composer is on (line 0 = file level).
    pub(super) fn build(
        notes: &[DiffNote],
        anchors: &RowAnchors,
        editing: Option<&str>,
        composer: Option<LineKey>,
    ) -> Self {
        let mut layout = Self::default();
        for note in notes.iter().filter(|note| Some(note.id.as_str()) != editing) {
            if note.line == 0 {
                layout.top.push(NoteCard::new(note, false));
            } else if let Some(row) = anchors.row_of((note.line, note.deleted)) {
                layout.by_row.entry(row).or_default().push(NoteCard::new(note, false));
            } else {
                layout.top.push(NoteCard::new(note, is_outdated(anchors, note)));
            }
        }
        layout.composer = composer.map(|key| if key.0 == 0 { None } else { anchors.row_of(key) });
        layout
    }

    pub(super) fn has_inline(&self) -> bool {
        !self.by_row.is_empty() || matches!(self.composer, Some(Some(_)))
    }
}

/// The open composer: a new note on `start..=end`, or the edit of an existing one.
pub(super) struct Composer {
    anchor: u32,
    start: u32,
    end: u32,
    deleted: bool,
    selected: String,
    editing: Option<String>,
    pub state: Entity<TextareaState>,
    _subscription: Subscription,
}

impl Composer {
    fn key(&self) -> LineKey {
        (self.end, self.deleted)
    }
}

/// The "+" in a gutter.
#[derive(Clone)]
pub(super) struct GutterAdd {
    pub owner: WeakEntity<DiffView>,
    pub group: SharedString,
    pub key: LineKey,
    pub id: usize,
}

/// Range after a shift-click: from the first clicked line to the new one.
pub(super) fn extend_range(anchor: u32, clicked: u32) -> (u32, u32) {
    (anchor.min(clicked), anchor.max(clicked))
}

impl DiffView {
    pub(super) fn active_list(&self) -> Option<&ListState> {
        self.wrapped_scroll.as_ref().or(self.notes_list.then_some(&self.notes_scroll))
    }

    fn scroll_to_row(&self, row: usize) {
        if let Some(list) = self.active_list() {
            list.scroll_to(ListOffset { item_ix: row, offset_in_item: px(0.) });
        } else {
            self.scroll.scroll_to_item_strict(row, ScrollStrategy::Center);
        }
    }

    pub(super) fn refresh_anchors(&mut self) {
        self.anchors = Arc::new(match self.comparison.as_deref() {
            Some(Ok(CompareContent::Text(comparison))) => RowAnchors::build(comparison, &self.projection),
            _ => RowAnchors::default(),
        });
    }

    fn rows_loaded(&self) -> bool {
        matches!(self.comparison.as_deref(), Some(Ok(CompareContent::Text(_))))
    }

    fn source(&self) -> NoteSource {
        NoteSource::from_compare(&self.key.source)
    }

    /// Recomputes where notes go for this frame; switches between the fixed-height list and the
    /// variable-height one when notes appear or disappear; honors a pending "show this line".
    pub(super) fn sync_notes(&mut self, cx: &mut Context<Self>) {
        let source = self.source();
        let worktree = self.worktree.clone();
        let mine: Vec<DiffNote> = self
            .notes
            .update(cx, |store, _| store.notes(&worktree))
            .into_iter()
            .filter(|note| matches_view(note, &self.key.path, self.original.as_deref(), &source))
            .collect();
        let layout = if self.rows_loaded() {
            let editing = self.composer.as_ref().and_then(|c| c.editing.as_deref());
            NoteLayout::build(&mine, &self.anchors, editing, self.composer.as_ref().map(Composer::key))
        } else {
            NoteLayout::default()
        };
        let want_list = self.wrapped_scroll.is_none() && layout.has_inline();
        if want_list != self.notes_list {
            if want_list {
                let offset = self.scroll.0.borrow().base_handle.offset().y;
                self.notes_scroll.scroll_to(fixed_anchor(-f32::from(offset)));
            } else {
                let top = self.notes_scroll.logical_scroll_top().item_ix;
                let handle = self.scroll.0.borrow().base_handle.clone();
                let offset = fixed_offset(ListOffset { item_ix: top, offset_in_item: px(0.) });
                handle.set_offset(point(px(0.), px(-offset)));
            }
            self.notes_list = want_list;
        }
        if *self.layout != layout {
            self.layout = Arc::new(layout);
            self.notes_scroll.remeasure();
            if let Some(list) = &self.wrapped_scroll {
                list.remeasure();
            }
        }
        self.apply_reveal(cx);
    }

    fn apply_reveal(&mut self, cx: &mut Context<Self>) {
        if !self.rows_loaded() {
            return;
        }
        let source = self.source();
        let wanted = self.notes.read(cx).pending_reveal().filter(|reveal| {
            reveal.worktree == self.worktree && reveal.file_path == self.key.path && reveal.source == source
        });
        let Some(key) = wanted.map(|reveal| (reveal.line, reveal.deleted)) else { return };
        self.notes.update(cx, |store, _| store.take_reveal());
        if let Some(row) = self.anchors.row_of(key) {
            self.change = Some(row);
            self.scroll_to_row(row);
        }
    }

    // ---- composer ------------------------------------------------------------------------

    fn open_composer(
        &mut self,
        (start, end, deleted): (u32, u32, bool),
        editing: Option<(String, String)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let lang = settings::language(cx);
        let body = editing.as_ref().map(|(_, body)| body.clone()).unwrap_or_default();
        let state = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(3, 3)
                .placeholder(tr(lang, Tr::NotesPlaceholder))
                .default_value(body)
        });
        let subscription = cx.subscribe_in(&state, window, |view, _, event, window, cx| {
            if matches!(event, InputEvent::PressEnter { secondary: true, shift: false }) {
                view.save_composer(window, cx);
            }
        });
        let selected = match self.comparison.as_deref() {
            Some(Ok(CompareContent::Text(comparison))) if start != 0 => {
                self.anchors.text_of(comparison, start, end, deleted)
            }
            _ => String::new(),
        };
        state.read(cx).focus_handle(cx).focus(window, cx);
        self.composer = Some(Composer {
            anchor: start,
            start,
            end,
            deleted,
            selected,
            editing: editing.map(|(id, _)| id),
            state,
            _subscription: subscription,
        });
        cx.notify();
    }

    /// The "+" of a line was clicked (shift extends the open composer to a range).
    pub(super) fn begin_note(&mut self, key: LineKey, shift: bool, window: &mut Window, cx: &mut Context<Self>) {
        if shift {
            if let Some(composer) = self.composer.as_mut().filter(|c| c.editing.is_none() && c.deleted == key.1) {
                let (start, end) = extend_range(composer.anchor, key.0);
                composer.start = start;
                composer.end = end;
                if let Some(Ok(CompareContent::Text(comparison))) = self.comparison.as_deref() {
                    composer.selected = self.anchors.text_of(comparison, start, end, key.1);
                }
                cx.notify();
                return;
            }
        }
        self.open_composer((key.0, key.0, key.1), None, window, cx);
    }

    /// A note on the whole file.
    pub(super) fn begin_file_note(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_composer((0, 0, false), None, window, cx);
    }

    fn begin_edit(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let worktree = self.worktree.clone();
        let note = self.notes.update(cx, |store, _| store.notes(&worktree)).into_iter().find(|note| note.id == id);
        if let Some(note) = note {
            let start = note.start_line.map_or(note.line, |start| start.min(note.line));
            self.open_composer((start, note.line, note.deleted), Some((note.id, note.body)), window, cx);
        }
    }

    pub(super) fn cancel_composer(&mut self, cx: &mut Context<Self>) {
        if self.composer.take().is_some() {
            cx.notify();
        }
    }

    fn save_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(composer) = &self.composer else { return };
        let body = composer.state.read(cx).value().trim().to_owned();
        if body.is_empty() {
            return;
        }
        let worktree = self.worktree.clone();
        let now = now_unix_ms() as i64;
        if let Some(id) = &composer.editing {
            let id = id.clone();
            self.notes.update(cx, |store, cx| store.edit(&worktree, &id, &body, now, cx));
        } else {
            let note = DiffNote {
                id: String::new(),
                worktree: worktree.clone(),
                file_path: self.key.path.clone(),
                old_path: self.original.clone(),
                source: self.source(),
                start_line: (composer.start != composer.end).then_some(composer.start),
                line: composer.end,
                deleted: composer.deleted,
                selected_text: composer.selected.clone(),
                body,
                created_ms: now,
                updated_ms: now,
                sent_ms: None,
            };
            self.notes.update(cx, |store, cx| store.add(&worktree, note, now, cx));
        }
        self.composer = None;
        self.focus(window, cx);
        cx.notify();
    }

    fn delete_note(&mut self, id: &str, cx: &mut Context<Self>) {
        let worktree = self.worktree.clone();
        self.notes.update(cx, |store, cx| store.delete(&worktree, id, cx));
    }

    // ---- elements ------------------------------------------------------------------------

    /// The composer's text box and its line range, for drawing.
    /// The composer's text box and its line range, for drawing.
    pub(super) fn composer_parts(&self) -> Option<(Entity<TextareaState>, u32, u32)> {
        self.composer.as_ref().map(|c| (c.state.clone(), c.start, c.end))
    }

    /// File-level and outdated notes (and a composer without a line) above the code.
    pub(super) fn top_notes(&self, lang: Language, owner: &WeakEntity<DiffView>, cx: &App) -> Option<AnyElement> {
        let theme = cx.theme();
        let composer = matches!(self.layout.composer, Some(None))
            .then(|| self.composer_parts())
            .flatten()
            .map(|(state, start, end)| composer_box(&state, start, end, lang, owner.clone(), theme));
        if self.layout.top.is_empty() && composer.is_none() {
            return None;
        }
        Some(
            v_flex()
                .id("diff-notes-top")
                .flex_shrink_0()
                .max_h(px(220.))
                .overflow_y_scroll()
                .gap_1()
                .p_1()
                .border_b_1()
                .border_color(theme.border)
                .children(self.layout.top.iter().map(|card| card_element(card, 0., lang, owner.clone(), theme)))
                .children(composer)
                .into_any_element(),
        )
    }

    pub(super) fn note_buttons(&self, lang: Language, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        Button::new("diff-note-file")
            .ghost()
            .xsmall()
            .icon(Icon::new(Ic::MessageSquarePlus))
            .tooltip(tr(lang, Tr::NotesAdd))
            .on_click(cx.listener(|view, _, window, cx| {
                view.focus(window, cx);
                view.begin_file_note(window, cx);
            }))
    }
}

fn badge(text: String, theme: &Theme, color: gpui_kit::Hsla) -> impl IntoElement + use<> {
    div()
        .flex_shrink_0()
        .px_1()
        .rounded(px(3.))
        .text_size(px(10.))
        .text_color(color)
        .bg(theme.muted_foreground.opacity(0.15))
        .child(text)
}

fn icon_button(id: impl Into<ElementId>, icon: Ic, label: &'static str) -> Button {
    Button::new(id).ghost().xsmall().icon(Icon::new(icon)).tooltip(label)
}

/// A compact card under a line; `indent` is the left offset (the gutter) in pixels.
pub(super) fn card_element(
    card: &NoteCard,
    indent: f32,
    lang: Language,
    owner: WeakEntity<DiffView>,
    theme: &Theme,
) -> AnyElement {
    let (edit_owner, delete_owner) = (owner.clone(), owner);
    let (edit_id, delete_id) = (card.id.clone(), card.id.clone());
    h_flex()
        .w_full()
        .items_start()
        .pl(px(indent))
        .pr_1()
        .py_0p5()
        .child(
            h_flex()
                .flex_1()
                .min_w_0()
                .items_start()
                .gap_1()
                .px_2()
                .py_1()
                .rounded(px(4.))
                .border_l_2()
                .border_color(theme.primary)
                .bg(theme.muted.opacity(0.35))
                .text_size(px(12.))
                .child(badge(badge_text(lang, card.span), theme, theme.muted_foreground))
                .children(card.outdated.then(|| badge(tr(lang, Tr::NotesOutdated).to_owned(), theme, theme.red)))
                .children(card.sent.then(|| badge(tr(lang, Tr::NotesSent).to_owned(), theme, theme.green)))
                .child(div().min_w_0().flex_1().whitespace_normal().child(card.body.clone()))
                .child(
                    h_flex()
                        .flex_shrink_0()
                        .child(
                            icon_button(
                                SharedString::from(format!("note-edit-{}", card.id)),
                                Ic::Pencil,
                                tr(lang, Tr::NotesEdit),
                            )
                            .on_click(move |_, window, cx| {
                                let id = edit_id.clone();
                                let _ = edit_owner.update(cx, |view, cx| view.begin_edit(&id, window, cx));
                            }),
                        )
                        .child(
                            icon_button(
                                SharedString::from(format!("note-del-{}", card.id)),
                                Ic::Trash,
                                tr(lang, Tr::NotesDelete),
                            )
                            .on_click(move |_, _, cx| {
                                let id = delete_id.clone();
                                let _ = delete_owner.update(cx, |view, cx| view.delete_note(&id, cx));
                            }),
                        ),
                ),
        )
        .into_any_element()
}

pub(super) fn composer_box(
    state: &Entity<TextareaState>,
    start: u32,
    end: u32,
    lang: Language,
    owner: WeakEntity<DiffView>,
    theme: &Theme,
) -> AnyElement {
    let (save_owner, cancel_owner, key_owner) = (owner.clone(), owner.clone(), owner);
    let span = if end == 0 {
        NoteSpan::File
    } else if start != end {
        NoteSpan::Lines(start, end)
    } else {
        NoteSpan::Line(end)
    };
    v_flex()
        .w_full()
        .gap_1()
        .px_2()
        .py_1()
        .pl(px(GUTTER_W))
        .capture_key_down(move |event, _, cx| {
            if event.keystroke.key.eq_ignore_ascii_case("escape") {
                let _ = key_owner.update(cx, |view, cx| view.cancel_composer(cx));
                cx.stop_propagation();
            }
        })
        .child(div().text_size(px(11.)).text_color(theme.muted_foreground).child(badge_text(lang, span)))
        .child(Textarea::new(state).small().aria_label(tr(lang, Tr::NotesPlaceholder)))
        .child(
            h_flex()
                .gap_1()
                .justify_end()
                .child(
                    Button::new("note-cancel")
                        .ghost()
                        .xsmall()
                        .label(tr(lang, Tr::Cancel))
                        .on_click(move |_, _, cx| {
                            let _ = cancel_owner.update(cx, |view, cx| view.cancel_composer(cx));
                        }),
                )
                .child(
                    Button::new("note-save")
                        .primary()
                        .xsmall()
                        .label(tr(lang, Tr::NotesSave))
                        .on_click(move |_, window, cx| {
                            let _ = save_owner.update(cx, |view, cx| view.save_composer(window, cx));
                        }),
                ),
        )
        .into_any_element()
}

/// The "+" that appears in a gutter while its row is hovered.
pub(super) fn gutter_add(add: GutterAdd, theme: &Theme) -> impl IntoElement + use<> {
    let GutterAdd { owner, group, key, id } = add;
    div()
        .id(("diff-note-add", id))
        .absolute()
        .top_0()
        .left(px(6.))
        .w(px(16.))
        .h(px(ROW_H))
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .text_color(theme.primary)
        .invisible()
        .group_hover(group, |style| style.visible())
        .hover(|style| style.bg(theme.primary.opacity(0.2)))
        .child(Icon::new(Ic::Plus).size(px(12.)))
        .on_click(move |_, window, cx| {
            let shift = window.modifiers().shift;
            let _ = owner.update(cx, |view, cx| view.begin_note(key, shift, window, cx));
        })
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::path::PathBuf;

    use sich_git::compare::compare_text;

    use super::*;

    fn note(id: &str, line: u32) -> DiffNote {
        DiffNote {
            id: id.into(),
            worktree: PathBuf::from("/w"),
            file_path: "f".into(),
            old_path: None,
            source: NoteSource::Unstaged,
            start_line: None,
            line,
            deleted: false,
            selected_text: String::new(),
            body: id.into(),
            created_ms: 0,
            updated_ms: 0,
            sent_ms: None,
        }
    }

    #[test]
    fn range_extends_in_both_directions() {
        assert_eq!(extend_range(5, 9), (5, 9));
        assert_eq!(extend_range(5, 2), (2, 5));
        assert_eq!(extend_range(5, 5), (5, 5));
    }

    #[test]
    fn layout_places_cards_under_their_row_and_keeps_outdated_ones_on_top() {
        let old = "a\nb\nc\nd\n";
        let new = "a\nB\nc\nd\n";
        let comparison = compare_text(old.into(), new.into());
        let projection = comparison.project(false, &HashSet::new());
        let anchors = RowAnchors::build(&comparison, &projection);
        let row = anchors.row_of((2, false)).unwrap();
        let notes = vec![note("on-line", 2), note("gone", 99), note("file", 0)];
        let layout = NoteLayout::build(&notes, &anchors, None, None);
        assert_eq!(layout.by_row[&row].len(), 1);
        let top: Vec<_> = layout.top.iter().map(|card| (card.id.as_str(), card.outdated)).collect();
        assert_eq!(top, [("gone", true), ("file", false)]);
        assert!(layout.has_inline());
        assert_eq!(layout.composer, None);
    }

    #[test]
    fn composer_and_editing_are_part_of_the_layout() {
        let comparison = compare_text("a\n".into(), "b\n".into());
        let projection = comparison.project(false, &HashSet::new());
        let anchors = RowAnchors::build(&comparison, &projection);
        let notes = vec![note("n", 1)];
        let editing = NoteLayout::build(&notes, &anchors, Some("n"), Some((1, false)));
        assert!(editing.by_row.is_empty(), "the edited note is shown as the composer");
        assert_eq!(editing.composer, Some(anchors.row_of((1, false))));
        assert!(editing.has_inline());
        let file_level = NoteLayout::build(&[], &anchors, None, Some((0, false)));
        assert_eq!(file_level.composer, Some(None));
        assert!(!file_level.has_inline());
        assert_eq!(NoteLayout::build(&[], &anchors, None, None), NoteLayout::default());
    }
}
