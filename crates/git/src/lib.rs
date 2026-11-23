//! Blocking Git operations and worktree observation, independent of the UI.
mod branch_compare;
mod branches;
mod commit_ops;
pub mod compare;
mod conflicts_ops;
pub mod forge;
mod diff;
pub mod graph;
mod history;
mod mutation;
pub mod pr_checkout;
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
pub use branch_compare::{BranchCompare, CompareStatus, choose_base};
pub use conflicts_ops::{ContinueOutcome, Resolution, conflict_resolutions};
pub use sequencer::InProgress;
pub use stash::Stash;
pub use status::{ChangeKind, ConflictKind, FileChange, RepoStatus, STATUS_LIMIT};

#[cfg(test)]
mod ops_tests;
#[cfg(test)]
mod repo_tests;
