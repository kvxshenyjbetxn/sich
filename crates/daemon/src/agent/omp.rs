//! Snapshots emitted by Sich's omp extension. No transcripts or agent config are read.

use serde::Deserialize;
use sich_proto::{AgentKind, AgentState, AgentStatus};

use super::normalize_text;

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
        let since_unix_ms = prev
            .filter(|s| self.owns(s) && s.state == state)
            .map_or(now, |s| s.since_unix_ms);
        Some(AgentStatus {
            kind: AgentKind::Omp,
            state,
            unread: matches!(state, AgentState::Done | AgentState::Failed),
            since_unix_ms,
            detail: super::non_empty(&self.detail),
            model: super::non_empty(&self.model),
            title: self.title.as_deref().and_then(|text| normalize_text(text, false)),
            prompt: self.prompt.as_deref().and_then(|text| normalize_text(text, true)),
            message: self.message.as_deref().and_then(|text| normalize_text(text, false)),
            subagents: Vec::new(),
            session_id: Some(self.session_id.to_owned()),
        })
    }
}
