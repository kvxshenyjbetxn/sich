//! Conflicted files section (placeholder: plain list until implemented).

use gpui_kit::{AnyElement, Context, IntoElement as _};
use sich_git::RepoStatus;

use super::{GitPanel, Operation, Section};
use crate::i18n::Language;

impl GitPanel {
    pub(super) fn conflicts_section(
        &self,
        status: &RepoStatus,
        busy: Option<Operation>,
        lang: Language,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        (!status.conflicted.is_empty()).then(|| {
            self.change_section(Section::Conflicts, &status.conflicted, busy, lang, cx).into_any_element()
        })
    }
}
