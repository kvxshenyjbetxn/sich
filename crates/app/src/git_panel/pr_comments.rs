//! PR tab section: comments of the pull request (review threads, conversation, reviews).
//!
//! The section is its own GPUI entity (`PrComments`) owned by the Git panel; the panel only
//! feeds it the PR the tab shows. Logic that can be tested without GPUI is in `state.rs`; the
//! `gh api` calls and the grouping live in `sich_git::forge::comments`.

mod render;
mod state;

pub(super) use state::strip_html_comments;
mod view;

use gpui_kit::{AnyElement, Context, IntoElement as _};
use sich_git::forge::PullRequest;

pub(crate) use view::PrComments;

use super::GitPanel;
use crate::i18n::Language;

impl GitPanel {
    /// Section shown in the Pull request tab under the PR card; `None` hides it.
    pub(super) fn pr_comments_section(
        &self,
        pr: &PullRequest,
        lang: Language,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let worktree = self.worktree.clone()?;
        let repo = self.pr_state(cx)?.target?.repo;
        let ctx = view::Ctx { worktree, repo, pr: pr.clone() };
        self.comments.update(cx, |comments, cx| comments.set_context(ctx, lang, cx));
        Some(self.comments.clone().into_any_element())
    }
}
