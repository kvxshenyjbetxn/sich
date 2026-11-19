//! Detects workers that went quiet without reporting, or whose terminal is gone.

use crate::model::{AgentObs, DispatchState};

/// A turn that ended this long ago, with no `worker_done`, is reported.
pub const STOPPED_AFTER_MS: i64 = 5000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EscalationInput {
    pub state: DispatchState,
    /// The terminal exists and its process has not exited.
    pub terminal_alive: bool,
    pub agent: AgentObs,
    /// Unix ms the agent entered its current state (`0` when unknown).
    pub agent_since_ms: i64,
    /// The latest task start or message sent to this worker (unix ms).
    pub last_activity_ms: i64,
    /// An escalation for this dispatch was already raised after `last_activity_ms`.
    pub already_escalated: bool,
    pub now_ms: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Escalation {
    None,
    /// The agent ended its turn without `worker_done`.
    Stopped,
    /// The terminal is gone: the dispatch becomes `lost`.
    Exited,
}

impl Escalation {
    pub fn reason(self) -> &'static str {
        match self {
            Self::None => "",
            Self::Stopped => "stopped without reporting",
            Self::Exited => "exited",
        }
    }
}

pub fn decide(i: &EscalationInput) -> Escalation {
    match i.state {
        DispatchState::Starting | DispatchState::Working if !i.terminal_alive => Escalation::Exited,
        DispatchState::Working => {
            if i.already_escalated {
                return Escalation::None;
            }
            let turn_ended = matches!(i.agent, AgentObs::Done | AgentObs::Idle | AgentObs::Failed);
            // The turn must have ended after the task/message was handed over, otherwise this is
            // the idle prompt from before the work started.
            let after_activity = i.agent_since_ms >= i.last_activity_ms;
            if turn_ended && after_activity && i.now_ms - i.agent_since_ms >= STOPPED_AFTER_MS {
                Escalation::Stopped
            } else {
                Escalation::None
            }
        }
        // `done`/`failed` already reported; `starting` is the CLI's business; the rest is over.
        _ => Escalation::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn working() -> EscalationInput {
        EscalationInput {
            state: DispatchState::Working,
            terminal_alive: true,
            agent: AgentObs::Done,
            agent_since_ms: 100_000,
            last_activity_ms: 50_000,
            already_escalated: false,
            now_ms: 105_000,
        }
    }

    #[test]
    fn stopped_after_five_seconds_without_a_report() {
        assert_eq!(decide(&working()), Escalation::Stopped);
        assert_eq!(decide(&EscalationInput { now_ms: 104_999, ..working() }), Escalation::None);
        assert_eq!(decide(&EscalationInput { agent: AgentObs::Idle, ..working() }), Escalation::Stopped);
        assert_eq!(decide(&EscalationInput { agent: AgentObs::Failed, ..working() }), Escalation::Stopped);
    }

    #[test]
    fn a_busy_agent_is_not_stopped() {
        for agent in [AgentObs::Working, AgentObs::Waiting, AgentObs::None] {
            assert_eq!(decide(&EscalationInput { agent, ..working() }), Escalation::None, "{agent:?}");
        }
    }

    #[test]
    fn never_duplicates() {
        assert_eq!(decide(&EscalationInput { already_escalated: true, ..working() }), Escalation::None);
    }

    #[test]
    fn an_idle_prompt_from_before_the_latest_message_does_not_count() {
        // Coordinator sent a message at 103s; the agent has been Done since 100s.
        let i = EscalationInput { last_activity_ms: 103_000, now_ms: 120_000, ..working() };
        assert_eq!(decide(&i), Escalation::None);
    }

    #[test]
    fn exited_terminal_marks_the_dispatch_lost() {
        let gone = EscalationInput { terminal_alive: false, ..working() };
        assert_eq!(decide(&gone), Escalation::Exited);
        assert_eq!(decide(&EscalationInput { state: DispatchState::Starting, ..gone }), Escalation::Exited);
        // Even when an escalation was raised before, the exit is reported once (state flips to lost).
        assert_eq!(decide(&EscalationInput { already_escalated: true, ..gone }), Escalation::Exited);
    }

    #[test]
    fn reported_or_finished_dispatches_are_left_alone() {
        for state in [DispatchState::Done, DispatchState::Failed, DispatchState::Lost, DispatchState::Released] {
            let i = EscalationInput { state, terminal_alive: false, ..working() };
            assert_eq!(decide(&i), Escalation::None, "{state:?}");
            assert_eq!(decide(&EscalationInput { state, ..working() }), Escalation::None, "{state:?}");
        }
    }

    #[test]
    fn reasons() {
        assert_eq!(Escalation::Stopped.reason(), "stopped without reporting");
        assert_eq!(Escalation::Exited.reason(), "exited");
    }
}
