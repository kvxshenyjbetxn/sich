//! Blocking Git operations and worktree observation, independent of the UI.
mod branches;
pub mod compare;
mod diff;
mod history;
mod mutation;
mod runner;
pub mod scheduler;
mod status;
pub mod watch;

pub use branches::Branch;
pub use diff::{Diff, DiffLine, DiffLineKind, DiffSource};
pub use history::{Commit, HISTORY_PAGE_SIZE};
pub use runner::{GitError, Repo};
pub use status::{ChangeKind, FileChange, RepoStatus, STATUS_LIMIT};

#[cfg(test)]
mod repo_tests;
