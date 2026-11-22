//! Actions on a single commit from the history list: cherry-pick, revert, web link.
use crate::forge::RemoteRepo;
use crate::runner::text;
use crate::{GitError, Repo};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommitOp {
    CherryPick,
    Revert,
}

impl CommitOp {
    fn verb(self) -> &'static str {
        match self {
            Self::CherryPick => "cherry-pick",
            Self::Revert => "revert",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommitOpOutcome {
    /// A new commit was created.
    Landed,
    /// Git stopped on conflicts; the operation is in progress and files are unmerged.
    Conflicted,
    /// The change was already applied; the empty step was skipped.
    Empty,
    /// Nothing happened; the message explains why.
    Refused(String),
}

/// `<web_url>/commit/<oid>` for a parsed GitHub-style remote.
pub fn commit_url(remote: &RemoteRepo, oid: &str) -> String {
    format!("{}/commit/{oid}", remote.web_url())
}

pub(crate) fn friendly_failure(stderr: &str) -> Option<&'static str> {
    stderr
        .contains("would be overwritten")
        .then_some("Commit or stash your changes, then try again")
}

impl Repo {
    pub fn cherry_pick(&self, oid: &str) -> Result<CommitOpOutcome, GitError> {
        self.apply_commit(CommitOp::CherryPick, oid)
    }

    pub fn revert(&self, oid: &str) -> Result<CommitOpOutcome, GitError> {
        self.apply_commit(CommitOp::Revert, oid)
    }

    /// Link to the commit on the push remote's web UI, when that remote is understood.
    pub fn commit_web_url(&self, oid: &str) -> Option<String> {
        let remote = self.remote_repo().ok().flatten()?;
        Some(commit_url(&remote, oid))
    }

    fn apply_commit(&self, op: CommitOp, oid: &str) -> Result<CommitOpOutcome, GitError> {
        let verb = op.verb();
        // Validates the oid as well.
        let parents = self.commit_parents(oid)?;
        if parents.split_whitespace().count() > 2 {
            return Ok(CommitOpOutcome::Refused(format!(
                "Can't {verb} a merge commit from here"
            )));
        }
        if let Some(in_progress) = self.in_progress() {
            return Ok(CommitOpOutcome::Refused(format!(
                "Finish or abort the {} in progress first",
                match in_progress {
                    crate::InProgress::Rebase => "rebase",
                    crate::InProgress::Merge => "merge",
                    crate::InProgress::CherryPick => "cherry-pick",
                    crate::InProgress::Revert => "revert",
                }
            )));
        }
        let editor = [("GIT_EDITOR", "true"), ("GIT_SEQUENCE_EDITOR", "true")];
        let args: &[&str] = match op {
            CommitOp::CherryPick => &["cherry-pick", "--no-edit", oid],
            CommitOp::Revert => &["revert", "--no-edit", oid],
        };
        let error = match self.write_output(args, &editor) {
            Ok(_) => return Ok(CommitOpOutcome::Landed),
            Err(error) => error,
        };
        if self.in_progress().is_some() {
            if self.has_unmerged()? {
                return Ok(CommitOpOutcome::Conflicted);
            }
            // Stopped without conflicts: the step produced no changes.
            self.write_output(&[verb, "--skip"], &editor)?;
            return Ok(CommitOpOutcome::Empty);
        }
        match &error {
            GitError::Failed(message) => Ok(CommitOpOutcome::Refused(
                friendly_failure(message)
                    .map(str::to_owned)
                    .unwrap_or_else(|| message.clone()),
            )),
            _ => Err(error),
        }
    }

    fn has_unmerged(&self) -> Result<bool, GitError> {
        let out = self.read(&["diff", "--name-only", "--diff-filter=U", "-z"])?;
        Ok(!text(&out).is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InProgress;
    use crate::runner::test_repo::TestRepo;

    /// main: base -> m1; side: base -> s1 (s1 touches b.txt only).
    fn fixture() -> (TestRepo, Repo, String) {
        let t = TestRepo::new();
        t.file("a.txt", "one\n");
        t.commit("base");
        t.git(&["checkout", "-q", "-b", "side"]);
        t.file("b.txt", "side\n");
        t.commit("s1");
        let s1 = t.git(&["rev-parse", "HEAD"]).trim().to_owned();
        t.git(&["checkout", "-q", "main"]);
        t.file("a.txt", "one\nmain\n");
        t.commit("m1");
        let repo = t.open();
        (t, repo, s1)
    }

    #[test]
    fn cherry_pick_lands_a_new_commit() {
        let (t, repo, s1) = fixture();
        assert_eq!(repo.cherry_pick(&s1).unwrap(), CommitOpOutcome::Landed);
        assert_eq!(t.git(&["log", "-1", "--format=%s"]).trim(), "s1");
        assert!(t.dir.join("b.txt").is_file());
        assert!(repo.in_progress().is_none());
    }

    #[test]
    fn revert_lands_a_new_commit() {
        let (t, repo, _) = fixture();
        assert_eq!(repo.revert("HEAD").unwrap_err(), GitError::Failed("Invalid commit ID: \"HEAD\"".into()));
        let head = t.git(&["rev-parse", "HEAD"]).trim().to_owned();
        assert_eq!(repo.revert(&head).unwrap(), CommitOpOutcome::Landed);
        assert!(t.git(&["log", "-1", "--format=%s"]).starts_with("Revert"));
        assert_eq!(std::fs::read_to_string(t.dir.join("a.txt")).unwrap(), "one\n");
    }

    #[test]
    fn conflicting_cherry_pick_reports_conflicted_and_stays_in_progress() {
        let t = TestRepo::new();
        t.file("a.txt", "one\n");
        t.commit("base");
        t.git(&["checkout", "-q", "-b", "side"]);
        t.file("a.txt", "side\n");
        t.commit("s1");
        let s1 = t.git(&["rev-parse", "HEAD"]).trim().to_owned();
        t.git(&["checkout", "-q", "main"]);
        t.file("a.txt", "main\n");
        t.commit("m1");
        let repo = t.open();
        assert_eq!(repo.cherry_pick(&s1).unwrap(), CommitOpOutcome::Conflicted);
        assert_eq!(repo.in_progress(), Some(InProgress::CherryPick));
        // A second operation is refused while this one is open.
        let outcome = repo.cherry_pick(&s1).unwrap();
        assert!(matches!(outcome, CommitOpOutcome::Refused(m) if m.contains("in progress")));
    }

    #[test]
    fn conflicting_revert_reports_conflicted() {
        let t = TestRepo::new();
        t.file("a.txt", "one\n");
        t.commit("base");
        t.file("a.txt", "two\n");
        t.commit("change");
        let target = t.git(&["rev-parse", "HEAD"]).trim().to_owned();
        t.file("a.txt", "three\n");
        t.commit("later");
        let repo = t.open();
        assert_eq!(repo.revert(&target).unwrap(), CommitOpOutcome::Conflicted);
        assert_eq!(repo.in_progress(), Some(InProgress::Revert));
    }

    #[test]
    fn already_applied_change_is_empty_and_skipped() {
        let (t, repo, s1) = fixture();
        assert_eq!(repo.cherry_pick(&s1).unwrap(), CommitOpOutcome::Landed);
        let count = t.git(&["rev-list", "--count", "HEAD"]);
        assert_eq!(repo.cherry_pick(&s1).unwrap(), CommitOpOutcome::Empty);
        assert!(repo.in_progress().is_none());
        assert_eq!(t.git(&["rev-list", "--count", "HEAD"]), count);
    }

    #[test]
    fn merge_commits_are_refused() {
        let (t, repo, _) = fixture();
        t.git(&["merge", "-q", "--no-ff", "-m", "merge", "side"]);
        let merge = t.git(&["rev-parse", "HEAD"]).trim().to_owned();
        for outcome in [repo.cherry_pick(&merge).unwrap(), repo.revert(&merge).unwrap()] {
            assert!(matches!(&outcome, CommitOpOutcome::Refused(m) if m.contains("merge commit")), "{outcome:?}");
        }
        assert!(repo.in_progress().is_none());
    }

    #[test]
    fn dirty_tree_is_refused_with_a_friendly_message() {
        let t = TestRepo::new();
        t.file("a.txt", "one\n");
        t.commit("base");
        t.git(&["checkout", "-q", "-b", "side"]);
        t.file("a.txt", "side\n");
        t.commit("s1");
        let s1 = t.git(&["rev-parse", "HEAD"]).trim().to_owned();
        t.git(&["checkout", "-q", "main"]);
        t.file("b.txt", "x\n");
        t.commit("m1");
        t.file("a.txt", "dirty\n");
        let repo = t.open();
        assert_eq!(
            repo.cherry_pick(&s1).unwrap(),
            CommitOpOutcome::Refused("Commit or stash your changes, then try again".into())
        );
        assert!(repo.in_progress().is_none());
        assert_eq!(std::fs::read_to_string(t.dir.join("a.txt")).unwrap(), "dirty\n");
    }

    #[test]
    fn invalid_oids_are_rejected_before_running_git() {
        let (_t, repo, _) = fixture();
        for bad in ["", "abc", "--help", &"g".repeat(40), "HEAD"] {
            assert!(repo.cherry_pick(bad).is_err(), "{bad}");
            assert!(repo.revert(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn friendly_failure_maps_only_overwrite_errors() {
        assert!(friendly_failure("error: your local changes would be overwritten by cherry-pick.").is_some());
        assert!(friendly_failure("fatal: bad object").is_none());
    }

    #[test]
    fn commit_url_uses_the_web_url_of_the_remote() {
        let remote = RemoteRepo::parse("git@github.com:owner/name.git").unwrap();
        assert_eq!(commit_url(&remote, "abc123"), "https://github.com/owner/name/commit/abc123");
    }

    #[test]
    fn commit_web_url_follows_the_push_remote() {
        let (t, repo, s1) = fixture();
        assert_eq!(repo.commit_web_url(&s1), None);
        t.git(&["remote", "add", "origin", "https://github.com/o/r.git"]);
        assert_eq!(repo.commit_web_url(&s1), Some(format!("https://github.com/o/r/commit/{s1}")));
    }
}
