//! PR tab section: files (placeholder until implemented).

use gpui_kit::{AnyElement, Context};
use sich_git::forge::PullRequest;

use super::GitPanel;
use crate::i18n::Language;

impl GitPanel {
    /// Section shown in the Pull request tab under the PR card; `None` hides it.
    pub(super) fn pr_files_section(
        &self,
        _pr: &PullRequest,
        _lang: Language,
        _cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        None
    }
}
