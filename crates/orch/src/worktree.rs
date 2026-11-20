use crate::model::DispatchState;

pub fn should_remove(
    state: DispatchState,
    has_live_terminal: bool,
    dirty: bool,
    merged: bool,
    enabled: bool,
    already_cleaned: bool,
) -> bool {
    matches!(state, DispatchState::Released | DispatchState::Lost)
        && !has_live_terminal
        && !dirty
        && merged
        && enabled
        && !already_cleaned
}

#[cfg(test)]
mod tests {
    use super::*;
    use DispatchState::*;

    #[test]
    fn only_closed_and_safe_worktrees_are_removed() {
        for state in [Starting, Working, Done, Failed, Lost, Released] {
            for flags in 0u8..32 {
                let bit = |n: u32| flags & (1u8 << n) != 0;
                assert_eq!(
                    should_remove(state, bit(0), bit(1), bit(2), bit(3), bit(4)),
                    matches!(state, Lost | Released) && flags == 0b01100,
                    "{state:?}, flags={flags:05b}"
                );
            }
        }
    }
}
