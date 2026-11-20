//! Lifetime policy for orchestration workers, shared by the GUI and release transaction.

use crate::model::{DispatchState, OrchSnapshot};

pub const AUTO_CLOSE_DELAY_MS: i64 = 120_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoClose {
    Running,
    ClosesAt(i64),
    Kept,
}

pub fn auto_close_state(
    state: DispatchState,
    keep_open: bool,
    ended_ms: Option<i64>,
    auto_close_allowed_ms: Option<i64>,
    enabled: bool,
) -> Option<AutoClose> {
    if !enabled || !state.is_live() {
        return None;
    }
    if keep_open {
        return Some(AutoClose::Kept);
    }
    match state {
        DispatchState::Starting | DispatchState::Working => Some(AutoClose::Running),
        DispatchState::Done | DispatchState::Failed => ended_ms.map(|ended_ms| {
            let from = ended_ms.max(auto_close_allowed_ms.unwrap_or(ended_ms));
            AutoClose::ClosesAt(from.saturating_add(AUTO_CLOSE_DELAY_MS))
        }),
        DispatchState::Lost | DispatchState::Released => None,
    }
}

pub fn due_for_close(state: Option<AutoClose>, now_ms: i64) -> bool {
    matches!(state, Some(AutoClose::ClosesAt(deadline)) if deadline <= now_ms)
}

pub(crate) fn due_dispatches(snapshot: &OrchSnapshot, enabled: bool, now_ms: i64) -> Vec<String> {
    snapshot
        .workers
        .values()
        .filter_map(|worker| {
            due_for_close(worker.auto_close_state(enabled), now_ms)
                .then(|| worker.dispatch_id.clone())
        })
        .collect()
}

pub fn countdown_seconds(remaining_ms: i64) -> i64 {
    let remaining_ms = remaining_ms.max(0);
    remaining_ms / 1_000 + i64::from(remaining_ms % 1_000 != 0)
}

pub fn format_countdown(remaining_ms: i64) -> String {
    let seconds = countdown_seconds(remaining_ms);
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use DispatchState::*;

    #[test]
    fn only_live_workers_with_auto_close_enabled_have_a_state() {
        for state in [Starting, Working] {
            assert_eq!(
                auto_close_state(state, false, None, None, true),
                Some(AutoClose::Running)
            );
        }
        for state in [Lost, Released] {
            assert_eq!(auto_close_state(state, true, Some(5), None, true), None);
        }
        assert_eq!(auto_close_state(Done, false, Some(5), None, false), None);
        assert_eq!(
            auto_close_state(Done, true, Some(5), None, true),
            Some(AutoClose::Kept)
        );
        assert_eq!(auto_close_state(Failed, false, None, None, true), None);
    }

    #[test]
    fn finished_workers_are_due_at_two_minutes_from_finish_or_allow() {
        for state in [Done, Failed] {
            let ordinary = auto_close_state(state, false, Some(10_000), None, true);
            assert_eq!(ordinary, Some(AutoClose::ClosesAt(130_000)));
            assert!(!due_for_close(ordinary, 129_999));
            assert!(due_for_close(ordinary, 130_000));
            assert_eq!(
                auto_close_state(state, false, Some(10_000), Some(40_000), true),
                Some(AutoClose::ClosesAt(160_000)),
            );
            assert_eq!(
                auto_close_state(state, false, Some(10_000), Some(9_999), true),
                ordinary
            );
        }
        let resumed = auto_close_state(Working, false, None, Some(40_000), true);
        assert_eq!(resumed, Some(AutoClose::Running));
        assert!(!due_for_close(resumed, 200_000));
    }

    #[test]
    fn countdown_rounds_up_without_displaying_negative_time() {
        for (ms, expected) in [
            (-10, "0:00"),
            (0, "0:00"),
            (1, "0:01"),
            (59_001, "1:00"),
            (60_001, "1:01"),
            (120_000, "2:00"),
        ] {
            assert_eq!(format_countdown(ms), expected);
        }
    }
}
