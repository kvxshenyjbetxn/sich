//! Configurable shell commands and the lifetime of temporary command terminals.

use gpui_kit::{Context, Div, Styled as _};
use serde::{Deserialize, Serialize};
use sich_orch::auto_close::{AUTO_CLOSE_DELAY_MS, AutoClose};
use sich_proto::TermId;

use crate::app_view::AppView;
use crate::i18n::{Language, Tr};
use crate::{orch_banner, settings};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuickCommand {
    pub name: String,
    pub command: String,
}

pub fn clean(name: &str, command: &str) -> Option<QuickCommand> {
    let name = name.trim();
    let command = command.trim();
    (!name.is_empty() && !command.is_empty()).then(|| QuickCommand {
        name: name.to_owned(),
        command: command.to_owned(),
    })
}

pub fn requires_temporary(has_agent: bool, exited: bool) -> bool {
    has_agent || exited
}

/// Submit the command and exit with Enter (CR). PSReadLine treats LF as a continuation.
/// A separate submission also prevents a trailing comment from swallowing `exit`.
pub fn startup_command(command: &str) -> String {
    format!("{command}\rexit")
}

pub fn command_input(command: &str) -> Vec<u8> {
    format!("{command}\r").into_bytes()
}

#[derive(Clone, Debug, Default)]
pub(crate) struct TemporaryCommand {
    ended_ms: Option<i64>,
    keep_open: bool,
    auto_close_allowed_ms: Option<i64>,
}

impl TemporaryCommand {
    pub(crate) fn observe(&mut self, exited: bool, now_ms: i64) {
        if exited && self.ended_ms.is_none() {
            self.ended_ms = Some(now_ms);
        }
    }

    pub(crate) fn auto_close_state(&self, enabled: bool) -> Option<AutoClose> {
        if self.keep_open {
            return Some(AutoClose::Kept);
        }
        if !enabled {
            return None;
        }
        if let Some(ended) = self.ended_ms {
            let from = ended.max(self.auto_close_allowed_ms.unwrap_or(ended));
            Some(AutoClose::ClosesAt(from.saturating_add(AUTO_CLOSE_DELAY_MS)))
        } else {
            Some(AutoClose::Running)
        }
    }

    fn toggle_keep_open(&mut self, now_ms: i64) {
        self.keep_open = !self.keep_open;
        self.auto_close_allowed_ms = (!self.keep_open).then_some(now_ms);
    }
}

pub(crate) fn banner(
    id: TermId,
    command: &TemporaryCommand,
    now_ms: i64,
    lang: Language,
    cx: &mut Context<AppView>,
) -> Option<Div> {
    let enabled = command.keep_open || settings::get(cx).orch_auto_close_workers;
    let state = command.auto_close_state(enabled)?;
    Some(
        orch_banner::lifetime_banner(
            state,
            now_ms,
            lang,
            (Tr::QuickCommandRunning, Tr::QuickCommandFinished),
            cx,
            cx.listener(move |this, _, _, cx| {
                if let Some(command) = this.quick_terminals.get_mut(&id) {
                    command.toggle_keep_open(sich_orch::paths::now_ms());
                    cx.notify();
                }
            }),
        )
        .absolute()
        .top_0()
        .right_0()
        .left_0(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use sich_orch::auto_close::due_for_close;

    #[test]
    fn validation_trims_and_rejects_empty_fields() {
        assert_eq!(clean("  Tests  ", "  cargo test  "), Some(QuickCommand {
            name: "Tests".into(),
            command: "cargo test".into(),
        }));
        assert_eq!(clean(" ", "cargo test"), None);
        assert_eq!(clean("Tests", "\n"), None);
    }

    #[test]
    fn agents_and_exited_shells_need_a_temporary_terminal() {
        assert!(!requires_temporary(false, false));
        assert!(requires_temporary(true, false));
        assert!(requires_temporary(false, true));
        assert!(requires_temporary(true, true));
    }

    #[test]
    fn temporary_shell_submits_command_and_exit_with_enter() {
        assert_eq!(command_input(&startup_command("cargo test # explanation")), b"cargo test # explanation\rexit\r");
        assert_eq!(command_input("cargo test"), b"cargo test\r");
    }

    #[test]
    fn completion_arms_once_and_closes_at_the_boundary() {
        let mut command = TemporaryCommand::default();
        command.observe(false, 100);
        assert_eq!(command.auto_close_state(true), Some(AutoClose::Running));
        assert!(!due_for_close(command.auto_close_state(true), 1_000_000));

        command.observe(true, 1_000);
        command.observe(true, 5_000);
        let state = command.auto_close_state(true);
        assert_eq!(state, Some(AutoClose::ClosesAt(121_000)));
        assert!(!due_for_close(state, 120_999));
        assert!(due_for_close(state, 121_000));
    }

    #[test]
    fn keep_open_and_reallow_match_activity_tabs() {
        let mut command = TemporaryCommand::default();
        command.observe(true, 1_000);
        command.toggle_keep_open(2_000);
        assert_eq!(command.auto_close_state(false), Some(AutoClose::Kept));
        assert!(!due_for_close(command.auto_close_state(true), i64::MAX));

        command.toggle_keep_open(10_000);
        assert_eq!(command.auto_close_state(true), Some(AutoClose::ClosesAt(130_000)));
        command.observe(true, 20_000);
        assert_eq!(command.auto_close_state(true), Some(AutoClose::ClosesAt(130_000)));
    }
}
