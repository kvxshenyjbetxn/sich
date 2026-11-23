//! Git operations beyond the basic buttons: the in-progress banner, the commit split menu
//! (commit & push, amend, undo) and the header overflow menu (pull --rebase, force push, stash).

use gpui_kit::assets::IconName as Ic;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, FontWeight, IntoElement, ParentElement as _, Styled as _,
    div, px,
};

use super::render::tool_button;
use super::*;
use crate::settings;

pub(super) fn banner_title(op: InProgress) -> Tr {
    match op {
        InProgress::Rebase => Tr::GitRebaseInProgress,
        InProgress::Merge => Tr::GitMergeInProgress,
        InProgress::CherryPick => Tr::GitCherryPickInProgress,
        InProgress::Revert => Tr::GitRevertInProgress,
    }
}

/// Continuing needs every conflict resolved (staged).
pub(super) fn continue_enabled(status: &RepoStatus, busy: bool) -> bool {
    !busy && status.conflicted.is_empty()
}

/// HEAD exists on the remote already: rewriting it needs a force push.
pub(super) fn is_pushed(status: &RepoStatus) -> bool {
    status.head_oid.is_some() && status.upstream.is_some() && status.ahead == 0
}

pub(super) fn amend_enabled(status: &RepoStatus, busy: bool) -> bool {
    !busy && status.head_oid.is_some() && status.conflicted.is_empty()
}

pub(super) fn undo_enabled(status: &RepoStatus, busy: bool) -> bool {
    !busy && status.head_oid.is_some() && status.in_progress.is_none()
}

pub(super) fn stash_enabled(status: &RepoStatus, busy: bool) -> bool {
    !busy
        && status.conflicted.is_empty()
        && !(status.staged.is_empty() && status.unstaged.is_empty())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AmendPlan {
    /// Empty draft: put the last commit message in the editor first.
    LoadMessage,
    /// The commit is already pushed: ask before rewriting it.
    Confirm,
    Run,
}

pub(super) fn amend_plan(status: &RepoStatus, draft: &str) -> AmendPlan {
    if draft.trim().is_empty() {
        AmendPlan::LoadMessage
    } else if is_pushed(status) {
        AmendPlan::Confirm
    } else {
        AmendPlan::Run
    }
}

impl GitPanel {
    pub(super) fn in_progress_banner(
        &self,
        status: &RepoStatus,
        busy: Option<Operation>,
        lang: Language,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let op = status.in_progress?;
        let theme = cx.theme().clone();
        Some(
            v_flex()
                .flex_shrink_0()
                .gap_1()
                .mx_2()
                .mb_1()
                .p_2()
                .rounded_md()
                .border_1()
                .border_color(theme.warning.opacity(0.4))
                .bg(theme.warning.opacity(0.12))
                .child(
                    div()
                        .text_size(px(12.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.warning)
                        .child(tr(lang, banner_title(op))),
                )
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme.warning)
                        .child(tr(lang, Tr::GitResolveHint)),
                )
                .child(
                    h_flex()
                        .gap_1()
                        .child(
                            Button::new("git-op-continue")
                                .primary()
                                .xsmall()
                                .label(tr(
                                    lang,
                                    if op == InProgress::Merge { Tr::CmpCompleteMerge } else { Tr::GitContinue },
                                ))
                                .disabled(!continue_enabled(status, busy.is_some()))
                                .loading(busy == Some(Operation::Continue))
                                .on_click(cx.listener(move |panel, _, window, cx| {
                                    let mutation = if op == InProgress::Merge {
                                        Mutation::CompleteMerge
                                    } else {
                                        Mutation::Continue(op)
                                    };
                                    panel.mutate(mutation, window, cx)
                                })),
                        )
                        .child(
                            Button::new("git-op-abort")
                                .xsmall()
                                .label(tr(lang, Tr::GitAbort))
                                .disabled(busy.is_some())
                                .loading(busy == Some(Operation::Abort))
                                .on_click(cx.listener(move |panel, _, window, cx| {
                                    panel.confirm(
                                        Tr::GitAbort,
                                        tr(lang, Tr::GitAbortConfirm).to_owned(),
                                        true,
                                        lang,
                                        move |panel, window, cx| {
                                            panel.mutate(Mutation::Abort(op), window, cx)
                                        },
                                        window,
                                        cx,
                                    );
                                })),
                        ),
                )
                .into_any_element(),
        )
    }

    /// Commit button with a chevron menu: commit & push, amend, undo.
    pub(super) fn commit_split_button(
        &self,
        status: &RepoStatus,
        busy: Option<Operation>,
        lang: Language,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let action = commit_action(status, self.draft(), busy.is_some());
        let label = if status.staged.is_empty() && !status.unstaged.is_empty() {
            Tr::GitCommitAll
        } else {
            Tr::GitCommit
        };
        let panel = cx.weak_entity();
        let can_push = action != CommitAction::Disabled;
        let can_amend = amend_enabled(status, busy.is_some());
        let can_undo = undo_enabled(status, busy.is_some());
        h_flex()
            .gap_px()
            .child(
                Button::new("git-commit")
                    .small()
                    .primary()
                    .flex_1()
                    .label(tr(lang, label))
                    .disabled(action == CommitAction::Disabled)
                    .loading(matches!(busy, Some(Operation::Commit | Operation::CommitPush)))
                    .on_click(cx.listener(|panel, _, window, cx| panel.commit_message(window, cx))),
            )
            .child(
                Button::new("git-commit-more")
                    .small()
                    .primary()
                    .icon(Icon::new(Ic::ChevronDown))
                    .tooltip(tr(lang, Tr::GitCommitMore))
                    .disabled(busy.is_some())
                    .dropdown_menu(move |menu, _, _| {
                        let (push, amend, undo) = (panel.clone(), panel.clone(), panel.clone());
                        menu.item(
                            PopupMenuItem::new(tr(lang, Tr::GitCommitPush))
                                .disabled(!can_push)
                                .on_click(move |_, window, cx| {
                                    let _ = push.update(cx, |panel, cx| panel.commit_and_push(window, cx));
                                }),
                        )
                        .item(
                            PopupMenuItem::new(tr(lang, Tr::GitAmend))
                                .disabled(!can_amend)
                                .on_click(move |_, window, cx| {
                                    let _ = amend.update(cx, |panel, cx| panel.amend_commit(window, cx));
                                }),
                        )
                        .item(
                            PopupMenuItem::new(tr(lang, Tr::GitUndoCommit))
                                .disabled(!can_undo)
                                .on_click(move |_, window, cx| {
                                    let _ = undo.update(cx, |panel, cx| panel.undo_commit(window, cx));
                                }),
                        )
                    }),
            )
    }

    /// The `…` button of the branch header.
    pub(super) fn header_menu(
        &self,
        status: &RepoStatus,
        busy: Option<Operation>,
        pr_visible: bool,
        lang: Language,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let panel = cx.weak_entity();
        let busy = busy.is_some();
        let can_stash = stash_enabled(status, busy);
        tool_button("git-more", Ic::Ellipsis, tr(lang, Tr::MoreActions)).dropdown_menu(
            move |menu, _, _| {
                let (rebase, force, stash, refresh) =
                    (panel.clone(), panel.clone(), panel.clone(), panel.clone());
                let mut menu = menu
                    .item(
                        PopupMenuItem::new(tr(lang, Tr::GitPullRebase))
                            .disabled(busy)
                            .on_click(move |_, window, cx| {
                                let _ = rebase.update(cx, |panel, cx| {
                                    panel.mutate(Mutation::PullRebase, window, cx)
                                });
                            }),
                    )
                    .item(
                        PopupMenuItem::new(tr(lang, Tr::GitForcePush))
                            .disabled(busy)
                            .on_click(move |_, window, cx| {
                                let _ = force.update(cx, |panel, cx| {
                                    panel.confirm(
                                        Tr::GitForcePush,
                                        tr(lang, Tr::GitForcePushConfirm).to_owned(),
                                        true,
                                        lang,
                                        |panel, window, cx| panel.mutate(Mutation::ForcePush, window, cx),
                                        window,
                                        cx,
                                    )
                                });
                            }),
                    )
                    .item(
                        PopupMenuItem::new(tr(lang, Tr::GitStashAll))
                            .disabled(!can_stash)
                            .on_click(move |_, window, cx| {
                                let _ = stash.update(cx, |panel, cx| {
                                    panel.mutate(Mutation::StashPush, window, cx)
                                });
                            }),
                    );
                if pr_visible {
                    menu = menu.separator().item(
                        PopupMenuItem::new(tr(lang, Tr::GitRefreshPr)).on_click(move |_, _, cx| {
                            let _ = refresh.update(cx, |panel, cx| panel.pr_refresh_now(cx));
                        }),
                    );
                }
                menu
            },
        )
    }

    /// Generic confirmation: runs `on_ok` against the panel if the selection did not change.
    pub(super) fn confirm(
        &self,
        title: Tr,
        message: String,
        danger: bool,
        lang: Language,
        on_ok: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let generation = self.generation;
        let panel = cx.weak_entity();
        let on_ok = Rc::new(on_ok);
        worktree_dialogs::open_confirm(
            window,
            cx,
            tr(lang, title).into(),
            message,
            None,
            tr(lang, title),
            danger,
            Rc::new(move |window, cx| {
                let on_ok = on_ok.clone();
                let _ = panel.update(cx, |panel, cx| {
                    panel
                        .generation
                        .apply(generation, || on_ok(panel, window, cx));
                });
            }),
        );
    }

    pub(super) fn commit_and_push(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(status) = &self.status else { return };
        let message = self.draft().to_owned();
        let action = commit_action(status, &message, self.active_operation().is_some());
        if action != CommitAction::Disabled {
            self.mutate(
                Mutation::CommitPush { message, all: action == CommitAction::All },
                window,
                cx,
            );
        }
    }

    pub(super) fn amend_commit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(status) = &self.status else { return };
        if !amend_enabled(status, self.active_operation().is_some()) {
            return;
        }
        let draft = self.draft().to_owned();
        match amend_plan(status, &draft) {
            AmendPlan::LoadMessage => self.load_last_message(window, cx),
            AmendPlan::Run => self.mutate(Mutation::Amend(draft), window, cx),
            AmendPlan::Confirm => {
                let lang = settings::language(cx);
                self.confirm(
                    Tr::GitAmend,
                    tr(lang, Tr::GitAlreadyPushed).to_owned(),
                    true,
                    lang,
                    move |panel, window, cx| panel.mutate(Mutation::Amend(draft.clone()), window, cx),
                    window,
                    cx,
                );
            }
        }
    }

    pub(super) fn undo_commit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(status) = &self.status else { return };
        if !undo_enabled(status, self.active_operation().is_some()) {
            return;
        }
        if is_pushed(status) {
            let lang = settings::language(cx);
            self.confirm(
                Tr::GitUndoCommit,
                tr(lang, Tr::GitAlreadyPushedUndo).to_owned(),
                true,
                lang,
                |panel, window, cx| panel.mutate(Mutation::Undo, window, cx),
                window,
                cx,
            );
        } else {
            self.mutate(Mutation::Undo, window, cx);
        }
    }

    /// Puts the last commit message into the editor so it can be edited and amended.
    fn load_last_message(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(repo), Some(path)) = (self.repo.clone(), self.worktree.clone()) else {
            return;
        };
        let generation = self.generation;
        let load = cx.background_spawn(async move { repo.last_commit_message() });
        cx.spawn_in(window, async move |panel, cx| {
            let result = load.await;
            let _ = panel.update_in(cx, |panel, window, cx| {
                panel.generation.apply(generation, || match result {
                    Ok(Some(message)) if panel.draft().trim().is_empty() => {
                        panel.drafts.insert(path, message.clone());
                        if let Some(input) = &panel.input {
                            input.state.update(cx, |input, cx| input.set_value(message, window, cx));
                        }
                        cx.notify();
                    }
                    Ok(_) => {}
                    Err(error) => {
                        panel.error = Some(error.to_string());
                        cx.notify();
                    }
                });
            });
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str) -> FileChange {
        FileChange {
            path: path.into(),
            orig_path: None,
            status: ChangeKind::Modified,
            added: None,
            removed: None,
            conflict: None,
        }
    }

    fn pushed_status() -> RepoStatus {
        RepoStatus {
            branch: Some("main".into()),
            head_oid: Some("abc".into()),
            upstream: Some("origin/main".into()),
            ..RepoStatus::default()
        }
    }

    #[test]
    fn banner_names_each_operation() {
        assert_eq!(banner_title(InProgress::Rebase), Tr::GitRebaseInProgress);
        assert_eq!(banner_title(InProgress::Merge), Tr::GitMergeInProgress);
        assert_eq!(banner_title(InProgress::CherryPick), Tr::GitCherryPickInProgress);
        assert_eq!(banner_title(InProgress::Revert), Tr::GitRevertInProgress);
        assert_eq!(tr(Language::En, banner_title(InProgress::Rebase)), "Rebase in progress");
        assert_eq!(tr(Language::En, banner_title(InProgress::CherryPick)), "Cherry-pick in progress");
    }

    #[test]
    fn continue_waits_for_conflicts_and_idle_operations() {
        let mut status = pushed_status();
        assert!(continue_enabled(&status, false));
        assert!(!continue_enabled(&status, true));
        status.conflicted.push(file("a.rs"));
        assert!(!continue_enabled(&status, false));
    }

    #[test]
    fn a_commit_is_pushed_only_with_an_upstream_and_nothing_ahead() {
        let mut status = pushed_status();
        assert!(is_pushed(&status));
        status.ahead = 1;
        assert!(!is_pushed(&status));
        status.ahead = 0;
        status.upstream = None;
        assert!(!is_pushed(&status));
        status.upstream = Some("origin/main".into());
        status.head_oid = None;
        assert!(!is_pushed(&status), "no HEAD, nothing to be pushed");
    }

    #[test]
    fn amend_needs_a_head_and_no_conflicts() {
        let mut status = pushed_status();
        assert!(amend_enabled(&status, false));
        assert!(!amend_enabled(&status, true));
        status.conflicted.push(file("a.rs"));
        assert!(!amend_enabled(&status, false));
        assert!(!amend_enabled(&RepoStatus::default(), false));
    }

    #[test]
    fn undo_needs_a_head_and_no_operation_in_progress() {
        let mut status = pushed_status();
        assert!(undo_enabled(&status, false));
        assert!(!undo_enabled(&status, true));
        assert!(!undo_enabled(&RepoStatus::default(), false));
        status.in_progress = Some(InProgress::Rebase);
        assert!(!undo_enabled(&status, false));
    }

    #[test]
    fn amend_plan_loads_the_message_confirms_pushed_commits_or_runs() {
        let mut status = pushed_status();
        assert_eq!(amend_plan(&status, ""), AmendPlan::LoadMessage);
        assert_eq!(amend_plan(&status, "  \n"), AmendPlan::LoadMessage);
        assert_eq!(amend_plan(&status, "fix typo"), AmendPlan::Confirm);
        status.ahead = 1;
        assert_eq!(amend_plan(&status, "fix typo"), AmendPlan::Run);
        status.ahead = 0;
        status.upstream = None;
        assert_eq!(amend_plan(&status, "fix typo"), AmendPlan::Run);
        // An empty draft always loads the message first, even for a pushed commit.
        assert_eq!(amend_plan(&pushed_status(), ""), AmendPlan::LoadMessage);
    }

    #[test]
    fn commit_and_push_follows_the_commit_enable_rule() {
        let mut status = pushed_status();
        assert_eq!(commit_action(&status, "msg", false), CommitAction::Disabled);
        status.unstaged.push(file("a.rs"));
        assert_eq!(commit_action(&status, "msg", false), CommitAction::All);
        status.staged.push(file("b.rs"));
        assert_eq!(commit_action(&status, "msg", false), CommitAction::Staged);
        assert_eq!(commit_action(&status, "", false), CommitAction::Disabled);
        assert_eq!(commit_action(&status, "msg", true), CommitAction::Disabled);
    }

    #[test]
    fn stash_needs_changes_and_no_conflicts() {
        let mut status = pushed_status();
        assert!(!stash_enabled(&status, false));
        status.unstaged.push(file("a.rs"));
        assert!(stash_enabled(&status, false));
        assert!(!stash_enabled(&status, true));
        status.conflicted.push(file("c.rs"));
        assert!(!stash_enabled(&status, false));
        let mut staged_only = pushed_status();
        staged_only.staged.push(file("a.rs"));
        assert!(stash_enabled(&staged_only, false));
    }

    #[test]
    fn mutations_report_their_operation_draft_and_history_behavior() {
        let amend = Mutation::Amend("fix".into());
        assert_eq!(amend.operation(), Operation::Amend);
        assert_eq!(amend.draft_message(), Some("fix"));
        let both = Mutation::CommitPush { message: "m".into(), all: true };
        assert_eq!(both.operation(), Operation::CommitPush);
        assert_eq!(both.draft_message(), Some("m"));
        assert_eq!(Mutation::Undo.draft_message(), None);
        assert_eq!(Mutation::StashPush.operation(), Operation::Stash);
        assert_eq!(Mutation::Continue(InProgress::Merge).operation(), Operation::Continue);
        assert_eq!(Mutation::Abort(InProgress::Merge).operation(), Operation::Abort);
        assert!(Operation::CommitPush.changes_history());
        assert!(Operation::Undo.changes_history());
        assert!(!Operation::Stash.changes_history());
        assert!(!Operation::Stage.changes_history());
        assert!(Operation::Push.affects_pr() && Operation::CommitPush.affects_pr());
        assert!(Operation::ForcePush.affects_pr() && Operation::Amend.affects_pr());
        assert!(!Operation::Stash.affects_pr() && !Operation::Commit.affects_pr());
    }
}
