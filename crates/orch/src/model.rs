//! Plain data types shared by the database, the CLI and the GUI watcher.

use std::collections::{HashMap, HashSet};

macro_rules! string_enum {
    ($(#[$m:meta])* $name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$m])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum $name { $($variant),+ }

        impl $name {
            pub fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $text),+ }
            }

            pub fn parse(text: &str) -> Option<Self> {
                match text { $($text => Some(Self::$variant),)+ _ => None }
            }
        }
    };
}

string_enum!(Agent { Claude => "claude", Codex => "codex" });
string_enum!(TaskStatus { Pending => "pending", Dispatched => "dispatched", Completed => "completed", Failed => "failed" });
string_enum!(DispatchState {
    Starting => "starting",
    Working => "working",
    Done => "done",
    Failed => "failed",
    Lost => "lost",
    Released => "released",
});
string_enum!(MsgType {
    Message => "message",
    Question => "question",
    Reply => "reply",
    WorkerDone => "worker_done",
    Escalation => "escalation",
});
string_enum!(Outcome { Succeeded => "succeeded", Failed => "failed" });

impl DispatchState {
    /// The worker agent may still be running and reachable.
    pub fn is_live(self) -> bool {
        !matches!(self, Self::Lost | Self::Released)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Endpoint {
    Coordinator,
    /// A worker, by dispatch id.
    Worker(String),
    System,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
    pub id: String,
    pub coordinator_term: u64,
    pub created_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Task {
    pub id: String,
    pub run_id: String,
    pub title: String,
    pub spec: String,
    pub status: TaskStatus,
    pub result_summary: Option<String>,
    pub report_path: Option<String>,
    pub files_modified: Option<String>,
    pub created_ms: i64,
    pub updated_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dispatch {
    pub id: String,
    pub task_id: String,
    pub run_id: String,
    pub worker_term: u64,
    pub agent: Agent,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub state: DispatchState,
    pub outcome: Option<String>,
    pub started_ms: i64,
    pub ended_ms: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub id: i64,
    pub run_id: String,
    pub from: Endpoint,
    pub to: Endpoint,
    pub kind: MsgType,
    pub subject: String,
    pub body: String,
    pub reply_to: Option<i64>,
    /// JSON with structured fields (`worker_done`, escalations).
    pub payload: Option<String>,
    pub created_ms: i64,
    pub read_ms: Option<i64>,
}

#[derive(Clone, Debug)]
pub struct NewMessage<'a> {
    pub run_id: &'a str,
    pub from: Endpoint,
    pub to: Endpoint,
    pub kind: MsgType,
    pub subject: &'a str,
    pub body: &'a str,
    pub reply_to: Option<i64>,
    pub payload: Option<&'a str>,
}

/// Which side of a conversation a terminal is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Role {
    Coordinator,
    Worker,
}

/// Terminals that have unread mail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recipient {
    pub term: u64,
    pub role: Role,
    pub unread: u32,
    pub max_unread_id: i64,
    /// When the run / dispatch behind this terminal was created (stale-id detection).
    pub bound_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerInfo {
    pub task_title: String,
    pub dispatch_id: String,
    pub state: DispatchState,
    pub started_ms: i64,
}

/// What the sidebar shows; refreshed by the watcher, never queried from `render()`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OrchSnapshot {
    pub workers: HashMap<u64, WorkerInfo>,
    /// Coordinator terminal -> run creation time.
    pub coordinators: HashMap<u64, i64>,
}

impl OrchSnapshot {
    pub fn worker_title(&self, term: u64) -> Option<&str> {
        self.workers.get(&term).map(|w| w.task_title.as_str())
    }

    pub fn is_coordinator(&self, term: u64) -> bool {
        self.coordinators.contains_key(&term)
    }
}

/// Agent state of a terminal as seen by the orchestration logic.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AgentObs {
    None,
    Working,
    Waiting,
    Done,
    Failed,
    Idle,
}

pub fn term_set(ids: impl IntoIterator<Item = u64>) -> HashSet<u64> {
    ids.into_iter().collect()
}
