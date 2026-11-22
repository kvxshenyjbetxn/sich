//! PR tab section: files changed by the pull request, with "viewed" checkboxes. A file opens as
//! a diff between the merge base and the PR head (needs the head commit locally).


use std::time::{Duration, Instant};

use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, InteractiveElement as _, IntoElement as _, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, div, px,
};
use sich_git::compare::{CompareKey, CompareSource};
use sich_git::forge::{FileStatus, PrFile, PullRequest, ViewedState, files_totals, viewed_count};

use super::render::chevron;
use super::*;
use crate::diff_view::split_file_label;
use crate::settings;

const FILES_REFRESH_AFTER: Duration = Duration::from_secs(30);
/// The filter input is only offered for longer lists.
const FILTER_FROM: usize = 12;
/// Rows drawn at once; the filter narrows a longer list down.
const MAX_ROWS: usize = 300;

#[derive(Default)]
pub(super) struct FilesEntry {
    pub files: Option<Vec<PrFile>>,
    pub viewed: ViewedState,
    /// `(merge_base, head)` when the head commit is available locally.
    pub range: Option<(String, String)>,
    pub head_oid: String,
    pub fetched_at: Option<Instant>,
    pub loading: bool,
    pub error: Option<String>,
    pub viewed_error: Option<String>,
}

impl FilesEntry {
    pub(super) fn needs_fetch(&self, head_oid: &str, now: Instant) -> bool {
        if self.loading {
            return false;
        }
        match self.fetched_at {
            None => true,
            Some(at) => self.head_oid != head_oid || now.duration_since(at) >= FILES_REFRESH_AFTER,
        }
    }
}

/// Files whose path contains `query` (case-insensitive); everything for an empty query.
pub(super) fn filter_files<'a>(files: &'a [PrFile], query: &str) -> Vec<&'a PrFile> {
    let query = query.trim().to_lowercase();
    files
        .iter()
        .filter(|file| query.is_empty() || file.path.to_lowercase().contains(&query))
        .collect()
}

/// The diff to open for `file`: the key and the original path (for a rename).
pub(super) fn diff_target(file: &PrFile, range: &(String, String)) -> (CompareKey, Option<String>) {
    let key = CompareKey {
        path: file.path.clone(),
        source: CompareSource::Range { base: range.0.clone(), head: range.1.clone() },
    };
    let original = match file.status {
        FileStatus::Renamed | FileStatus::Copied => file.previous_path.clone(),
        _ => None,
    };
    (key, original)
}

impl GitPanel {
    pub(super) fn pr_files_ensure(&mut self, pr: &PullRequest, cx: &mut Context<Self>) {
        let number = pr.number;
        let Some(gh) = self.pr_gh(number, cx) else { return };
        let key = gh.target.key.clone();
        let entry = self.pr_extras.files.entry(key.clone()).or_default();
        if !entry.needs_fetch(&pr.head_oid, Instant::now()) {
            return;
        }
        entry.loading = true;
        entry.head_oid = pr.head_oid.clone();
        let head_oid = pr.head_oid.clone();
        let github = gh.github();
        let (local, base_ref) = (gh.local.clone(), gh.base_ref.clone());
        let job = cx.background_spawn(async move {
            let files = github.pr_files(number).map_err(|e| e.to_string())?;
            let viewed = github.viewed_state(number).unwrap_or_default();
            let range = base_ref.and_then(|base| local.pr_range(&base, &head_oid).ok().flatten());
            Ok::<_, String>((files, viewed, range))
        });
        cx.spawn(async move |panel, cx| {
            let result = job.await;
            let _ = panel.update(cx, |panel, cx| {
                let entry = panel.pr_extras.files.entry(key).or_default();
                entry.loading = false;
                entry.fetched_at = Some(Instant::now());
                match result {
                    Ok((files, viewed, range)) => {
                        entry.files = Some(files);
                        entry.viewed = viewed;
                        entry.range = range;
                        entry.error = None;
                    }
                    Err(error) => entry.error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Optimistically flips the viewed flag; a failure puts it back.
    fn pr_toggle_viewed(&mut self, key: (PathBuf, u64), path: String, cx: &mut Context<Self>) {
        let Some(gh) = self.pr_gh(key.1, cx).filter(|gh| gh.target.key == key) else { return };
        let Some(entry) = self.pr_extras.files.get_mut(&key) else { return };
        if entry.viewed.pr_id.is_empty() {
            return;
        }
        let previous = entry.viewed.viewed.get(&path).copied().unwrap_or(false);
        let next = !previous;
        entry.viewed.viewed.insert(path.clone(), next);
        entry.viewed_error = None;
        let pr_id = entry.viewed.pr_id.clone();
        cx.notify();
        let github = gh.github();
        let sent = path.clone();
        let job = cx.background_spawn(async move {
            github.set_file_viewed(&pr_id, &sent, next).map_err(|e| e.to_string())
        });
        cx.spawn(async move |panel, cx| {
            let result = job.await;
            let _ = panel.update(cx, |panel, cx| {
                if let (Err(error), Some(entry)) = (result, panel.pr_extras.files.get_mut(&key)) {
                    entry.viewed.viewed.insert(path, previous);
                    entry.viewed_error = Some(error);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Section shown in the Pull request tab under the PR card; `None` hides it.
    pub(super) fn pr_files_section(
        &self,
        pr: &PullRequest,
        lang: Language,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let gh = self.pr_gh(pr.number, cx)?;
        let theme = cx.theme().clone();
        let key = gh.target.key.clone();
        let entry = self.pr_extras.files.get(&key);
        if entry.is_none_or(|entry| entry.needs_fetch(&pr.head_oid, Instant::now())) {
            let pr = pr.clone();
            cx.spawn(async move |panel, cx| {
                let _ = panel.update(cx, |panel, cx| panel.pr_files_ensure(&pr, cx));
            })
            .detach();
        }
        let files = entry.and_then(|entry| entry.files.as_deref());
        let collapsed = self.pr_extras.files_collapsed;
        let (additions, deletions) = files.map(files_totals).unwrap_or((0, 0));
        let viewed = entry.zip(files).map(|(entry, files)| viewed_count(files, &entry.viewed.viewed)).unwrap_or(0);
        let header = h_flex()
            .id("pr-files-head")
            .h(px(26.))
            .gap_1()
            .px_2()
            .items_center()
            .cursor_pointer()
            .bg(theme.list_head)
            .hover(|style| style.bg(theme.list_hover))
            .on_click(cx.listener(|panel, _, window, cx| {
                panel.pr_extras.files_collapsed = !panel.pr_extras.files_collapsed;
                if panel.pr_extras.files_filter.is_none() {
                    let lang = settings::language(cx);
                    let input = cx.new(|cx| InputState::new(window, cx).placeholder(tr(lang, Tr::FilesFilter)));
                    panel.pr_extras.files_filter_sub = Some(cx.subscribe(&input, |_, _, event, cx| {
                        if matches!(event, InputEvent::Change) {
                            cx.notify();
                        }
                    }));
                    panel.pr_extras.files_filter = Some(input);
                }
                cx.notify();
            }))
            .child(chevron(!collapsed))
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(px(12.))
                    .child(format!("{} {}", tr(lang, Tr::FilesTitle), files.map_or(0, <[PrFile]>::len))),
            )
            .child(div().flex_shrink_0().text_size(px(11.)).text_color(theme.green).child(format!("+{additions}")))
            .child(div().flex_shrink_0().text_size(px(11.)).text_color(theme.red).child(format!("\u{2212}{deletions}")))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_right()
                    .text_size(px(11.))
                    .text_color(theme.muted_foreground)
                    .child(
                        tr(lang, Tr::FilesViewed)
                            .replace("{k}", &viewed.to_string())
                            .replace("{n}", &files.map_or(0, <[PrFile]>::len).to_string()),
                    ),
            );
        let body = (!collapsed).then(|| self.pr_files_body(&key, entry, lang, cx));
        Some(v_flex().id("pr-files-section").min_w_0().child(header).children(body).into_any_element())
    }

    fn pr_files_body(
        &self,
        key: &(PathBuf, u64),
        entry: Option<&FilesEntry>,
        lang: Language,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let note = |text: String, error: bool| {
            div()
                .px_2()
                .py_1()
                .text_size(px(11.))
                .text_color(if error { theme.red } else { theme.muted_foreground })
                .child(text)
                .into_any_element()
        };
        let Some(entry) = entry else {
            return note(tr(lang, Tr::FilesLoading).to_owned(), false);
        };
        let Some(files) = entry.files.as_deref() else {
            return match &entry.error {
                Some(error) => note(error.lines().take(3).collect::<Vec<_>>().join("\n"), true),
                None => note(tr(lang, Tr::FilesLoading).to_owned(), false),
            };
        };
        if files.is_empty() {
            return note(tr(lang, Tr::FilesNone).to_owned(), false);
        }
        let query = self
            .pr_extras
            .files_filter
            .as_ref()
            .map(|input| input.read(cx).value().to_string())
            .unwrap_or_default();
        let shown = filter_files(files, &query);
        let hidden = shown.len().saturating_sub(MAX_ROWS);
        let can_mark = !entry.viewed.pr_id.is_empty();
        let rows = shown.iter().take(MAX_ROWS).enumerate().map(|(index, file)| {
            let is_viewed = entry.viewed.viewed.get(&file.path).copied().unwrap_or(false);
            let (name, dir) = split_file_label(&file.path);
            let (letter_color, _) = match file.status {
                FileStatus::Added | FileStatus::Copied => (theme.green, ()),
                FileStatus::Removed => (theme.red, ()),
                FileStatus::Renamed => (theme.yellow, ()),
                _ => (theme.muted_foreground, ()),
            };
            let target = entry.range.as_ref().map(|range| diff_target(file, range));
            let toggle_key = key.clone();
            let toggle_path = file.path.clone();
            h_flex()
                .h(px(24.))
                .px_2()
                .gap_1()
                .items_center()
                .min_w_0()
                .child(
                    div()
                        .w(px(12.))
                        .flex_shrink_0()
                        .text_size(px(11.))
                        .text_color(letter_color)
                        .child(file.status.letter().to_string()),
                )
                .child(
                    h_flex()
                        .id(SharedString::from(format!("pr-file-{index}")))
                        .flex_1()
                        .min_w_0()
                        .gap_1()
                        .overflow_hidden()
                        .items_center()
                        .when(target.is_some(), |node| node.cursor_pointer())
                        .when(is_viewed, |node| node.opacity(0.6))
                        .hover(|style| style.bg(theme.list_hover))
                        .on_click(cx.listener(move |panel, _, _, cx| {
                            if let Some((diff_key, original)) = target.clone() {
                                panel.open_diff(diff_key, original, cx);
                            }
                        }))
                        .child(div().flex_shrink_0().text_size(px(12.)).child(name.to_owned()))
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_size(px(11.))
                                .text_color(theme.muted_foreground)
                                .child(dir.to_owned()),
                        ),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(10.))
                        .text_color(theme.green)
                        .child(format!("+{}", file.additions)),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(10.))
                        .text_color(theme.red)
                        .child(format!("\u{2212}{}", file.deletions)),
                )
                .child(
                    Checkbox::new(SharedString::from(format!("pr-file-viewed-{index}")))
                        .checked(is_viewed)
                        .small()
                        .disabled(!can_mark)
                        .tooltip(tr(lang, Tr::FilesMarkViewed))
                        .on_change(cx.listener(move |panel, _: &bool, _, cx| {
                            panel.pr_toggle_viewed(toggle_key.clone(), toggle_path.clone(), cx);
                        })),
                )
        });
        v_flex()
            .child(
                div()
                    .px_2()
                    .py_1()
                    .when(files.len() <= FILTER_FROM, |node| node.hidden())
                    .children(self.pr_extras.files_filter.as_ref().map(|input| Input::new(input).small())),
            )
            .children(
                (entry.range.is_none())
                    .then(|| note(tr(lang, Tr::FilesFetchHint).to_owned(), false)),
            )
            .children(rows)
            .children((hidden > 0).then(|| note(format!("\u{2026} +{hidden}"), false)))
            .children(entry.viewed_error.as_ref().map(|error| {
                note(error.lines().take(3).collect::<Vec<_>>().join("\n"), true)
            }))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, status: FileStatus, previous: Option<&str>) -> PrFile {
        PrFile {
            path: path.into(),
            previous_path: previous.map(str::to_owned),
            status,
            additions: 1,
            deletions: 1,
        }
    }

    #[test]
    fn filter_is_case_insensitive_on_the_path() {
        let files = [
            file("src/Main.rs", FileStatus::Modified, None),
            file("docs/readme.md", FileStatus::Added, None),
        ];
        assert_eq!(filter_files(&files, "").len(), 2);
        assert_eq!(filter_files(&files, "  MAIN ").len(), 1);
        assert_eq!(filter_files(&files, "src/").len(), 1);
        assert!(filter_files(&files, "zzz").is_empty());
    }

    #[test]
    fn diff_target_uses_the_previous_path_for_renames() {
        let range = ("base".to_owned(), "head".to_owned());
        let (key, original) = diff_target(&file("new.rs", FileStatus::Renamed, Some("old.rs")), &range);
        assert_eq!(key.path, "new.rs");
        assert_eq!(key.source, CompareSource::Range { base: "base".into(), head: "head".into() });
        assert_eq!(original.as_deref(), Some("old.rs"));
        let (_, original) = diff_target(&file("a.rs", FileStatus::Modified, Some("ignored")), &range);
        assert_eq!(original, None);
    }

    #[test]
    fn fetch_policy() {
        let now = Instant::now();
        let mut entry = FilesEntry::default();
        assert!(entry.needs_fetch("h1", now));
        entry.fetched_at = Some(now);
        entry.head_oid = "h1".into();
        assert!(!entry.needs_fetch("h1", now + Duration::from_secs(5)));
        assert!(entry.needs_fetch("h2", now + Duration::from_secs(5)));
        assert!(entry.needs_fetch("h1", now + FILES_REFRESH_AFTER));
        entry.loading = true;
        assert!(!entry.needs_fetch("h2", now));
    }

    #[test]
    fn viewed_toggle_roll_back_shape() {
        let mut viewed: HashMap<String, bool> = HashMap::new();
        let previous = viewed.get("a").copied().unwrap_or(false);
        viewed.insert("a".into(), !previous);
        assert!(viewed["a"]);
        viewed.insert("a".into(), previous);
        assert!(!viewed["a"]);
    }
}
