use gpui_kit::assets::IconName as Ic;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Textarea;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, h_flex, v_flex,
};
use gpui_kit::{
    AnyElement, Context, ElementId, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, SharedString, StatefulInteractiveElement as _, Styled as _, Window, div, px, relative,
};
use sich_git::{DiffLineKind, STATUS_LIMIT};

use super::*;
use crate::i18n::{git_more_lines, relative_time};
use crate::settings;
use crate::sidebar::now_unix_ms;

const DIFF_ROWS: usize = 400;

impl Render for GitPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let lang = settings::language(cx);
        let input = if self.status.is_some() {
            self.message_input(lang, window, cx)
        } else {
            None
        };
        let content = if let (Some(status), Some(input)) = (&self.status, input) {
            self.git_tab(status, input, lang, cx).into_any_element()
        } else {
            let message = match (&self.worktree, &self.unavailable) {
                (None, _) => Tr::GitSelectProject,
                (_, Some(GitError::GitMissing)) => Tr::GitMissing,
                (_, Some(GitError::NotRepository)) => Tr::GitNotRepo,
                _ => Tr::GitLoading,
            };
            v_flex()
                .flex_1()
                .min_h_0()
                .child(self.error_notice(lang, cx))
                .child(
                    v_flex()
                        .flex_1()
                        .min_h_0()
                        .justify_center()
                        .items_center()
                        .px_3()
                        .text_center()
                        .text_color(theme.muted_foreground)
                        .text_size(px(12.))
                        .children(self.error.is_none().then(|| tr(lang, message))),
                )
                .into_any_element()
        };
        v_flex()
            .size_full()
            .min_h_0()
            .overflow_hidden()
            .bg(theme.sidebar)
            .child(content)
    }
}

impl GitPanel {
    fn git_tab(
        &self,
        status: &RepoStatus,
        input: Entity<TextareaState>,
        lang: Language,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let busy = self.active_operation();
        let button = commit_action(status, self.draft(), busy.is_some());
        let commit_label = if status.staged.is_empty() && !status.unstaged.is_empty() {
            Tr::GitCommitAll
        } else {
            Tr::GitCommit
        };
        v_flex()
            .flex_1()
            .min_h_0()
            .child(self.branch_header(status, busy, lang, cx))
            .child(self.error_notice(lang, cx))
            .child(
                v_flex()
                    .flex_shrink_0()
                    .gap_1()
                    .p_2()
                    .child(
                        Textarea::new(&input)
                            .small()
                            .aria_label(tr(lang, Tr::GitCommitMessage)),
                    )
                    .child(
                        Button::new("git-commit")
                            .small()
                            .primary()
                            .label(tr(lang, commit_label))
                            .disabled(button == CommitAction::Disabled)
                            .loading(busy == Some(Operation::Commit))
                            .on_click(
                                cx.listener(|panel, _, window, cx| {
                                    panel.commit_message(window, cx)
                                }),
                            ),
                    ),
            )
            .child(
                div()
                    .id("git-changes-scroll")
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .overflow_y_scroll()
                    .children(status.truncated.then(|| {
                        let notice = format!("{} {STATUS_LIMIT}", tr(lang, Tr::GitTruncated));
                        div()
                            .px_2()
                            .py_1()
                            .text_size(px(11.))
                            .text_color(theme.warning)
                            .child(notice)
                    }))
                    .children((!status.conflicted.is_empty()).then(|| {
                        self.change_section(Section::Conflicts, &status.conflicted, busy, lang, cx)
                    }))
                    .child(self.change_section(Section::Staged, &status.staged, busy, lang, cx))
                    .child(self.change_section(Section::Changes, &status.unstaged, busy, lang, cx))
                    .children(
                        (status.staged.is_empty()
                            && status.unstaged.is_empty()
                            && status.conflicted.is_empty())
                        .then(|| {
                            div()
                                .px_2()
                                .py_2()
                                .text_size(px(12.))
                                .text_color(theme.muted_foreground)
                                .child(tr(lang, Tr::GitNoChanges))
                        }),
                    ),
            )
            .child(self.history_section(lang, cx))
    }

    fn branch_header(
        &self,
        status: &RepoStatus,
        busy: Option<Operation>,
        lang: Language,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let branch = status.branch.clone().unwrap_or_else(|| {
            let short = status
                .head_oid
                .as_deref()
                .map_or("", |oid| oid.get(..7).unwrap_or(oid));
            format!("{} @ {short}", tr(lang, Tr::GitDetached))
        });
        let buttons = [
            ("git-fetch", Ic::Download, Tr::GitFetch, Mutation::Fetch),
            ("git-pull", Ic::ArrowDown, Tr::GitPull, Mutation::Pull),
            ("git-push", Ic::ArrowUp, Tr::GitPush, Mutation::Push),
        ];
        h_flex()
            .h(px(34.))
            .flex_shrink_0()
            .min_w_0()
            .px_2()
            .gap_1()
            .child(Icon::new(Ic::GitBranch).size(px(14.)).flex_shrink_0())
            .child(
                div()
                    .id("git-branch")
                    .min_w_0()
                    .flex_1()
                    .truncate()
                    .text_size(px(12.5))
                    .tooltip({
                        let tooltip = branch.clone();
                        move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx)
                    })
                    .child(branch),
            )
            .children(
                [(status.ahead, '↑'), (status.behind, '↓')]
                    .into_iter()
                    .filter_map(|(count, arrow)| {
                        (count > 0).then(|| {
                            div()
                                .flex_shrink_0()
                                .text_size(px(11.))
                                .text_color(theme.muted_foreground)
                                .child(format!("{arrow}{count}"))
                        })
                    }),
            )
            .children(buttons.into_iter().map(|(id, icon, label, mutation)| {
                tool_button(id, icon, tr(lang, label))
                    .loading(busy == Some(mutation.operation()))
                    .disabled(busy.is_some())
                    .on_click(cx.listener(move |panel, _, window, cx| {
                        panel.mutate(mutation.clone(), window, cx)
                    }))
            }))
            .child(
                tool_button("git-refresh", Ic::RefreshCw, tr(lang, Tr::Refresh))
                    .loading(self.refreshing)
                    .on_click(cx.listener(|panel, _, _, cx| panel.refresh(cx))),
            )
    }

    fn error_notice(&self, lang: Language, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        self.error
            .as_ref()
            .map(|error| {
                h_flex()
                    .items_start()
                    .flex_shrink_0()
                    .gap_1()
                    .mx_2()
                    .mb_1()
                    .p_1()
                    .rounded_sm()
                    .bg(theme.red.opacity(0.14))
                    .text_color(theme.red)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(11.))
                            .line_clamp(3)
                            .child(error.lines().take(3).collect::<Vec<_>>().join("\n")),
                    )
                    .child(
                        Button::new("git-dismiss-error")
                            .ghost()
                            .xsmall()
                            .icon(Icon::new(IconName::Close))
                            .tooltip(tr(lang, Tr::GitDismiss))
                            .on_click(cx.listener(|panel, _, _, cx| {
                                panel.error = None;
                                cx.notify();
                            })),
                    )
            })
            .into_iter()
            .fold(div(), |node, notice| node.child(notice))
    }

    fn change_section(
        &self,
        section: Section,
        files: &[FileChange],
        busy: Option<Operation>,
        lang: Language,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let (id, label) = match section {
            Section::Conflicts => ("git-conflicts", Tr::GitConflicts),
            Section::Staged => ("git-staged", Tr::GitStaged),
            Section::Changes => ("git-working", Tr::GitChanges),
        };
        let theme = cx.theme().clone();
        let collapsed = self.collapsed.contains(&section);
        let actions = if files.is_empty() || section == Section::Conflicts {
            None
        } else {
            let (icon, label, mutation) = if section == Section::Staged {
                (Ic::Minus, Tr::GitUnstageAll, Mutation::UnstageAll)
            } else {
                (Ic::Plus, Tr::GitStageAll, Mutation::StageAll)
            };
            Some(
                h_flex()
                    .invisible()
                    .group_hover(id, |style| style.visible())
                    .child(
                        tool_button("git-stage-section", icon, tr(lang, label))
                            .disabled(busy.is_some())
                            .on_click(cx.listener(move |panel, _, window, cx| {
                                panel.mutate(mutation.clone(), window, cx);
                                cx.stop_propagation();
                            })),
                    )
                    .children((section == Section::Changes).then(|| {
                        tool_button(
                            "git-discard-section",
                            Ic::Trash,
                            tr(lang, Tr::GitDiscardAll),
                        )
                        .disabled(busy.is_some())
                        .on_click(cx.listener(
                            move |panel, _, window, cx| {
                                panel.confirm_discard(Mutation::DiscardAll, lang, window, cx);
                                cx.stop_propagation();
                            },
                        ))
                    })),
            )
        };
        v_flex()
            .id(SharedString::from(format!("{id}-section")))
            .child(
                h_flex()
                    .id(id)
                    .group(id)
                    .h(px(26.))
                    .gap_1()
                    .px_2()
                    .items_center()
                    .cursor_pointer()
                    .bg(theme.list_head)
                    .hover(|style| style.bg(theme.list_hover))
                    .on_click(cx.listener(move |panel, _, _, cx| {
                        if !panel.collapsed.remove(&section) {
                            panel.collapsed.insert(section);
                        }
                        cx.notify();
                    }))
                    .child(chevron(!collapsed))
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .text_size(px(12.))
                            .child(format!("{} ({})", tr(lang, label), files.len())),
                    )
                    .children(actions),
            )
            .children(files.iter().filter(|_| !collapsed).map(|file| {
                let key = DiffKey::worktree(section, file);
                let group = SharedString::from(format!("{id}-{}", file.path));
                let (label, icon, mutation) = if section == Section::Staged {
                    (
                        Tr::GitUnstage,
                        Ic::Minus,
                        Mutation::Unstage(action_paths(file)),
                    )
                } else {
                    (Tr::GitStage, Ic::Plus, Mutation::Stage(action_paths(file)))
                };
                let actions = h_flex()
                    .flex_shrink_0()
                    .invisible()
                    .group_hover(group.clone(), |s| s.visible())
                    .child(
                        tool_button("git-stage-path", icon, tr(lang, label))
                            .disabled(busy.is_some())
                            .on_click(cx.listener(move |panel, _, window, cx| {
                                panel.mutate(mutation.clone(), window, cx);
                                cx.stop_propagation();
                            })),
                    )
                    .children((section == Section::Changes).then(|| {
                        let file = file.clone();
                        tool_button("git-discard-path", Ic::Trash, tr(lang, Tr::GitDiscard))
                            .disabled(busy.is_some())
                            .on_click(cx.listener(move |panel, _, window, cx| {
                                panel.confirm_discard(
                                    Mutation::Discard(file.clone()),
                                    lang,
                                    window,
                                    cx,
                                );
                                cx.stop_propagation();
                            }))
                    }))
                    .into_any_element();
                self.file_row(file, key, group, Some(actions), lang, cx)
            }))
    }

    fn file_row(
        &self,
        file: &FileChange,
        key: DiffKey,
        group: SharedString,
        actions: Option<AnyElement>,
        lang: Language,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let (letter, color) = match file.status {
            ChangeKind::Modified => ("M", theme.yellow),
            ChangeKind::Added => ("A", theme.green),
            ChangeKind::Deleted => ("D", theme.red),
            ChangeKind::Renamed => ("R", theme.blue),
            ChangeKind::Copied => ("C", theme.blue),
            ChangeKind::TypeChanged => ("T", theme.yellow),
            ChangeKind::Untracked => ("U", theme.green),
            ChangeKind::Conflicted => ("U", theme.red),
        };
        let (name, directory) = split_file_label(&file.path);
        let tooltip: SharedString = file
            .orig_path
            .as_ref()
            .map_or_else(
                || file.path.clone(),
                |path| format!("{path} → {}", file.path),
            )
            .into();
        let diff = self
            .diffs
            .get(&key)
            .map(|diff| self.render_diff(diff, lang, cx));
        v_flex()
            .id(SharedString::from(format!("{group}-row")))
            .w_full()
            .child(
                h_flex()
                    .id(group.clone())
                    .group(group)
                    .h(px(24.))
                    .min_w_0()
                    .gap_1()
                    .px_2()
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.list_hover))
                    .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                    .on_click(
                        cx.listener(move |panel, _, _, cx| panel.toggle_diff(key.clone(), cx)),
                    )
                    .child(
                        div()
                            .w(px(14.))
                            .flex_shrink_0()
                            .text_size(px(11.))
                            .text_color(color)
                            .child(letter),
                    )
                    .child(
                        h_flex()
                            .flex_1()
                            .gap_1()
                            .min_w_0()
                            .child(
                                div()
                                    .min_w_0()
                                    .flex_shrink_0()
                                    .max_w(relative(1.))
                                    .truncate()
                                    .text_size(px(12.))
                                    .child(name.to_owned()),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(11.))
                                    .text_color(theme.muted_foreground)
                                    .child(directory.to_owned()),
                            ),
                    )
                    .children(file.added.map(|n| {
                        div()
                            .flex_shrink_0()
                            .text_size(px(11.))
                            .text_color(theme.green)
                            .child(format!("+{n}"))
                    }))
                    .children(file.removed.map(|n| {
                        div()
                            .flex_shrink_0()
                            .text_size(px(11.))
                            .text_color(theme.red)
                            .child(format!("−{n}"))
                    }))
                    .children(actions),
            )
            .children(diff)
    }

    fn render_diff(
        &self,
        diff: &Expansion<Diff>,
        lang: Language,
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let content = match &diff.result {
            None => return tiny(tr(lang, Tr::GitLoading), cx),
            Some(Err(error)) => return tiny(error.to_string(), cx),
            Some(Ok(diff)) if diff.lines.is_empty() => return tiny(tr(lang, Tr::GitNoDiff), cx),
            Some(Ok(diff)) => diff,
        };
        v_flex()
            .w_full()
            .min_w_0()
            .border_y_1()
            .border_color(theme.border)
            .bg(theme.background)
            .child(
                div()
                    .id("git-inline-diff")
                    .min_w_0()
                    .overflow_x_scroll()
                    .child(
                        v_flex()
                            .min_w_full()
                            .w_auto()
                            .font_family("Cascadia Mono")
                            .text_size(px(11.5))
                            .children(content.lines.iter().take(DIFF_ROWS).map(|line| {
                                let (foreground, background) = match line.kind {
                                    DiffLineKind::Added => (theme.green, theme.green.opacity(0.12)),
                                    DiffLineKind::Removed => (theme.red, theme.red.opacity(0.12)),
                                    DiffLineKind::Hunk | DiffLineKind::Meta => {
                                        (theme.muted_foreground, theme.background)
                                    }
                                    DiffLineKind::Context => (theme.foreground, theme.background),
                                };
                                div()
                                    .px_2()
                                    .whitespace_nowrap()
                                    .bg(background)
                                    .text_color(foreground)
                                    .child(line.text.clone())
                            })),
                    ),
            )
            .children((content.lines.len() > DIFF_ROWS).then(|| {
                tiny(
                    git_more_lines(lang, content.lines.len() - DIFF_ROWS, content.truncated),
                    cx,
                )
            }))
            .children(
                (content.truncated && content.lines.len() <= DIFF_ROWS)
                    .then(|| tiny(tr(lang, Tr::GitDiffTruncated), cx)),
            )
            .into_any_element()
    }

    fn history_section(&self, lang: Language, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let header = h_flex()
            .id("git-history-header")
            .h(px(28.))
            .flex_shrink_0()
            .px_2()
            .gap_1()
            .items_center()
            .cursor_pointer()
            .bg(theme.list_head)
            .hover(|s| s.bg(theme.list_hover))
            .on_click(cx.listener(|panel, _, _, cx| {
                panel.history_collapsed = !panel.history_collapsed;
                cx.notify();
            }))
            .child(chevron(!self.history_collapsed))
            .child(div().text_size(px(12.)).child(format!(
                "{} ({}{})",
                tr(lang, Tr::GitCommits),
                self.history.commits.len(),
                if self.history.more { "+" } else { "" },
            )));
        v_flex()
            .w_full()
            .flex_shrink_0()
            .border_t_1()
            .border_color(theme.border)
            .child(header)
            .children((!self.history_collapsed).then(|| {
                div()
                    .id("git-history-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .children(self.history.commits.is_empty().then(|| {
                        let label = if self.refreshing_history {
                            Tr::GitLoading
                        } else {
                            Tr::GitNoCommits
                        };
                        tiny(tr(lang, label), cx)
                    }))
                    .children(
                        self.history
                            .commits
                            .iter()
                            .map(|commit| self.commit_row(commit, lang, cx)),
                    )
                    .children(self.history.more.then(|| {
                        div().p_1().child(
                            Button::new("git-load-more")
                                .ghost()
                                .small()
                                .label(tr(lang, Tr::GitLoadMore))
                                .loading(self.history.loading_more)
                                .disabled(self.refreshing_history)
                                .on_click(cx.listener(|panel, _, _, cx| panel.load_more(cx))),
                        )
                    }))
            }))
            .h(if self.history_collapsed {
                px(28.).into()
            } else {
                relative(0.4)
            })
    }

    fn commit_row(
        &self,
        commit: &Commit,
        lang: Language,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let oid = SharedString::from(commit.oid.clone());
        let expansion = self.history.expanded.get(&commit.oid);
        let (badges, more) = ref_badges(&commit.refs);
        v_flex()
            .id(oid.clone())
            .w_full()
            .child(
                h_flex()
                    .id("git-commit-heading")
                    .h(px(26.))
                    .min_w_0()
                    .px_2()
                    .gap_1()
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.list_hover))
                    .on_click(
                        cx.listener(move |panel, _, _, cx| {
                            panel.toggle_commit(oid.to_string(), cx)
                        }),
                    )
                    .child(chevron(expansion.is_some()))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.))
                            .child(commit.subject.clone()),
                    )
                    .children(
                        badges
                            .iter()
                            .cloned()
                            .chain((more > 0).then(|| format!("+{more}")))
                            .map(|badge| {
                                div()
                                    .max_w(px(52.))
                                    .min_w_0()
                                    .px_1()
                                    .truncate()
                                    .rounded_sm()
                                    .bg(theme.muted.opacity(0.75))
                                    .text_color(theme.muted_foreground)
                                    .text_size(px(10.))
                                    .child(badge)
                            }),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_color(theme.muted_foreground)
                            .text_size(px(11.))
                            .font_family("Cascadia Mono")
                            .child(commit.short.clone()),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_color(theme.muted_foreground)
                            .text_size(px(11.))
                            .child(relative_time(
                                lang,
                                now_unix_ms(),
                                (commit.time_unix.max(0) as u64).saturating_mul(1000),
                            )),
                    ),
            )
            .children(expansion.map(|files| {
                let date = chrono::DateTime::from_timestamp(commit.time_unix, 0)
                    .map(|date| date.format("%Y-%m-%d %H:%M UTC").to_string())
                    .unwrap_or_default();
                let metadata = format!("{} <{}> · {date}", commit.author, commit.email);
                v_flex()
                    .w_full()
                    .pl_3()
                    .pb_1()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .px_2()
                            .text_size(px(11.))
                            .text_color(theme.muted_foreground)
                            .child(metadata),
                    )
                    .children((!commit.body.is_empty()).then(|| {
                        div()
                            .px_2()
                            .py_1()
                            .text_size(px(11.))
                            .text_color(theme.muted_foreground)
                            .line_clamp(10)
                            .child(commit.body.lines().take(10).collect::<Vec<_>>().join("\n"))
                    }))
                    .child(match &files.result {
                        None => tiny(tr(lang, Tr::GitLoading), cx),
                        Some(Err(error)) => tiny(error.to_string(), cx),
                        Some(Ok(files)) => v_flex()
                            .w_full()
                            .children(files.iter().map(|file| {
                                let key =
                                    DiffKey::new(DiffSource::Commit(commit.oid.clone()), file);
                                self.file_row(
                                    file,
                                    key,
                                    format!("git-commit-file-{}-{}", commit.oid, file.path).into(),
                                    None,
                                    lang,
                                    cx,
                                )
                            }))
                            .into_any_element(),
                    })
            }))
    }
}

fn tool_button(id: impl Into<ElementId>, icon: Ic, label: &'static str) -> Button {
    Button::new(id)
        .ghost()
        .xsmall()
        .icon(Icon::new(icon))
        .tooltip(label)
}

fn chevron(expanded: bool) -> Icon {
    Icon::new(if expanded {
        Ic::ChevronDown
    } else {
        Ic::ChevronRight
    })
    .size(px(12.))
    .flex_shrink_0()
}

fn tiny(value: impl Into<SharedString>, cx: &Context<GitPanel>) -> AnyElement {
    div()
        .px_2()
        .py_1()
        .text_size(px(11.))
        .text_color(cx.theme().muted_foreground)
        .child(value.into())
        .into_any_element()
}

fn split_file_label(path: &str) -> (&str, &str) {
    path.rsplit_once('/')
        .map_or((path, ""), |(directory, name)| (name, directory))
}

fn ref_badges(refs: &[String]) -> (Vec<String>, usize) {
    let badges = refs
        .iter()
        .take(2)
        .map(|name| {
            name.strip_prefix("HEAD -> ")
                .map_or_else(|| name.clone(), |branch| format!("HEAD → {branch}"))
        })
        .collect();
    (badges, refs.len().saturating_sub(2))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_git_paths_at_the_last_separator() {
        assert_eq!(
            split_file_label("src/components/a file.rs"),
            ("a file.rs", "src/components")
        );
        assert_eq!(split_file_label("literal\\name"), ("literal\\name", ""));
        assert_eq!(split_file_label("main.rs"), ("main.rs", ""));
    }

    #[test]
    fn ref_badges_limit_visible_labels_and_preserve_head() {
        let refs = ["HEAD -> main", "tag: v1", "topic", "origin/main"].map(str::to_owned);
        assert_eq!(
            ref_badges(&refs),
            (vec!["HEAD → main".into(), "tag: v1".into()], 2)
        );
        assert_eq!(ref_badges(&[]), (Vec::new(), 0));
    }
}
