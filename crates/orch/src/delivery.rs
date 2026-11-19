//! When to type a "you have mail" pointer into a terminal. Pure decision function.

use crate::model::AgentObs;

/// The agent must have been settled this long before we type anything.
pub const SETTLE_MS: i64 = 1500;
/// Never type into a terminal the user typed into this recently.
pub const USER_TYPING_QUIET_MS: i64 = 3000;
/// Re-point at most this often while mail stays unread.
pub const REPOINT_MS: i64 = 60_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeliveryInput {
    pub unread: u32,
    pub max_unread_id: i64,
    pub last_pointed_id: Option<i64>,
    /// When a pointer was last typed into this terminal (unix ms).
    pub last_pointed_ms: Option<i64>,
    pub agent: AgentObs,
    /// How long the agent's state has been the current one.
    pub settled_ms: i64,
    /// How long ago the user last typed into the terminal (`None` = never in this run).
    pub user_idle_ms: Option<i64>,
    pub now_ms: i64,
}

/// Why a pointer is (not) typed; the reason doubles as documentation in tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Point,
    NoMail,
    AgentBusy,
    NotSettled,
    UserTyping,
    AlreadyPointed,
}

pub fn decide(i: &DeliveryInput) -> Decision {
    if i.unread == 0 {
        return Decision::NoMail;
    }
    // Only an agent sitting at its prompt may be typed into: never while it works or waits for
    // a permission answer, and never into a plain shell (no agent).
    if !matches!(i.agent, AgentObs::Done | AgentObs::Idle) {
        return Decision::AgentBusy;
    }
    if i.settled_ms < SETTLE_MS {
        return Decision::NotSettled;
    }
    if i.user_idle_ms.is_some_and(|ms| ms < USER_TYPING_QUIET_MS) {
        return Decision::UserTyping;
    }
    let new_mail = i.last_pointed_id.is_none_or(|last| i.max_unread_id > last);
    if new_mail {
        return Decision::Point;
    }
    // Same mail still unread: nag, but rarely.
    match i.last_pointed_ms {
        Some(at) if i.now_ms - at < REPOINT_MS => Decision::AlreadyPointed,
        _ => Decision::Point,
    }
}

pub fn should_point(input: &DeliveryInput) -> bool {
    decide(input) == Decision::Point
}

/// The text typed (as a bracketed paste) before Enter. `exe` is already quoted if needed.
pub fn pointer_text(exe: &str, unread: u32) -> String {
    let noun = if unread == 1 { "message" } else { "messages" };
    format!("You have {unread} Sich orchestration {noun}. Run `{exe} orch check`.")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> DeliveryInput {
        DeliveryInput {
            unread: 1,
            max_unread_id: 5,
            last_pointed_id: None,
            last_pointed_ms: None,
            agent: AgentObs::Idle,
            settled_ms: 10_000,
            user_idle_ms: None,
            now_ms: 1_000_000,
        }
    }

    #[test]
    fn points_when_unread_mail_and_agent_is_idle() {
        assert_eq!(decide(&base()), Decision::Point);
        assert!(should_point(&DeliveryInput { agent: AgentObs::Done, ..base() }));
    }

    #[test]
    fn no_mail_no_pointer() {
        assert_eq!(decide(&DeliveryInput { unread: 0, ..base() }), Decision::NoMail);
    }

    #[test]
    fn never_while_working_waiting_failed_or_no_agent() {
        for agent in [AgentObs::Working, AgentObs::Waiting, AgentObs::Failed, AgentObs::None] {
            assert_eq!(decide(&DeliveryInput { agent, ..base() }), Decision::AgentBusy, "{agent:?}");
        }
    }

    #[test]
    fn needs_a_settled_agent() {
        assert_eq!(decide(&DeliveryInput { settled_ms: 1499, ..base() }), Decision::NotSettled);
        assert_eq!(decide(&DeliveryInput { settled_ms: 1500, ..base() }), Decision::Point);
        assert_eq!(decide(&DeliveryInput { settled_ms: 0, ..base() }), Decision::NotSettled);
    }

    #[test]
    fn never_right_after_the_user_typed() {
        assert_eq!(decide(&DeliveryInput { user_idle_ms: Some(2999), ..base() }), Decision::UserTyping);
        assert_eq!(decide(&DeliveryInput { user_idle_ms: Some(3000), ..base() }), Decision::Point);
        assert_eq!(decide(&DeliveryInput { user_idle_ms: Some(0), ..base() }), Decision::UserTyping);
    }

    #[test]
    fn new_mail_after_a_pointer_points_again_immediately() {
        let i = DeliveryInput {
            last_pointed_id: Some(5),
            last_pointed_ms: Some(1_000_000 - 1000),
            max_unread_id: 6,
            ..base()
        };
        assert_eq!(decide(&i), Decision::Point);
    }

    #[test]
    fn same_unread_mail_is_re_pointed_at_most_once_a_minute() {
        let pointed = DeliveryInput { last_pointed_id: Some(5), last_pointed_ms: Some(1_000_000 - 59_999), ..base() };
        assert_eq!(decide(&pointed), Decision::AlreadyPointed);
        let later = DeliveryInput { last_pointed_ms: Some(1_000_000 - 60_000), ..pointed };
        assert_eq!(decide(&later), Decision::Point);
        // A pointer without a timestamp (should not happen) must not block forever.
        assert_eq!(decide(&DeliveryInput { last_pointed_ms: None, ..pointed }), Decision::Point);
    }

    #[test]
    fn pointer_text_is_exact() {
        assert_eq!(
            pointer_text("C:/Apps/sich.exe", 1),
            "You have 1 Sich orchestration message. Run `C:/Apps/sich.exe orch check`."
        );
        assert!(pointer_text("sich", 3).starts_with("You have 3 Sich orchestration messages."));
    }
}
