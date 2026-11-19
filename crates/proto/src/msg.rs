use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const PROTO_VERSION: u32 = 2;

/// Maximum payload of a single `ServerMsg::Output` frame.
pub const MAX_OUTPUT_CHUNK: usize = 64 * 1024;

pub type TermId = u64;
pub type ProjectId = u64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMsg {
    Hello { proto_version: u32, client_name: String },
    ListTerminals,
    /// `shell` is the program to run (default shell when `None`), `args` its arguments.
    CreateTerminal {
        cwd: Option<PathBuf>,
        shell: Option<String>,
        args: Vec<String>,
        cols: u16,
        rows: u16,
        /// Worktree this terminal belongs to; used as cwd when `cwd` is `None`.
        worktree: Option<PathBuf>,
    },
    Subscribe { id: TermId, resume_from_seq: Option<u64> },
    Unsubscribe { id: TermId },
    Input { id: TermId, bytes: Vec<u8> },
    Resize { id: TermId, cols: u16, rows: u16 },
    KillTerminal { id: TermId },
    ListProjects,
    RefreshProjects,
    AddProject { path: PathBuf },
    RemoveProject { id: ProjectId },
    CreateWorktree { project_id: ProjectId, name: String, base: Option<String> },
    RemoveWorktree { project_id: ProjectId, path: PathBuf, force: bool, delete_branch: bool },
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerMsg {
    Welcome { proto_version: u32, daemon_pid: u32 },
    /// Reply to `ListTerminals`.
    Terminals(Vec<TerminalInfo>),
    /// Broadcast to all clients.
    TerminalCreated(TerminalInfo),
    /// Broadcast to all clients.
    TerminalExited { id: TermId, code: Option<i32> },
    /// Broadcast to all clients (after a kill, or when an exited terminal is removed).
    TerminalRemoved { id: TermId },
    /// `seq` is the byte offset of the first byte of the chunk.
    Output { id: TermId, seq: u64, bytes: Vec<u8> },
    /// Reply to `ListProjects`; also broadcast to all clients after any project change/refresh.
    Projects(Vec<ProjectInfo>),
    /// User-facing failure of an operation; sent only to the requesting connection.
    OpFailed { op: String, message: String },
    Error { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalInfo {
    pub id: TermId,
    pub title: String,
    pub cwd: PathBuf,
    pub cols: u16,
    pub rows: u16,
    /// `Some(code)` once the process has exited (`code` itself may be unknown).
    pub exited: Option<Option<i32>>,
    pub created_unix_ms: u64,
    pub worktree: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectInfo {
    pub id: ProjectId,
    pub name: String,
    pub path: PathBuf,
    pub is_git: bool,
    pub worktrees: Vec<WorktreeInfo>,
    /// E.g. "Folder not found".
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    /// `None` = detached HEAD (or not a git project).
    pub branch: Option<String>,
    /// Short sha; empty for non-git projects.
    pub head: String,
    pub is_main: bool,
    pub dirty: bool,
}
