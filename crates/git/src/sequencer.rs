use crate::GitError;
use crate::Repo;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InProgress {
    Rebase,
    Merge,
    CherryPick,
    Revert,
}

impl InProgress {
    fn verb(self) -> &'static str {
        match self {
            Self::Rebase => "rebase",
            Self::Merge => "merge",
            Self::CherryPick => "cherry-pick",
            Self::Revert => "revert",
        }
    }
}

impl Repo {
    /// Detects an unfinished operation from the per-worktree Git directory (no Git process).
    pub fn in_progress(&self) -> Option<InProgress> {
        let dir = &self.git_dir;
        if dir.join("rebase-merge").is_dir() || dir.join("rebase-apply").is_dir() {
            Some(InProgress::Rebase)
        } else if dir.join("MERGE_HEAD").is_file() {
            Some(InProgress::Merge)
        } else if dir.join("CHERRY_PICK_HEAD").is_file() {
            Some(InProgress::CherryPick)
        } else if dir.join("REVERT_HEAD").is_file() {
            Some(InProgress::Revert)
        } else {
            None
        }
    }

    pub fn abort_in_progress(&self, op: InProgress) -> Result<(), GitError> {
        self.write_output(&[op.verb(), "--abort"], &[]).map(|_| ())
    }

    /// Never opens an editor: the editor variables point at `true`.
    pub fn continue_in_progress(&self, op: InProgress) -> Result<(), GitError> {
        self.write_output(
            &[op.verb(), "--continue"],
            &[("GIT_EDITOR", "true"), ("GIT_SEQUENCE_EDITOR", "true")],
        )
        .map(|_| ())
    }
}
