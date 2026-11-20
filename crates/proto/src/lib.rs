//! Wire types and frame codec shared by the daemon and all clients.

mod codec;
mod msg;

pub use codec::{CodecError, MAX_FRAME_LEN, decode, encode, frame_codec};
pub use msg::{
    AgentKind, AgentState, AgentStatus, ClientMsg, GroupId, GroupInfo, MAX_OUTPUT_CHUNK, PROTO_VERSION, ProjectId, ProjectInfo, ServerMsg, SubagentStatus, TermId,
    SubagentActivity, SubagentActivityEntry, SubagentActivityKey, SubagentActivityKind, TerminalInfo, WorktreeInfo,
};

/// Suffix in `Hello.client_name` for clients that can decode `AgentKind::Omp`.
pub const OMP_CLIENT_SUFFIX: &str = "+omp";

/// Dedicated read-only client that never receives terminal broadcasts.
pub const SUBAGENT_ACTIVITY_CLIENT: &str = "sich-app-subagent-activity";

#[cfg(test)]
mod tests;
