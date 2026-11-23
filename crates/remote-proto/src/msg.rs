use serde::{Deserialize, Serialize};

/// Phone -> gateway. Variant order is wire format: append only, `Hello` stays 0.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PhoneMsg {
    /// First message; `pair_secret` only when pairing from a QR.
    Hello {
        proto_version: u32,
        min_compatible: u32,
        app_version: String,
        device_name: String,
        capabilities: Vec<String>,
        pair_secret: Option<String>,
    },
    /// App-level liveness (every 5 s).
    Ping { nonce: u64 },
    /// Start streaming; `fit` = resize PTY to phone.
    Subscribe { term: TermRef, cols: u16, rows: u16, fit: bool, replay_bytes: u32 },
    /// Stop streaming a terminal.
    Unsubscribe { term: TermRef },
    /// Raw key bytes (keys row, direct typing).
    Input { term: TermRef, bytes: Vec<u8> },
    /// Composer message; `images` = upload ids.
    SendText { term: TermRef, text: String, images: Vec<String>, submit: bool },
    /// Phone viewport changed (applied only while fit).
    Resize { term: TermRef, cols: u16, rows: u16 },
    /// Toggle fit-to-phone / desktop size.
    SetFit { term: TermRef, fit: bool },
    /// Clear unread (Done -> Idle).
    MarkSeen { term: TermRef },
    /// Phone downsizes to <= 2048 px before sending.
    UploadImage { req_id: u64, file_name: String, data: Vec<u8> },
}

/// Gateway -> phone. Variant order is wire format: append only, `Welcome` stays 0.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum GatewayMsg {
    /// Reply to an accepted `Hello`.
    Welcome {
        proto_version: u32,
        min_compatible: u32,
        host_name: String,
        gateway_version: String,
        capabilities: Vec<String>,
    },
    /// The gateway closes the connection after this.
    Rejected { reason: RejectReason },
    /// Valid secret, waiting for the desktop user to confirm.
    PairPending,
    /// Reply to `Ping`.
    Pong { nonce: u64 },
    /// Full snapshot; after Welcome and on every change (debounced 150 ms).
    Workspace(Workspace),
    /// Phone clears that terminal; replay follows.
    Reset { term: TermRef, cols: u16, rows: u16 },
    /// `seq` = daemon byte offset of the chunk start.
    Output { term: TermRef, seq: u64, bytes: Vec<u8> },
    /// PTY size / fit state changed.
    TermSize { term: TermRef, cols: u16, rows: u16, fit: bool },
    /// Ok(upload id) or Err(message).
    Uploaded { req_id: u64, result: Result<String, String> },
    /// Terminal process exited.
    Exited { term: TermRef, code: Option<i32> },
    /// Non-fatal error.
    Error { message: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RejectReason {
    /// Device is not paired.
    Unpaired,
    /// Pair secret unknown, expired or used up.
    PairSecretInvalid,
    /// Desktop user denied the pairing.
    PairDenied,
    /// Desktop user did not confirm in time.
    PairTimeout,
    /// Device was revoked.
    Revoked,
    /// Protocol versions do not overlap.
    Incompatible { gateway_version: u32, gateway_min: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TermRef {
    /// Daemon socket name.
    pub daemon: String,
    pub id: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Workspace {
    pub host_name: String,
    pub groups: Vec<Group>,
    pub projects: Vec<Project>,
    pub terminals: Vec<Terminal>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Group {
    pub id: u64,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub id: u64,
    pub name: String,
    pub path: String,
    pub is_git: bool,
    pub group: Option<u64>,
    pub worktrees: Vec<Worktree>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Worktree {
    pub path: String,
    pub branch: Option<String>,
    pub is_main: bool,
    pub dirty: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Terminal {
    pub term: TermRef,
    pub title: String,
    pub cwd: String,
    pub worktree: Option<String>,
    pub cols: u16,
    pub rows: u16,
    /// `Some(code)` once exited; the inner option is the exit code if known.
    pub exited: Option<Option<i32>>,
    pub created_unix_ms: u64,
    pub agent: Option<Agent>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Agent {
    pub kind: AgentKind,
    pub state: AgentState,
    pub unread: bool,
    pub since_unix_ms: u64,
    pub title: Option<String>,
    pub detail: Option<String>,
    pub model: Option<String>,
    pub prompt: Option<String>,
    pub message: Option<String>,
    pub subagents_working: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AgentKind {
    Claude,
    Gemini,
    Codex,
    Omp,
    Other,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AgentState {
    Working,
    Waiting,
    Done,
    Failed,
    Idle,
}
