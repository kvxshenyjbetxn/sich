//! Wire types and frame codec shared by the daemon and all clients.

mod codec;
mod msg;

pub use codec::{CodecError, MAX_FRAME_LEN, decode, encode, frame_codec};
pub use msg::{
    AgentKind, AgentState, AgentStatus, ClientMsg, GroupId, GroupInfo, MAX_OUTPUT_CHUNK, PROTO_VERSION, ProjectId, ProjectInfo, ServerMsg,
    SubagentActivity, SubagentActivityEntry, SubagentActivityKey, SubagentActivityKind, SubagentModel, SubagentStatus, TermId, TerminalInfo, WorktreeInfo,
};

/// Suffix in `Hello.client_name` for clients that can decode `AgentKind::Omp`.
pub const OMP_CLIENT_SUFFIX: &str = "+omp";

/// Suffix in `Hello.client_name` for clients that can decode the child-model messages.
/// Appears before `OMP_CLIENT_SUFFIX` when both capabilities are requested.
pub const SUBAGENT_MODELS_CLIENT_SUFFIX: &str = "+subagent-models";

/// Dedicated read-only client that never receives terminal broadcasts.
pub const SUBAGENT_ACTIVITY_CLIENT: &str = "sich-app-subagent-activity";

#[cfg(test)]
mod tests;
