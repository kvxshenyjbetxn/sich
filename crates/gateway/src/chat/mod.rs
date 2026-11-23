//! Phone chat view: builds a read-only chat from an agent's local transcript file (Claude Code
//! JSONL, Codex rollout JSONL). See `docs/phone-chat.md`.
//!
//! Only official, local, read-only data is used: the transcript the agent itself writes on the
//! user's disk. The formats are undocumented, so every decoder is fail-soft: unknown or malformed
//! lines are skipped and nothing here panics on bad input.
//!
//! Layout:
//! - `locate`: finds the transcript file of a terminal's agent session;
//! - `decode`: one JSONL line -> `ChatMessage`s (pure);
//! - `tail`: incremental reader of an append-only file;
//! - `state`: decoded messages of one file, paging, change detection;
//! - `image`: image refs -> bytes, downsized for the phone;
//! - `session`: the async watcher for one (phone, terminal).

pub mod decode;
pub mod image;
pub mod locate;
pub mod session;
pub mod state;
pub mod tail;

use std::path::PathBuf;

pub use session::ChatHandle;

/// Which transcript format a file uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Claude,
    Codex,
}

/// What a terminal's chat currently maps to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// The transcript exists.
    Ready { path: PathBuf, format: Format },
    /// The agent has no session id or file yet.
    Pending,
    /// This terminal has no supported agent.
    Unavailable(String),
}

/// Re-evaluates the target of a terminal (the agent's session id or file may appear later).
pub type Resolver = std::sync::Arc<dyn Fn() -> Target + Send + Sync>;
