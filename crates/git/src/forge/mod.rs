//! GitHub pull request support through the user's installed official `gh` CLI.
//!
//! Sich never reads gh credentials or tokens: every call is a plain `gh` invocation.
mod gh;
mod github;
mod model;
mod prefill;
mod remote;
mod repo_ext;

use std::fmt;

pub use gh::{GhCli, GhExec, GhOutput};
pub use github::{CreatePr, CreatedPr, GitHub, MergeMethod};
pub use model::{
    Check, CheckState, ChecksSummary, MergeState, Mergeable, PrState, PullRequest, ReviewDecision,
    parse_pr_json,
};
pub use prefill::{PrDraft, pr_prefill};
pub use remote::RemoteRepo;
pub use repo_ext::CommitMessage;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ForgeError {
    GhMissing,
    NotAuthenticated,
    Unsupported,
    NoBranch,
    TimedOut,
    Failed(String),
}

impl fmt::Display for ForgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::GhMissing => "GitHub CLI (gh) is not installed",
            Self::NotAuthenticated => "Not logged in to GitHub (run `gh auth login`)",
            Self::Unsupported => "No GitHub remote",
            Self::NoBranch => "No branch checked out",
            Self::TimedOut => "GitHub CLI timed out",
            Self::Failed(message) => message,
        })
    }
}

impl std::error::Error for ForgeError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_display_in_english() {
        assert!(ForgeError::GhMissing.to_string().contains("gh"));
        assert!(ForgeError::NotAuthenticated.to_string().contains("gh auth login"));
        assert_eq!(ForgeError::Failed("boom".into()).to_string(), "boom");
        assert!(!ForgeError::Unsupported.to_string().is_empty());
        assert!(!ForgeError::NoBranch.to_string().is_empty());
        assert!(!ForgeError::TimedOut.to_string().is_empty());
    }
}
