//! Blocking Git operations and worktree observation, independent of the UI.
mod branches;
mod commit_ops;
pub mod compare;
pub mod forge;
mod diff;
pub mod graph;
mod history;
mod mutation;
mod runner;
pub mod scheduler;
mod sequencer;
mod stash;
mod status;
pub mod watch;

pub use branches::Branch;
pub use commit_ops::{CommitOpOutcome, commit_url};
pub use diff::{Diff, DiffLine, DiffLineKind, DiffSource};
pub use history::{Commit, HISTORY_PAGE_SIZE};
pub use runner::{GitError, Repo};
pub use sequencer::InProgress;
pub use stash::Stash;
pub use status::{ChangeKind, FileChange, RepoStatus, STATUS_LIMIT};

#[cfg(test)]
mod ops_tests;
#[cfg(test)]
mod repo_tests;
