//! Result of the last release check, shown in the settings dialog.

use gpui_kit::{App, Global};

use crate::i18n::{self, Language, Tr, tr};

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum CheckStatus {
    #[default]
    Idle,
    Checking,
    UpToDate,
    Available(String),
    Failed(String),
    Dev,
}

impl Global for CheckStatus {}

pub fn get(cx: &App) -> CheckStatus {
    cx.try_global::<CheckStatus>().cloned().unwrap_or_default()
}

pub fn set(status: CheckStatus, cx: &mut App) {
    cx.set_global(status);
    cx.refresh_windows();
}

/// Status line text; `current` is the running version. Returns `(text, is_error)`.
pub fn text(lang: Language, status: &CheckStatus, current: &str) -> (String, bool) {
    match status {
        CheckStatus::Idle => (format!("v{current}"), false),
        CheckStatus::Checking => (tr(lang, Tr::CheckingUpdates).to_string(), false),
        CheckStatus::UpToDate => (i18n::up_to_date(lang, current), false),
        CheckStatus::Available(v) => (i18n::update_available(lang, v), false),
        CheckStatus::Failed(msg) => (i18n::check_failed(lang, msg), true),
        CheckStatus::Dev => (tr(lang, Tr::UpdatesDevBuild).to_string(), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_every_state_in_every_language() {
        for lang in Language::ALL {
            let (idle, err) = text(lang, &CheckStatus::Idle, "1.0.0");
            assert_eq!(idle, "v1.0.0");
            assert!(!err);
            assert_eq!(text(lang, &CheckStatus::Checking, "1.0.0").0, tr(lang, Tr::CheckingUpdates));
            assert!(text(lang, &CheckStatus::UpToDate, "1.0.0").0.contains("1.0.0"));
            assert!(text(lang, &CheckStatus::Available("2.0.0".into()), "1.0.0").0.contains("2.0.0"));
            assert_eq!(text(lang, &CheckStatus::Dev, "1.0.0").0, tr(lang, Tr::UpdatesDevBuild));
        }
    }

    #[test]
    fn only_failure_is_an_error() {
        let (msg, err) = text(Language::En, &CheckStatus::Failed("timeout".into()), "1.0.0");
        assert!(err);
        assert!(msg.contains("timeout"));
        for s in [CheckStatus::Idle, CheckStatus::Checking, CheckStatus::UpToDate, CheckStatus::Dev] {
            assert!(!text(Language::En, &s, "1.0.0").1);
        }
    }

    #[test]
    fn default_is_idle() {
        assert_eq!(CheckStatus::default(), CheckStatus::Idle);
    }
}
