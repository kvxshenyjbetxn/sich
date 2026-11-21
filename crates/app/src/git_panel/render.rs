use std::fmt::Display;

use chrono::{DateTime, Local, TimeZone};
use gpui_kit::assets::IconName as Ic;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Textarea;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, CursorStyle, ElementId, Hsla, InteractiveElement as _, IntoElement,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement as _, Render,
    SharedString, StatefulInteractiveElement as _, Styled as _, Window, div, px, relative,
};
use sich_git::STATUS_LIMIT;

use super::*;
use crate::diff_view::split_file_label;
use crate::i18n::relative_time;
use crate::settings;
use crate::sidebar::now_unix_ms;

const FILE_ACTION_W: f32 = 20.;
const FILE_COUNT_W: f32 = 42.;
const REF_LABEL_CHARS: usize = 16;

impl Render for GitPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let available = (f32::from(window.viewport_size().height) - 56.).max(28.);
        if let Some(height) = self.history_height {
            self.history_height = Some(clamp_history_height(height, available));
        }
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
            .relative()
            .overflow_hidden()
            .bg(theme.sidebar)
            .child(content)
            .children(self.history_drag.is_some().then(|| {
                div()
                    .id("git-history-drag-overlay")
                    .absolute()
                    .size_full()
                    .cursor(CursorStyle::ResizeUpDown)
                    .on_mouse_move(cx.listener(Self::on_history_drag_move))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::on_history_drag_end))
                    .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_history_drag_end))
            }))
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
            .child(self.branch_trigger(branch, lang, cx))
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
                let source = match section {
                    Section::Conflicts => CompareSource::Conflict,
                    Section::Staged => CompareSource::Staged,
                    Section::Changes => CompareSource::Working,
                };
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
                    .child(
                        tool_button("git-stage-path", icon, tr(lang, label))
                            .w(px(FILE_ACTION_W))
                            .disabled(busy.is_some())
                            .on_click(cx.listener(move |panel, _, window, cx| {
                                panel.mutate(mutation.clone(), window, cx);
                                cx.stop_propagation();
                            })),
                    )
                    .children((section == Section::Changes).then(|| {
                        let file = file.clone();
                        tool_button("git-discard-path", Ic::Trash, tr(lang, Tr::GitDiscard))
                            .w(px(FILE_ACTION_W))
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
                self.file_row(file, source, group, Some(actions), cx)
            }))
    }

    fn file_row(
        &self,
        file: &FileChange,
        source: CompareSource,
        group: SharedString,
        actions: Option<AnyElement>,
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
        let original = file.orig_path.clone().filter(|_| {
            matches!(source, CompareSource::Commit(_)) || file.status == ChangeKind::Renamed
        });
        let key = CompareKey {
            path: file.path.clone(),
            source,
        };
        let selected = self.active_diff.as_ref() == Some(&key);
        v_flex()
            .id(SharedString::from(format!("{group}-row")))
            .w_full()
            .child(
                h_flex()
                    .id(group.clone())
                    .group(group.clone())
                    .h(px(24.))
                    .min_w_0()
                    .gap_1()
                    .px_2()
                    .cursor_pointer()
                    .bg(if selected {
                        theme.list_active
                    } else {
                        theme.sidebar.opacity(0.)
                    })
                    .hover(|s| {
                        s.bg(if selected {
                            theme.list_active
                        } else {
                            theme.list_hover
                        })
                    })
                    .when(!selected, |row| {
                        row.tooltip(move |window, cx| {
                            Tooltip::new(tooltip.clone()).build(window, cx)
                        })
                    })
                    .on_click(cx.listener(move |panel, _, _, cx| {
                        panel.open_diff(key.clone(), original.clone(), cx);
                    }))
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
                    .child(file_count(file.added, '+', theme.green))
                    .child(file_count(file.removed, '−', theme.red))
                    .child(
                        div()
                            .w(px(2. * FILE_ACTION_W))
                            .flex_shrink_0()
                            .invisible()
                            .group_hover(group, |s| s.visible())
                            .children(actions),
                    ),
            )
    }

    fn on_history_handle_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        if event.click_count >= 2 {
            self.history_drag = None;
            self.history_height = None;
        } else {
            let available = (f32::from(window.viewport_size().height) - 56.).max(28.);
            self.history_drag = Some(HistoryDrag {
                start_y: event.position.y.into(),
                start_height: clamp_history_height(self.history_height.unwrap_or(f32::NAN), available),
            });
        }
        cx.notify();
    }

    fn on_history_drag_move(&mut self, event: &MouseMoveEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(drag) = self.history_drag else { return };
        let available = (f32::from(window.viewport_size().height) - 56.).max(28.);
        let y: f32 = event.position.y.into();
        self.history_height = Some(clamp_history_height(drag.start_height + drag.start_y - y, available));
        cx.notify();
    }

    fn on_history_drag_end(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.history_drag.take().is_some() {
            cx.notify();
        }
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
            .relative()
            .flex_shrink_0()
            .border_t_1()
            .border_color(theme.border)
            .children((!self.history_collapsed).then(|| {
                div()
                    .id("git-history-resize")
                    .group("git-history-resize")
                    .absolute()
                    .top(px(-3.))
                    .left_0()
                    .right_0()
                    .h(px(6.))
                    .cursor(CursorStyle::ResizeUpDown)
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::on_history_handle_down))
                    .child(
                        div()
                            .absolute()
                            .top(px(2.))
                            .left_0()
                            .right_0()
                            .h(px(2.))
                            .group_hover("git-history-resize", |s| s.bg(theme.primary.opacity(0.6))),
                    )
            }))
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
            } else if let Some(height) = self.history_height {
                px(height).into()
            } else {
                relative(DEFAULT_HISTORY_RATIO)
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
                    .children(badges.into_iter().enumerate().map(|(index, badge)| {
                        let (background, color) = if badge.current {
                            (theme.primary.opacity(0.20), theme.primary)
                        } else {
                            (theme.muted.opacity(0.75), theme.muted_foreground)
                        };
                        let tooltip: SharedString = badge.label.to_owned().into();
                        div()
                            .id(("git-ref-badge", index))
                            .min_w_0()
                            .max_w(px(180.))
                            .px_1()
                            .truncate()
                            .rounded_sm()
                            .bg(background)
                            .text_color(color)
                            .text_size(px(10.))
                            .tooltip(move |window, cx| {
                                Tooltip::new(tooltip.clone()).build(window, cx)
                            })
                            .child(badge_text(badge.label))
                    }))
                    .children((more > 0).then(|| {
                        div()
                            .flex_shrink_0()
                            .px_1()
                            .rounded_sm()
                            .bg(theme.muted.opacity(0.75))
                            .text_color(theme.muted_foreground)
                            .text_size(px(10.))
                            .child(format!("+{more}"))
                    }))
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
                let date = format_commit_date(commit.time_unix, &Local);
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
                                self.file_row(
                                    file,
                                    CompareSource::Commit(commit.oid.clone()),
                                    format!("git-commit-file-{}-{}", commit.oid, file.path).into(),
                                    None,
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

fn file_count(count: Option<u32>, sign: char, color: Hsla) -> impl IntoElement {
    div()
        .w(px(FILE_COUNT_W))
        .flex_shrink_0()
        .truncate()
        .text_right()
        .text_size(px(11.))
        .text_color(color)
        .children(count.map(|n| format!("{sign}{n}")))
}

#[derive(Debug, PartialEq, Eq)]
struct RefBadge<'a> {
    label: &'a str,
    current: bool,
}

fn parse_ref_badge(decoration: &str) -> RefBadge<'_> {
    if let Some(branch) = decoration.strip_prefix("HEAD -> ") {
        RefBadge {
            label: branch,
            current: true,
        }
    } else {
        RefBadge {
            label: decoration.strip_prefix("tag: ").unwrap_or(decoration),
            current: decoration == "HEAD",
        }
    }
}

fn ref_badges(refs: &[String]) -> (Vec<RefBadge<'_>>, usize) {
    let badges = refs
        .iter()
        .take(2)
        .map(|name| parse_ref_badge(name))
        .collect();
    (badges, refs.len().saturating_sub(2))
}

fn badge_text(label: &str) -> String {
    match label.char_indices().nth(REF_LABEL_CHARS) {
        Some((end, _)) => format!("{}…", &label[..end]),
        None => label.to_owned(),
    }
}

fn format_commit_date<Tz: TimeZone>(timestamp: i64, timezone: &Tz) -> String
where
    Tz::Offset: Display,
{
    DateTime::from_timestamp(timestamp, 0)
        .map(|date| {
            date.with_timezone(timezone)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_default()
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
    fn ref_badges_strip_prefixes_mark_current_and_limit_visible_refs() {
        let refs = ["HEAD -> main", "tag: v1", "topic", "origin/main"].map(str::to_owned);
        assert_eq!(
            ref_badges(&refs),
            (
                vec![
                    RefBadge {
                        label: "main",
                        current: true
                    },
                    RefBadge {
                        label: "v1",
                        current: false
                    }
                ],
                2
            )
        );
        assert_eq!(ref_badges(&[]), (Vec::new(), 0));
    }

    #[test]
    fn detached_head_and_remote_refs_keep_their_labels() {
        assert_eq!(
            parse_ref_badge("HEAD"),
            RefBadge {
                label: "HEAD",
                current: true
            }
        );
        assert_eq!(
            parse_ref_badge("origin/main"),
            RefBadge {
                label: "origin/main",
                current: false
            }
        );
        assert_eq!(
            parse_ref_badge("tag: HEAD"),
            RefBadge {
                label: "HEAD",
                current: false
            }
        );
    }

    #[test]
    fn badge_labels_fit_sixteen_characters_and_truncate_at_a_utf8_boundary() {
        assert_eq!(badge_text("origin/main"), "origin/main");
        assert_eq!(badge_text("1234567890abcdef"), "1234567890abcdef");
        assert_eq!(badge_text("1234567890abcdeї😀"), "1234567890abcdeї…");
    }

    #[test]
    fn commit_dates_apply_the_display_timezone_and_omit_its_suffix() {
        let timestamp = DateTime::parse_from_rfc3339("2026-11-19T20:50:27Z")
            .unwrap()
            .timestamp();
        let plus_two = chrono::FixedOffset::east_opt(2 * 3600).unwrap();
        let plus_five = chrono::FixedOffset::east_opt(5 * 3600).unwrap();
        assert_eq!(format_commit_date(timestamp, &plus_two), "2026-11-19 22:50");
        assert_eq!(
            format_commit_date(timestamp, &plus_five),
            "2026-11-20 01:50"
        );
        assert_eq!(format_commit_date(i64::MAX, &plus_two), "");
    }
}
