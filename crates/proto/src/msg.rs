use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const PROTO_VERSION: u32 = 1;

/// Maximum payload of a single `ServerMsg::Output` frame.
pub const MAX_OUTPUT_CHUNK: usize = 64 * 1024;

pub type TermId = u64;

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
    },
    Subscribe { id: TermId, resume_from_seq: Option<u64> },
    Unsubscribe { id: TermId },
    Input { id: TermId, bytes: Vec<u8> },
    Resize { id: TermId, cols: u16, rows: u16 },
    KillTerminal { id: TermId },
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
}
