//! Snapshots emitted by Sich's omp extension. No transcripts or agent config are read.

use serde::Deserialize;
use sich_proto::{AgentKind, AgentState, AgentStatus, SubagentStatus};

use super::{MAX_SUBAGENTS, normalize_text};

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum State {
    Idle,
    Working,
    Waiting,
    Done,
    Failed,
    Exited,
}

#[derive(Deserialize)]
pub(super) struct Snapshot<'a> {
    session_id: &'a str,
    state: State,
    detail: Option<String>,
    model: Option<String>,
    title: Option<String>,
    prompt: Option<String>,
    message: Option<String>,
    finished_unix_ms: Option<u64>,
    #[serde(default)]
    subagents: Vec<Subagent>,
}

#[derive(Deserialize)]
struct Subagent {
    id: String,
    state: State,
    agent_type: Option<String>,
    description: Option<String>,
    detail: Option<String>,
    since_unix_ms: Option<u64>,
}

impl Subagent {
    fn status(self, now: u64) -> Option<SubagentStatus> {
        let state = match self.state {
            State::Working => AgentState::Working,
            State::Waiting => AgentState::Waiting,
            _ => return None,
        };
        if self.id.trim().is_empty() || self.id.len() > 256 || self.id.chars().any(char::is_control) {
            return None;
        }
        Some(SubagentStatus {
            id: self.id,
            state,
            agent_type: self.agent_type.as_deref().and_then(|text| normalize_text(text, false)),
            description: self.description.as_deref().and_then(|text| normalize_text(text, false)),
            detail: super::non_empty(&self.detail),
            since_unix_ms: self.since_unix_ms.unwrap_or(now).min(now),
        })
    }
}

impl<'a> Snapshot<'a> {
    pub(super) fn parse(json: &'a str) -> Option<Self> {
        let snapshot: Self = serde_json::from_str(json).ok()?;
        let id = snapshot.session_id.as_bytes();
        (!id.is_empty() && id.len() <= 128 && id.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'-'))
            .then_some(snapshot)
    }

    pub(super) fn owns(&self, status: &AgentStatus) -> bool {
        status.kind == AgentKind::Omp && status.session_id.as_deref() == Some(self.session_id)
    }

    pub(super) fn status(self, prev: Option<&AgentStatus>, now: u64) -> Option<AgentStatus> {
        let state = match self.state {
            State::Idle => AgentState::Idle,
            State::Working => AgentState::Working,
            State::Waiting => AgentState::Waiting,
            State::Done => AgentState::Done,
            State::Failed => AgentState::Failed,
            State::Exited => return None,
        };
        let finished = matches!(state, AgentState::Done | AgentState::Failed);
        // A child-only update repeats the parent's finish time; it is not a new unread turn.
        let finish_time = self.finished_unix_ms.filter(|_| finished);
        let same_state = prev.filter(|s| {
            self.owns(s) && s.state == state && finish_time.is_none_or(|stamp| stamp == s.since_unix_ms)
        });
        let since_unix_ms = same_state.map_or_else(|| finish_time.unwrap_or(now), |s| s.since_unix_ms);
        let mut subagents: Vec<SubagentStatus> = Vec::with_capacity(self.subagents.len().min(MAX_SUBAGENTS));
        for subagent in self.subagents.into_iter().filter_map(|s| s.status(now)) {
            if !subagents.iter().any(|s| s.id == subagent.id) {
                subagents.push(subagent);
                if subagents.len() == MAX_SUBAGENTS {
                    break;
                }
            }
        }
        Some(AgentStatus {
            kind: AgentKind::Omp,
            state,
            unread: finished && same_state.is_none_or(|s| s.unread),
            since_unix_ms,
            detail: super::non_empty(&self.detail),
            model: super::non_empty(&self.model),
            title: self.title.as_deref().and_then(|text| normalize_text(text, false)),
            prompt: self.prompt.as_deref().and_then(|text| normalize_text(text, true)),
            message: self.message.as_deref().and_then(|text| normalize_text(text, false)),
            subagents,
            session_id: Some(self.session_id.to_owned()),
        })
    }
}
