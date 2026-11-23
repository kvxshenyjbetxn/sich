//! Conflicts section: the unresolved files of a merge, rebase, cherry-pick or revert, with a
//! kind badge, per-file resolutions and "Resolve with agent".

use std::path::{Path, PathBuf};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, InteractiveElement as _, IntoElement as _, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, div, px,
};
use sich_git::compare::{CompareKey, CompareSource};
use sich_git::forge::write_report;
use sich_git::{ConflictKind, Resolution, conflict_resolutions};

use super::render::chevron;
use super::{GitPanel, GitPanelEvent, Mutation, Operation, Section};
use crate::diff_view::split_file_label;
use crate::i18n::{Language, Tr, tr};
use crate::sidebar::now_unix_ms;
use sich_git::{FileChange, InProgress, RepoStatus};

pub(super) fn conflicts_title(op: Option<InProgress>) -> Tr {
    match op {
        Some(InProgress::Merge) => Tr::CmpMergeConflicts,
        Some(InProgress::Rebase) => Tr::CmpRebaseConflicts,
        Some(InProgress::CherryPick) => Tr::CmpCherryPickConflicts,
        Some(InProgress::Revert) => Tr::CmpRevertConflicts,
        None => Tr::GitConflicts,
    }
}

pub(super) fn abort_label(op: InProgress) -> Tr {
    match op {
        InProgress::Merge => Tr::CmpAbortMerge,
        InProgress::Rebase => Tr::CmpAbortRebase,
        InProgress::CherryPick => Tr::CmpAbortCherryPick,
        InProgress::Revert => Tr::CmpAbortRevert,
    }
}

pub(super) fn kind_tr(kind: ConflictKind) -> Tr {
    match kind {
        ConflictKind::BothModified => Tr::CmpBothModified,
        ConflictKind::BothAdded => Tr::CmpBothAdded,
        ConflictKind::BothDeleted => Tr::CmpBothDeleted,
        ConflictKind::AddedByUs => Tr::CmpAddedByUs,
        ConflictKind::AddedByThem => Tr::CmpAddedByThem,
        ConflictKind::DeletedByUs => Tr::CmpDeletedByUs,
        ConflictKind::DeletedByThem => Tr::CmpDeletedByThem,
    }
}

fn resolution_tr(resolution: Resolution) -> Tr {
    match resolution {
        Resolution::MarkResolved => Tr::CmpMarkResolved,
        Resolution::KeepFile => Tr::CmpKeepFile,
        Resolution::KeepDeletion => Tr::CmpKeepDeletion,
    }
}

/// English names for the agent report.
fn kind_name(kind: Option<ConflictKind>) -> &'static str {
    match kind {
        Some(ConflictKind::BothModified) => "both modified",
        Some(ConflictKind::BothAdded) => "both added",
        Some(ConflictKind::BothDeleted) => "both deleted",
        Some(ConflictKind::AddedByUs) => "added by us",
        Some(ConflictKind::AddedByThem) => "added by them",
        Some(ConflictKind::DeletedByUs) => "deleted by us",
        Some(ConflictKind::DeletedByThem) => "deleted by them",
        None => "unmerged",
    }
}

fn op_name(op: Option<InProgress>) -> &'static str {
    match op {
        Some(InProgress::Merge) => "merge",
        Some(InProgress::Rebase) => "rebase",
        Some(InProgress::CherryPick) => "cherry-pick",
        Some(InProgress::Revert) => "revert",
        None => "operation",
    }
}

/// Control characters become spaces and runs of whitespace collapse: safe inside one line.
fn one_line(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The full report written to disk for the agent.
pub(super) fn conflicts_report(op: Option<InProgress>, files: &[FileChange]) -> String {
    let mut out = format!(
        "A git {} stopped with {} unresolved file(s).\n\nUnresolved files:\n",
        op_name(op),
        files.len()
    );
    for file in files {
        out.push_str(&format!("- {} ({})\n", one_line(&file.path), kind_name(file.conflict)));
    }
    out.push_str(
        "\nWhat to do:\n\
         1. Open each file and resolve the conflict markers (<<<<<<<, =======, >>>>>>>), keeping the intent of both sides.\n\
         2. For files deleted by one side, decide whether to keep the file or accept the deletion.\n\
         3. Run the tests or the build to check the result.\n\
         4. Stage the resolved files (`git add <path>`, or `git rm <path>` for accepted deletions).\n\
         5. Do not commit, continue the operation or push. Tell me what you changed.\n",
    );
    out
}

/// The single line pasted into the agent terminal; the report is in `report_path`.
pub(super) fn conflicts_prompt_line(op: Option<InProgress>, count: usize, report_path: &Path) -> String {
    format!(
        "Resolve the {} conflicts in {count} file(s). Full report: {}. Read it, resolve the conflict markers keeping both sides' intent, run the tests, stage the resolved files, and do not commit or push.",
        op_name(op),
        one_line(&report_path.display().to_string()),
    )
}

/// `<sich data dir>/sich/agent-prompts`, the same place the pull request reports go to.
fn agent_prompts_dir() -> Option<PathBuf> {
    sich_daemon::data_local_dir().map(|dir| dir.join("sich").join("agent-prompts"))
}

impl GitPanel {
    pub(super) fn conflicts_section(
        &self,
        status: &RepoStatus,
        busy: Option<Operation>,
        lang: Language,
        cx: &mut gpui_kit::Context<Self>,
    ) -> Option<AnyElement> {
        if status.conflicted.is_empty() {
            return None;
        }
        let theme = cx.theme().clone();
        let op = status.in_progress;
        let collapsed = self.collapsed.contains(&Section::Conflicts);
        let header = h_flex()
            .id("git-conflicts")
            .h(px(26.))
            .gap_1()
            .px_2()
            .items_center()
            .cursor_pointer()
            .bg(theme.list_head)
            .hover(|style| style.bg(theme.list_hover))
            .on_click(cx.listener(|panel, _, _, cx| {
                if !panel.collapsed.remove(&Section::Conflicts) {
                    panel.collapsed.insert(Section::Conflicts);
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
                    .child(tr(lang, conflicts_title(op))),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(px(11.))
                    .text_color(theme.warning)
                    .child(format!("{} {}", status.conflicted.len(), tr(lang, Tr::CmpUnresolved))),
            );
        let buttons = h_flex()
            .flex_shrink_0()
            .gap_1()
            .px_2()
            .py_1()
            .child(
                Button::new("cmp-resolve-agent")
                    .primary()
                    .xsmall()
                    .label(tr(lang, Tr::CmpResolveWithAgent))
                    .on_click(cx.listener(|panel, _, _, cx| panel.conflicts_to_agent(cx))),
            )
            .children(op.map(|op| {
                Button::new("cmp-abort")
                    .xsmall()
                    .label(tr(lang, abort_label(op)))
                    .disabled(busy.is_some())
                    .loading(busy == Some(Operation::Abort))
                    .on_click(cx.listener(move |panel, _, window, cx| {
                        panel.confirm(
                            abort_label(op),
                            tr(lang, Tr::GitAbortConfirm).to_owned(),
                            true,
                            lang,
                            move |panel, window, cx| panel.mutate(Mutation::Abort(op), window, cx),
                            window,
                            cx,
                        );
                    }))
            }));
        Some(
            v_flex()
                .id("git-conflicts-section")
                .child(header)
                .children((!collapsed).then(|| buttons))
                .children(
                    status
                        .conflicted
                        .iter()
                        .filter(|_| !collapsed)
                        .map(|file| self.conflict_row(file, busy, lang, cx)),
                )
                .into_any_element(),
        )
    }

    fn conflict_row(
        &self,
        file: &FileChange,
        busy: Option<Operation>,
        lang: Language,
        cx: &mut gpui_kit::Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let (name, directory) = split_file_label(&file.path);
        let key = CompareKey { path: file.path.clone(), source: CompareSource::Conflict };
        let selected = self.active_diff.as_ref() == Some(&key);
        let group = SharedString::from(format!("git-conflicts-{}", file.path));
        let actions = conflict_resolutions(file.conflict.unwrap_or(ConflictKind::BothModified));
        v_flex()
            .id(SharedString::from(format!("{group}-row")))
            .w_full()
            .child(
                h_flex()
                    .id(group.clone())
                    .h(px(24.))
                    .min_w_0()
                    .gap_1()
                    .px_2()
                    .cursor_pointer()
                    .bg(if selected { theme.list_active } else { theme.sidebar.opacity(0.) })
                    .hover(|s| s.bg(if selected { theme.list_active } else { theme.list_hover }))
                    .on_click(cx.listener(move |panel, _, _, cx| panel.open_diff(key.clone(), None, cx)))
                    .child(
                        div().w(px(14.)).flex_shrink_0().text_size(px(11.)).text_color(theme.red).child("U"),
                    )
                    .child(
                        h_flex()
                            .flex_1()
                            .gap_1()
                            .min_w_0()
                            .child(div().min_w_0().flex_shrink_0().truncate().text_size(px(12.)).child(name.to_owned()))
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(11.))
                                    .text_color(theme.muted_foreground)
                                    .child(directory.to_owned()),
                            ),
                    )
                    .children(file.conflict.map(|kind| {
                        div()
                            .flex_shrink_0()
                            .px_1()
                            .rounded_sm()
                            .bg(theme.red.opacity(0.15))
                            .text_size(px(10.))
                            .text_color(theme.red)
                            .child(tr(lang, kind_tr(kind)))
                    })),
            )
            .child(
                h_flex()
                    .gap_1()
                    .px_2()
                    .pb_1()
                    .children(actions.iter().map(|&resolution| {
                        let path = file.path.clone();
                        Button::new(SharedString::from(format!("{group}-{resolution:?}")))
                            .ghost()
                            .xsmall()
                            .label(tr(lang, resolution_tr(resolution)))
                            .disabled(busy.is_some())
                            .loading(busy == Some(Operation::Resolve))
                            .on_click(cx.listener(move |panel, _, window, cx| {
                                panel.mutate(
                                    Mutation::ResolveConflict { path: path.clone(), resolution },
                                    window,
                                    cx,
                                );
                            }))
                    })),
            )
            .into_any_element()
    }

    /// Writes the report and pastes the one-line prompt into the worktree's agent terminal.
    fn conflicts_to_agent(&mut self, cx: &mut gpui_kit::Context<Self>) {
        let (Some(worktree), Some(status)) = (self.worktree.clone(), self.status.as_ref()) else {
            return;
        };
        let Some(dir) = agent_prompts_dir() else {
            self.error = Some("No local data directory for the report".to_owned());
            cx.notify();
            return;
        };
        let report = conflicts_report(status.in_progress, &status.conflicted);
        let (op, count) = (status.in_progress, status.conflicted.len());
        // Number 0: the report is not tied to a pull request (it shares the PR reports' folder).
        match write_report(&dir, 0, u128::from(now_unix_ms()), &report) {
            Ok(path) => cx.emit(GitPanelEvent::SendToAgent {
                worktree,
                text: conflicts_prompt_line(op, count, &path),
            }),
            Err(error) => {
                self.error = Some(format!("Could not write the report to {}: {error}", dir.display()));
                cx.notify();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Language;
    use sich_git::ChangeKind;

    fn file(path: &str, conflict: Option<ConflictKind>) -> FileChange {
        FileChange {
            path: path.into(),
            orig_path: None,
            status: ChangeKind::Conflicted,
            added: None,
            removed: None,
            conflict,
        }
    }

    #[test]
    fn titles_and_abort_labels_follow_the_operation() {
        assert_eq!(conflicts_title(Some(InProgress::Merge)), Tr::CmpMergeConflicts);
        assert_eq!(conflicts_title(Some(InProgress::Rebase)), Tr::CmpRebaseConflicts);
        assert_eq!(conflicts_title(Some(InProgress::CherryPick)), Tr::CmpCherryPickConflicts);
        assert_eq!(conflicts_title(Some(InProgress::Revert)), Tr::CmpRevertConflicts);
        assert_eq!(conflicts_title(None), Tr::GitConflicts);
        assert_eq!(abort_label(InProgress::Merge), Tr::CmpAbortMerge);
        assert_eq!(abort_label(InProgress::Revert), Tr::CmpAbortRevert);
        assert_eq!(tr(Language::En, kind_tr(ConflictKind::DeletedByUs)), "deleted by us");
    }

    #[test]
    fn report_lists_files_kinds_and_instructions() {
        let files = [
            file("src/a.rs", Some(ConflictKind::BothModified)),
            file("b\nc.rs", Some(ConflictKind::DeletedByThem)),
            file("d.rs", None),
        ];
        let report = conflicts_report(Some(InProgress::Rebase), &files);
        assert!(report.contains("git rebase stopped with 3 unresolved"), "{report}");
        assert!(report.contains("- src/a.rs (both modified)"), "{report}");
        assert!(report.contains("- b c.rs (deleted by them)"), "{report}");
        assert!(report.contains("- d.rs (unmerged)"), "{report}");
        assert!(report.contains("conflict markers"));
        assert!(report.contains("Run the tests"));
        assert!(report.contains("Do not commit"));
    }

    #[test]
    fn prompt_line_is_a_single_line_with_the_report_path() {
        let line = conflicts_prompt_line(Some(InProgress::Merge), 2, Path::new("C:/data/a\nb.md"));
        assert!(!line.contains('\n'), "{line}");
        assert!(line.contains("merge conflicts in 2 file(s)"), "{line}");
        assert!(line.contains("C:/data/a b.md"), "{line}");
        assert!(line.contains("do not commit or push"));
    }
}
