//! Resolving unmerged paths and finishing merge, cherry-pick and revert operations.

use crate::runner::{GitError, Repo, literal_path};
use crate::status::{ConflictKind, parse_status};
use crate::{InProgress, STATUS_LIMIT};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// Stage the file as it is in the working tree.
    MarkResolved,
    /// Keep the surviving version of a file one side deleted.
    KeepFile,
    /// Accept the deletion.
    KeepDeletion,
}

/// The actions that make sense for a conflict of `kind`.
pub fn conflict_resolutions(kind: ConflictKind) -> &'static [Resolution] {
    match kind {
        ConflictKind::DeletedByUs | ConflictKind::DeletedByThem => {
            &[Resolution::KeepFile, Resolution::KeepDeletion]
        }
        ConflictKind::BothDeleted => &[Resolution::KeepDeletion],
        ConflictKind::AddedByUs
        | ConflictKind::AddedByThem
        | ConflictKind::BothAdded
        | ConflictKind::BothModified => &[Resolution::MarkResolved],
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContinueOutcome {
    Continued,
    /// The resolved commit changed nothing, so it was skipped.
    AlreadyApplied,
}

const NO_EDITOR: [(&str, &str); 2] = [("GIT_EDITOR", "true"), ("GIT_SEQUENCE_EDITOR", "true")];

impl Repo {
    /// Re-reads the path's state first: the panel may be showing a stale conflict.
    pub fn resolve_conflict(&self, path: &str, resolution: Resolution) -> Result<(), GitError> {
        let relative = literal_path(path)?;
        let output = self.read(&[
            "status",
            "--porcelain=v2",
            "-z",
            "--untracked-files=no",
            "--",
            path,
        ])?;
        let status = parse_status(&output, STATUS_LIMIT);
        let Some(entry) = status.conflicted.iter().find(|file| file.path == path) else {
            return Err(GitError::Failed(format!(
                "{path} is no longer in conflict. Refresh."
            )));
        };
        let kind = entry.conflict.unwrap_or(ConflictKind::BothModified);
        if !conflict_resolutions(kind).contains(&resolution) {
            return Err(GitError::Failed(format!(
                "{resolution:?} does not apply to {path}"
            )));
        }
        match resolution {
            Resolution::MarkResolved | Resolution::KeepFile => {
                if std::fs::symlink_metadata(self.root().join(relative)).is_err() {
                    return Err(GitError::Failed(format!(
                        "{path} is missing from the working tree"
                    )));
                }
                self.write(&["add", "--", path], None)
            }
            Resolution::KeepDeletion => self.write(&["rm", "--force", "--quiet", "--", path], None),
        }
    }

    /// Commits a merge whose conflicts are all resolved, keeping the prepared message.
    pub fn complete_merge(&self) -> Result<(), GitError> {
        if !self.git_dir.join("MERGE_HEAD").is_file() {
            return Err(GitError::Failed("No merge in progress".into()));
        }
        if !self.read(&["ls-files", "-u", "-z"])?.is_empty() {
            return Err(GitError::Failed("Unresolved conflicts remain".into()));
        }
        self.write_output(&["commit", "--no-edit", "--cleanup=strip"], &NO_EDITOR)
            .map(|_| ())
    }

    /// `--continue`; for a cherry-pick or revert whose resolution left nothing to commit, skips
    /// the now-empty step instead.
    pub fn continue_or_skip(&self, op: InProgress) -> Result<ContinueOutcome, GitError> {
        let error = match self.continue_in_progress(op) {
            Ok(()) => return Ok(ContinueOutcome::Continued),
            Err(error) => error,
        };
        if !matches!(op, InProgress::CherryPick | InProgress::Revert) {
            return Err(error);
        }
        let matches_head = self.read(&["diff", "--cached", "--quiet", "--exit-code"]).is_ok();
        let unmerged = self.read(&["ls-files", "-u", "-z"]).map_or(true, |out| !out.is_empty());
        if !matches_head || unmerged {
            return Err(error);
        }
        let verb = if op == InProgress::CherryPick {
            "cherry-pick"
        } else {
            "revert"
        };
        self.write_output(&[verb, "--skip"], &NO_EDITOR)?;
        Ok(ContinueOutcome::AlreadyApplied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::test_repo::TestRepo;
    use crate::ChangeKind;

    fn conflict(repo: &Repo, path: &str) -> Option<ConflictKind> {
        let status = repo.status().unwrap();
        status
            .conflicted
            .iter()
            .find(|file| file.path == path)
            .map(|file| file.conflict.expect("kind"))
    }

    fn failure(result: Result<(), GitError>) -> String {
        match result {
            Err(GitError::Failed(message)) => message,
            other => panic!("expected failure, got {other:?}"),
        }
    }

    /// `main` and `other` both change `f.txt` from the same base.
    fn modified_modified() -> TestRepo {
        let fixture = TestRepo::new();
        fixture.file("f.txt", "one\ntwo\nthree\n");
        fixture.commit("base");
        fixture.git(&["switch", "-q", "-c", "other"]);
        fixture.file("f.txt", "one\nOTHER\nthree\n");
        fixture.commit("other");
        fixture.git(&["switch", "-q", "main"]);
        fixture.file("f.txt", "one\nMAIN\nthree\n");
        fixture.commit("main");
        fixture
    }

    #[test]
    fn resolutions_follow_the_conflict_kind() {
        use ConflictKind::*;
        use Resolution::*;
        assert_eq!(conflict_resolutions(DeletedByUs), [KeepFile, KeepDeletion]);
        assert_eq!(conflict_resolutions(DeletedByThem), [KeepFile, KeepDeletion]);
        assert_eq!(conflict_resolutions(BothDeleted), [KeepDeletion]);
        for kind in [AddedByUs, AddedByThem, BothAdded, BothModified] {
            assert_eq!(conflict_resolutions(kind), [MarkResolved]);
        }
    }

    #[test]
    fn both_modified_is_marked_resolved_after_editing() {
        let fixture = modified_modified();
        assert!(fixture.try_git(&["merge", "other"]).is_err());
        let repo = fixture.open();
        assert_eq!(conflict(&repo, "f.txt"), Some(ConflictKind::BothModified));
        fixture.file("f.txt", "one\nBOTH\nthree\n");
        repo.resolve_conflict("f.txt", Resolution::MarkResolved).unwrap();
        assert!(repo.status().unwrap().conflicted.is_empty());
        // The path is not in conflict any more: a second click is refused.
        let message = failure(repo.resolve_conflict("f.txt", Resolution::MarkResolved));
        assert_eq!(message, "f.txt is no longer in conflict. Refresh.");
    }

    #[test]
    fn missing_worktree_file_and_wrong_resolution_are_refused() {
        let fixture = modified_modified();
        let _ = fixture.try_git(&["merge", "other"]);
        let repo = fixture.open();
        let wrong = failure(repo.resolve_conflict("f.txt", Resolution::KeepDeletion));
        assert!(wrong.contains("does not apply"), "{wrong}");
        std::fs::remove_file(fixture.dir.join("f.txt")).unwrap();
        let missing = failure(repo.resolve_conflict("f.txt", Resolution::MarkResolved));
        assert!(missing.contains("missing"), "{missing}");
        assert_eq!(conflict(&repo, "f.txt"), Some(ConflictKind::BothModified));
        assert!(failure(repo.resolve_conflict("../x", Resolution::MarkResolved)).contains("Invalid"));
        let other = failure(repo.resolve_conflict("nothing.txt", Resolution::MarkResolved));
        assert!(other.contains("no longer in conflict"), "{other}");
    }

    #[test]
    fn both_added_conflict_is_detected() {
        let fixture = TestRepo::new();
        fixture.file("seed.txt", "seed\n");
        fixture.commit("base");
        fixture.git(&["switch", "-q", "-c", "other"]);
        fixture.file("n.txt", "theirs\n");
        fixture.commit("other adds");
        fixture.git(&["switch", "-q", "main"]);
        fixture.file("n.txt", "ours\n");
        fixture.commit("main adds");
        let _ = fixture.try_git(&["merge", "other"]);
        let repo = fixture.open();
        assert_eq!(conflict(&repo, "n.txt"), Some(ConflictKind::BothAdded));
        fixture.file("n.txt", "merged\n");
        repo.resolve_conflict("n.txt", Resolution::MarkResolved).unwrap();
        assert!(repo.status().unwrap().conflicted.is_empty());
    }

    /// One side deletes `f.txt`, the other edits it.
    fn delete_modify(delete_on_main: bool) -> TestRepo {
        let fixture = TestRepo::new();
        fixture.file("f.txt", "one\ntwo\nthree\n");
        fixture.commit("base");
        let (deleter, editor) = if delete_on_main {
            ("main", "other")
        } else {
            ("other", "main")
        };
        fixture.git(&["switch", "-q", "-c", "other"]);
        fixture.git(&["switch", "-q", editor]);
        if editor == "other" {
            fixture.file("f.txt", "one\nedited\nthree\n");
            fixture.commit("edit");
            fixture.git(&["switch", "-q", "main"]);
            fixture.git(&["rm", "-q", "f.txt"]);
            fixture.commit("delete");
        } else {
            fixture.file("f.txt", "one\nedited\nthree\n");
            fixture.commit("edit");
            fixture.git(&["switch", "-q", "other"]);
            fixture.git(&["rm", "-q", "f.txt"]);
            fixture.commit("delete");
            fixture.git(&["switch", "-q", "main"]);
        }
        let _ = deleter;
        let _ = fixture.try_git(&["merge", "other"]);
        fixture
    }

    #[test]
    fn deleted_by_us_can_keep_the_file_or_the_deletion() {
        let keep = delete_modify(true);
        let repo = keep.open();
        assert_eq!(conflict(&repo, "f.txt"), Some(ConflictKind::DeletedByUs));
        repo.resolve_conflict("f.txt", Resolution::KeepFile).unwrap();
        assert!(repo.status().unwrap().conflicted.is_empty());
        assert!(keep.dir.join("f.txt").is_file());

        let delete = delete_modify(true);
        let repo = delete.open();
        repo.resolve_conflict("f.txt", Resolution::KeepDeletion).unwrap();
        assert!(repo.status().unwrap().conflicted.is_empty());
        assert!(!delete.dir.join("f.txt").exists());
    }

    #[test]
    fn deleted_by_them_can_keep_the_file_or_the_deletion() {
        let keep = delete_modify(false);
        let repo = keep.open();
        assert_eq!(conflict(&repo, "f.txt"), Some(ConflictKind::DeletedByThem));
        repo.resolve_conflict("f.txt", Resolution::KeepFile).unwrap();
        assert!(repo.status().unwrap().conflicted.is_empty());

        let delete = delete_modify(false);
        let repo = delete.open();
        repo.resolve_conflict("f.txt", Resolution::KeepDeletion).unwrap();
        assert!(repo.status().unwrap().conflicted.is_empty());
        assert!(!delete.dir.join("f.txt").exists());
    }

    #[test]
    fn rename_rename_yields_both_deleted_and_added_by_each_side() {
        let fixture = TestRepo::new();
        let body = "line one\nline two\nline three\nline four\nline five\nline six\n";
        fixture.file("f.txt", body);
        fixture.commit("base");
        fixture.git(&["switch", "-q", "-c", "other"]);
        fixture.git(&["mv", "f.txt", "theirs.txt"]);
        fixture.commit("other renames");
        fixture.git(&["switch", "-q", "main"]);
        fixture.git(&["mv", "f.txt", "ours.txt"]);
        fixture.commit("main renames");
        let _ = fixture.try_git(&["merge", "other"]);
        let repo = fixture.open();
        assert_eq!(conflict(&repo, "f.txt"), Some(ConflictKind::BothDeleted));
        assert_eq!(conflict(&repo, "ours.txt"), Some(ConflictKind::AddedByUs));
        assert_eq!(conflict(&repo, "theirs.txt"), Some(ConflictKind::AddedByThem));
        repo.resolve_conflict("f.txt", Resolution::KeepDeletion).unwrap();
        repo.resolve_conflict("ours.txt", Resolution::MarkResolved).unwrap();
        repo.resolve_conflict("theirs.txt", Resolution::MarkResolved).unwrap();
        assert!(repo.status().unwrap().conflicted.is_empty());
    }

    #[test]
    fn complete_merge_commits_only_when_resolved() {
        let fixture = modified_modified();
        let repo = fixture.open();
        assert_eq!(failure(repo.complete_merge()), "No merge in progress");
        let _ = fixture.try_git(&["merge", "other"]);
        assert_eq!(failure(repo.complete_merge()), "Unresolved conflicts remain");
        assert_eq!(repo.in_progress(), Some(InProgress::Merge));
        fixture.file("f.txt", "one\nBOTH\nthree\n");
        repo.resolve_conflict("f.txt", Resolution::MarkResolved).unwrap();
        repo.complete_merge().unwrap();
        assert_eq!(repo.in_progress(), None);
        let parents = fixture.git(&["rev-list", "--parents", "-n1", "HEAD"]);
        assert_eq!(parents.split_whitespace().count(), 3, "{parents}");
        let message = repo.last_commit_message().unwrap().unwrap();
        assert!(message.starts_with("Merge branch 'other'"), "{message}");
        assert!(!message.contains('#'), "{message}");
    }

    #[test]
    fn cherry_pick_continues_or_skips_an_empty_resolution() {
        let fixture = modified_modified();
        let _ = fixture.try_git(&["cherry-pick", "other"]);
        let repo = fixture.open();
        assert_eq!(repo.in_progress(), Some(InProgress::CherryPick));
        fixture.file("f.txt", "one\nBOTH\nthree\n");
        repo.resolve_conflict("f.txt", Resolution::MarkResolved).unwrap();
        assert_eq!(
            repo.continue_or_skip(InProgress::CherryPick).unwrap(),
            ContinueOutcome::Continued
        );
        assert_eq!(repo.in_progress(), None);

        // Resolving to exactly HEAD leaves an empty commit: it is skipped.
        let fixture = modified_modified();
        let _ = fixture.try_git(&["cherry-pick", "other"]);
        let repo = fixture.open();
        fixture.file("f.txt", "one\nMAIN\nthree\n");
        repo.resolve_conflict("f.txt", Resolution::MarkResolved).unwrap();
        let before = fixture.git(&["rev-parse", "HEAD"]);
        assert_eq!(
            repo.continue_or_skip(InProgress::CherryPick).unwrap(),
            ContinueOutcome::AlreadyApplied
        );
        assert_eq!(repo.in_progress(), None);
        assert_eq!(fixture.git(&["rev-parse", "HEAD"]), before);
    }

    #[test]
    fn continue_with_unresolved_conflicts_keeps_the_error() {
        let fixture = modified_modified();
        let _ = fixture.try_git(&["cherry-pick", "other"]);
        let repo = fixture.open();
        assert!(repo.continue_or_skip(InProgress::CherryPick).is_err());
        assert_eq!(repo.in_progress(), Some(InProgress::CherryPick));
        assert_eq!(
            repo.status().unwrap().conflicted[0].status,
            ChangeKind::Conflicted
        );
    }

    #[test]
    fn revert_conflict_can_be_aborted_and_continued() {
        let fixture = TestRepo::new();
        fixture.file("f.txt", "one\n");
        fixture.commit("base");
        fixture.file("f.txt", "two\n");
        fixture.commit("change");
        let target = fixture.git(&["rev-parse", "HEAD"]);
        fixture.file("f.txt", "three\n");
        fixture.commit("change again");
        let _ = fixture.try_git(&["revert", "--no-edit", target.trim()]);
        let repo = fixture.open();
        assert_eq!(repo.in_progress(), Some(InProgress::Revert));
        fixture.file("f.txt", "three\n");
        repo.resolve_conflict("f.txt", Resolution::MarkResolved).unwrap();
        assert_eq!(
            repo.continue_or_skip(InProgress::Revert).unwrap(),
            ContinueOutcome::AlreadyApplied
        );
        assert_eq!(repo.in_progress(), None);
    }
}
