//! Diff notes shelf (placeholder until implemented).

use gpui_kit::{AnyElement, Context};

use super::GitPanel;
use crate::i18n::Language;

impl GitPanel {
    pub(super) fn notes_section(&self, _lang: Language, _cx: &mut Context<Self>) -> Option<AnyElement> {
        None
    }
}
