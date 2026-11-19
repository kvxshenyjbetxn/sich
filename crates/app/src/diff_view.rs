//! Background comparison state and virtualized side-by-side and inline views.

mod render;

use std::collections::HashSet;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;

use gpui_kit::{
    AppContext as _, Context, Entity, FocusHandle, ScrollStrategy, UniformListScrollHandle, Window,
};
use sich_git::compare::{
    CompareContent, CompareKey, CompareSource, DiffLineView, Fold, Projection,
};
use sich_git::{GitError, Repo};

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
        Self {
            id,
            worktree,
            repo,
            key,
            original,
            panel,
            focus: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            expanded: HashSet::new(),
            projection: Arc::default(),
            mode: settings::get(cx).diff_mode,
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
                    view.projection = if view.mode == mode && view.expanded == expanded {
                        Arc::new(projection)
                    } else {
                        Arc::new(project(&content, view.mode, &view.expanded))
                    };
                    view.change = None;
                    view.comparison = Some(Arc::new(content));
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
            self.projection = Arc::new(project(content, self.mode, &self.expanded));
        }
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
            cx.notify();
        }
    }

    fn navigate(&mut self, next: bool, cx: &mut Context<Self>) {
        let offset = self.scroll.0.borrow().base_handle.offset().y;
        let top = (-f32::from(offset) / render::ROW_H).max(0.) as usize;
        if let Some(row) = change_destination(self.projection.changes(), self.change, top, next) {
            self.change = Some(row);
            self.scroll
                .scroll_to_item_strict(row, ScrollStrategy::Center);
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

fn code_text(line: DiffLineView<'_>) -> (String, Vec<Range<usize>>) {
    let mut text = String::new();
    let mut highlights: Vec<Range<usize>> = Vec::new();
    let mut emphasis = line.emphasis.unwrap_or_default().iter().peekable();
    let whole_line = line.emphasis.is_none() && line.kind == sich_git::compare::LineKind::Changed;
    // The view clips horizontally. Bound text shaping work for very long single lines.
    for (offset, ch) in line.text.char_indices().take(4096) {
        while emphasis.peek().is_some_and(|range| range.end <= offset) {
            emphasis.next();
        }
        let emphasized = emphasis.peek().is_some_and(|range| range.start <= offset);
        let start = text.len();
        match ch {
            '\t' => text.push_str("    "),
            ch if ch.is_control() => continue,
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
    fn code_text_expands_tabs_and_remaps_highlights_around_control_characters() {
        let line = DiffLineView {
            number: 1,
            text: "\tπ\r\x1b界\u{85}\n",
            kind: LineKind::Changed,
            emphasis: Some(&[1..8]),
        };
        assert_eq!(code_text(line), ("    π界".into(), vec![4..9]));
        let line = DiffLineView {
            text: "same\r\n",
            emphasis: Some(&[4..6]),
            ..line
        };
        assert_eq!(code_text(line), ("same".into(), Vec::new()));
        let line = DiffLineView {
            text: "\0before\x07\u{7f}after",
            kind: LineKind::Same,
            emphasis: None,
            ..line
        };
        assert_eq!(code_text(line), ("beforeafter".into(), Vec::new()));
    }
}
