//! Background comparison state and virtualized side-by-side and inline views.

mod render;
mod scroll;

use std::collections::HashSet;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;

use gpui_kit::{
    AppContext as _, Context, Entity, FocusHandle, ListAlignment, ListOffset, ListState,
    ScrollStrategy, UniformListScrollHandle, Window, point, px,
};
use sich_git::compare::{
    CompareContent, CompareKey, CompareSource, DiffLineView, Fold, Projection, display_columns,
};
use sich_git::{GitError, Repo};

use self::scroll::{HorizontalGeometry, ScrollDrag};
use crate::git_panel::GitPanel;
use crate::i18n::{Language, Tr, tr};
use crate::settings::{self, DiffMode};

pub(crate) struct DiffView {
    id: u64,
    worktree: PathBuf,
    repo: Repo,
    key: CompareKey,
    original: Option<String>,
    panel: Entity<GitPanel>,
    focus: FocusHandle,
    scroll: UniformListScrollHandle,
    wrapped_scroll: Option<ListState>,
    horizontal_offset: f32,
    drag: Option<ScrollDrag>,
    body_width: f32,
    advance: Option<f32>,
    expanded: HashSet<Fold>,
    projection: Arc<Projection>,
    mode: DiffMode,
    comparison: Option<Arc<Result<CompareContent, GitError>>>,
    loading: Reload,
    busy: bool,
    mutation_error: Option<String>,
    change: Option<usize>,
}

#[derive(Default)]
struct Reload {
    running: bool,
    queued: bool,
}

impl Reload {
    fn begin(&mut self) -> bool {
        if self.running {
            self.queued = true;
            false
        } else {
            self.running = true;
            true
        }
    }

    fn finish(&mut self) -> bool {
        self.running = false;
        std::mem::take(&mut self.queued)
    }
}

impl DiffView {
    pub fn new(
        id: u64,
        worktree: PathBuf,
        repo: Repo,
        key: CompareKey,
        original: Option<String>,
        panel: Entity<GitPanel>,
        cx: &mut Context<Self>,
    ) -> Self {
        let busy = panel.read(cx).operation_busy(&worktree);
        if key.source.is_live() {
            cx.observe(&panel, |view, panel, cx| {
                let busy = panel.read(cx).operation_busy(&view.worktree);
                if busy != view.busy {
                    view.busy = busy;
                    cx.notify();
                }
            })
            .detach();
        }
        let settings = settings::get(cx);
        Self {
            id,
            worktree,
            repo,
            key,
            original,
            panel,
            focus: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            wrapped_scroll: settings.diff_wrap_lines.then(|| wrapped_list(0)),
            horizontal_offset: 0.,
            drag: None,
            body_width: 0.,
            advance: None,
            expanded: HashSet::new(),
            projection: Arc::default(),
            mode: settings.diff_mode,
            comparison: None,
            loading: Reload::default(),
            busy,
            mutation_error: None,
            change: None,
        }
    }

    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window, cx);
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        if !self.loading.begin() {
            return;
        }
        let repo = self.repo.clone();
        let key = self.key.clone();
        let original = self.original.clone();
        let mode = self.mode;
        let expanded = self.expanded.clone();
        let task = cx.background_spawn(async move {
            let content = repo.compare(&key, original.as_deref());
            let projection = project(&content, mode, &expanded);
            (content, projection, expanded)
        });
        cx.spawn(async move |view, cx| {
            let (content, projection, expanded) = task.await;
            let _ = view.update(cx, |view, cx| {
                if view.comparison.as_deref() != Some(&content) {
                    let projection = if view.mode == mode && view.expanded == expanded {
                        projection
                    } else {
                        project(&content, view.mode, &view.expanded)
                    };
                    view.replace_projection(projection);
                    view.comparison = Some(Arc::new(content));
                    view.clamp_horizontal_offset();
                    cx.notify();
                }
                if view.loading.finish() {
                    view.reload(cx);
                }
            });
        })
        .detach();
    }

    pub fn refresh_live(&mut self, original: Option<Option<String>>, cx: &mut Context<Self>) {
        if let Some(original) = original {
            self.original = original;
        }
        self.reload(cx);
    }

    pub fn mutation_finished(&mut self, error: Option<String>, cx: &mut Context<Self>) {
        self.mutation_error = error;
        cx.notify();
    }

    fn rebuild_projection(&mut self) {
        if let Some(content) = &self.comparison {
            self.replace_projection(project(content, self.mode, &self.expanded));
        }
    }

    fn replace_projection(&mut self, projection: Projection) {
        if let Some(list) = &self.wrapped_scroll {
            if list.item_count() == projection.len() {
                list.remeasure();
            } else {
                let top = list.logical_scroll_top();
                list.reset(projection.len());
                list.scroll_to(top);
            }
        }
        self.projection = Arc::new(projection);
        self.change = None;
    }

    fn expand(&mut self, fold: Fold, cx: &mut Context<Self>) {
        if self.expanded.insert(fold) {
            self.rebuild_projection();
            cx.notify();
        }
    }

    fn choose_mode(&mut self, mode: DiffMode, cx: &mut Context<Self>) {
        settings::set_diff_mode(mode, cx);
        if self.mode != mode {
            self.mode = mode;
            self.rebuild_projection();
            self.clamp_horizontal_offset();
            cx.notify();
        }
    }

    fn vertical_anchor(&self) -> ListOffset {
        if let Some(list) = &self.wrapped_scroll {
            return list.logical_scroll_top();
        }
        let offset = self.scroll.0.borrow().base_handle.offset().y;
        fixed_anchor(-f32::from(offset))
    }

    fn toggle_wrap(&mut self, cx: &mut Context<Self>) {
        let anchor = self.vertical_anchor();
        let wrap = self.wrapped_scroll.is_none();
        settings::set_diff_wrap_lines(wrap, cx);
        self.drag = None;
        if wrap {
            let list = wrapped_list(self.projection.len());
            list.scroll_to(anchor);
            self.wrapped_scroll = Some(list);
        } else {
            self.wrapped_scroll = None;
            let handle = self.scroll.0.borrow().base_handle.clone();
            handle.set_offset(point(px(0.), px(-fixed_offset(anchor))));
            self.clamp_horizontal_offset();
        }
        cx.notify();
    }

    fn navigate(&mut self, next: bool, cx: &mut Context<Self>) {
        let top = self.vertical_anchor().item_ix;
        if let Some(row) = change_destination(self.projection.changes(), self.change, top, next) {
            self.change = Some(row);
            if let Some(list) = &self.wrapped_scroll {
                list.scroll_to(ListOffset {
                    item_ix: row,
                    offset_in_item: px(0.),
                });
            } else {
                self.scroll
                    .scroll_to_item_strict(row, ScrollStrategy::Center);
            }
            cx.notify();
        }
    }

    fn measure_advance(&mut self, window: &Window) {
        if self.advance.is_none() {
            let text = window.text_system();
            let font = text.resolve_font(&gpui_kit::font(render::FONT));
            let size = render::FONT_SIZE;
            let advance = text
                .advance(font, px(size), '0')
                .ok()
                .map(|size| f32::from(size.width));
            self.advance = Some(advance.filter(|&width| width > 0.).unwrap_or(size * 0.6));
        }
    }

    fn geometry(&self) -> HorizontalGeometry {
        let columns = match self.comparison.as_deref() {
            Some(Ok(CompareContent::Text(comparison))) => comparison.longest_line_columns(),
            _ => 0,
        };
        HorizontalGeometry {
            content: columns as f32 * self.advance.unwrap_or_default(),
            viewport: render::code_viewport_width(self.body_width, self.mode),
        }
    }

    fn clamp_horizontal_offset(&mut self) {
        self.horizontal_offset = self.geometry().clamp(self.horizontal_offset);
    }

    fn set_body_width(&mut self, width: f32, cx: &mut Context<Self>) {
        if self.body_width != width {
            self.body_width = width;
            if self.wrapped_scroll.is_none() {
                self.clamp_horizontal_offset();
            }
            cx.notify();
        }
    }

    fn set_horizontal_offset(&mut self, offset: f32, cx: &mut Context<Self>) {
        let offset = self.geometry().clamp(offset);
        if self.horizontal_offset != offset {
            self.horizontal_offset = offset;
            cx.notify();
        }
    }

    fn stage_action(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let unstage = self.key.source == CompareSource::Staged;
        let mut paths = vec![self.key.path.clone()];
        paths.extend(self.original.clone());
        self.mutation_error = None;
        self.panel.update(cx, |panel, cx| {
            panel.stage_from_diff(
                self.id,
                self.repo.clone(),
                self.worktree.clone(),
                paths,
                unstage,
                window,
                cx,
            );
        });
        cx.notify();
    }
}

fn wrapped_list(count: usize) -> ListState {
    ListState::new(count, ListAlignment::Top, px(160.))
}

fn fixed_anchor(offset: f32) -> ListOffset {
    let offset = offset.max(0.);
    let row_height = render::ROW_H;
    ListOffset {
        item_ix: (offset / row_height).floor() as usize,
        offset_in_item: px(offset % row_height),
    }
}

fn fixed_offset(anchor: ListOffset) -> f32 {
    let row_height = render::ROW_H;
    let within = f32::from(anchor.offset_in_item).clamp(0., row_height - 1.);
    anchor.item_ix as f32 * row_height + within
}

fn project(
    content: &Result<CompareContent, GitError>,
    mode: DiffMode,
    expanded: &HashSet<Fold>,
) -> Projection {
    match content {
        Ok(CompareContent::Text(comparison)) => {
            comparison.project(mode == DiffMode::Inline, expanded)
        }
        _ => Projection::default(),
    }
}

fn change_destination(
    changes: &[usize],
    last: Option<usize>,
    top: usize,
    next: bool,
) -> Option<usize> {
    let at = last.unwrap_or(top);
    if next {
        let index = changes.partition_point(|&row| row < at || last == Some(row));
        changes.get(index).or_else(|| changes.first()).copied()
    } else {
        let index = changes.partition_point(|&row| row < at || last.is_none() && row == at);
        index
            .checked_sub(1)
            .and_then(|index| changes.get(index))
            .or_else(|| changes.last())
            .copied()
    }
}

pub(crate) fn split_file_label(path: &str) -> (&str, &str) {
    path.rsplit_once('/')
        .map_or((path, ""), |(directory, name)| (name, directory))
}

pub(crate) fn source_label(source: &CompareSource, lang: Language) -> &str {
    match source {
        CompareSource::Working => tr(lang, Tr::DiffWorking),
        CompareSource::Staged => tr(lang, Tr::DiffStaged),
        CompareSource::Commit(oid) => oid.get(..7).unwrap_or(oid),
        CompareSource::Conflict => tr(lang, Tr::DiffConflict),
    }
}

fn code_text(
    line: DiffLineView<'_>,
    columns: Option<&Range<usize>>,
) -> (String, Vec<Range<usize>>) {
    let mut text = String::new();
    let mut highlights: Vec<Range<usize>> = Vec::new();
    let mut emphasis = line.emphasis.unwrap_or_default().iter().peekable();
    let whole_line = line.emphasis.is_none() && line.kind == sich_git::compare::LineKind::Changed;
    let columns = columns.cloned().unwrap_or(0..usize::MAX);
    let mut column = 0;
    for (offset, ch) in line.text.char_indices() {
        let start_column = column;
        let width = display_columns(ch);
        column += width;
        if start_column >= columns.end {
            break;
        }
        if width == 0 || column <= columns.start {
            continue;
        }
        while emphasis.peek().is_some_and(|range| range.end <= offset) {
            emphasis.next();
        }
        let emphasized = emphasis.peek().is_some_and(|range| range.start <= offset);
        let start = text.len();
        match ch {
            '\t' => {
                let visible = column.min(columns.end) - start_column.max(columns.start);
                text.push_str(&"    "[..visible]);
            }
            ch => text.push(ch),
        }
        if emphasized || whole_line {
            if let Some(prior) = highlights.last_mut().filter(|range| range.end == start) {
                prior.end = text.len();
            } else {
                highlights.push(start..text.len());
            }
        }
    }
    (text, highlights)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sich_git::compare::LineKind;

    #[test]
    fn navigation_uses_the_viewport_initially_and_wraps_after_last_selection() {
        assert_eq!(change_destination(&[], None, 0, true), None);
        assert_eq!(change_destination(&[2, 8, 15], None, 8, true), Some(8));
        assert_eq!(change_destination(&[2, 8, 15], None, 9, false), Some(8));
        assert_eq!(change_destination(&[2, 8, 15], Some(15), 9, true), Some(2));
        assert_eq!(change_destination(&[2, 8, 15], Some(2), 9, false), Some(15));
    }

    #[test]
    fn reload_coalesces_to_one_trailing_read() {
        let mut reads = Reload::default();
        assert!(reads.begin());
        assert!(!reads.begin());
        assert!(!reads.begin());
        assert!(reads.finish());
        assert!(reads.begin());
        assert!(!reads.finish());
    }

    #[test]
    fn code_text_keeps_lines_longer_than_four_thousand_characters() {
        let text = format!("{}last", "x".repeat(5000));
        let line = DiffLineView {
            number: 1,
            text: &text,
            kind: LineKind::Same,
            emphasis: None,
        };
        assert_eq!(code_text(line, None).0, text);
    }

    #[test]
    fn horizontal_window_can_shape_the_end_and_partial_tabs() {
        let text = format!("{}tail", "x".repeat(5000));
        let line = DiffLineView {
            number: 1,
            text: &text,
            kind: LineKind::Same,
            emphasis: None,
        };
        assert_eq!(code_text(line, Some(&(5000..5004))).0, "tail");
        let line = DiffLineView {
            text: "\tπ",
            emphasis: Some(&[0..3]),
            ..line
        };
        assert_eq!(code_text(line, Some(&(2..5))), ("  π".into(), vec![0..4]));
    }

    #[test]
    fn switching_wrap_keeps_the_top_logical_row() {
        let anchor = fixed_anchor(47.);
        assert_eq!(anchor.item_ix, 2);
        assert_eq!(anchor.offset_in_item, px(9.));
        assert_eq!(fixed_offset(anchor), 47.);
        let wrapped = ListOffset {
            item_ix: 2,
            offset_in_item: px(180.),
        };
        assert_eq!(fixed_anchor(fixed_offset(wrapped)).item_ix, 2);
    }

    #[test]
    fn code_text_expands_tabs_and_remaps_highlights_around_control_characters() {
        let line = DiffLineView {
            number: 1,
            text: "\tπ\r\x1b界\u{85}\n",
            kind: LineKind::Changed,
            emphasis: Some(&[1..8]),
        };
        assert_eq!(code_text(line, None), ("    π界".into(), vec![4..9]));
        let line = DiffLineView {
            text: "same\r\n",
            emphasis: Some(&[4..6]),
            ..line
        };
        assert_eq!(code_text(line, None), ("same".into(), Vec::new()));
        let line = DiffLineView {
            text: "\0before\x07\u{7f}after",
            kind: LineKind::Same,
            emphasis: None,
            ..line
        };
        assert_eq!(code_text(line, None), ("beforeafter".into(), Vec::new()));
    }
}
