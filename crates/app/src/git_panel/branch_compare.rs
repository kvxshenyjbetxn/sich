//! Branch compare against a base ref (placeholder until implemented).

use gpui_kit::{AnyElement, Context};
use sich_git::RepoStatus;

use super::GitPanel;
use crate::i18n::Language;

impl GitPanel {
    /// Compact row under the PR row: base ref, ahead/behind, line totals.
    pub(super) fn branch_compare_row(&self, _status: &RepoStatus, _lang: Language, _cx: &mut Context<Self>) -> Option<AnyElement> {
        None
    }

    /// Collapsible "Branch changes" file list (merge-base..HEAD).
    pub(super) fn branch_changes_section(&self, _status: &RepoStatus, _lang: Language, _cx: &mut Context<Self>) -> Option<AnyElement> {
        None
    }
}
