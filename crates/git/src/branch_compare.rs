//! Compares HEAD with a base ref: ahead/behind, the files changed since the merge base and the
//! line totals including the working tree.

use crate::history::parse_name_status;
use crate::runner::{GitError, Repo, text};
use crate::status::{
    ChangeKind, STATUS_LIMIT, merge_counts, parse_numstat, parse_status, untracked_text,
};
use crate::FileChange;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompareStatus {
    Ready,
    /// The base ref is empty, option-like or does not name a commit.
    InvalidBase,
    /// HEAD has no commit yet.
    UnbornHead,
    /// The base and HEAD share no history.
    NoMergeBase,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BranchCompare {
    pub base_ref: String,
    pub base_oid: Option<String>,
    pub head_oid: Option<String>,
    pub merge_base: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    /// Files changed between the merge base and HEAD (committed changes only).
    pub files: Vec<FileChange>,
    /// Added and removed lines from the merge base to the working tree (untracked text included).
    pub totals: (u32, u32),
    pub status: CompareStatus,
}

impl BranchCompare {
    fn empty(base_ref: &str, status: CompareStatus) -> Self {
        Self {
            base_ref: base_ref.to_owned(),
            base_oid: None,
            head_oid: None,
            merge_base: None,
            ahead: 0,
            behind: 0,
            files: Vec::new(),
            totals: (0, 0),
            status,
        }
    }

    /// Whether there is anything to show: commits either way or changed lines.
    pub fn is_empty(&self) -> bool {
        self.ahead == 0 && self.behind == 0 && self.files.is_empty() && self.totals == (0, 0)
    }
}

/// Picks the base ref: the pinned one, else the pull request's base, else the repository default.
/// Empty and option-like values are ignored.
pub fn choose_base(
    pinned: Option<&str>,
    pr_base: Option<&str>,
    default: Option<&str>,
) -> Option<String> {
    [pinned, pr_base, default]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|base| usable_ref(base))
        .map(str::to_owned)
}

fn usable_ref(name: &str) -> bool {
    !name.is_empty() && !name.starts_with('-') && !name.contains(['\0', '\n'])
}

fn is_oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `rev-list --left-right --count A...B` prints `<only in A>\t<only in B>`.
pub(crate) fn parse_left_right(output: &str) -> Option<(u32, u32)> {
    let mut counts = output.split_whitespace().map(str::parse::<u32>);
    match (counts.next(), counts.next(), counts.next()) {
        (Some(Ok(left)), Some(Ok(right)), None) => Some((left, right)),
        _ => None,
    }
}

fn sum_numstat(bytes: &[u8]) -> (u32, u32) {
    parse_numstat(bytes)
        .values()
        .fold((0, 0), |(added, removed), (a, r)| {
            (added + a.unwrap_or(0), removed + r.unwrap_or(0))
        })
}

impl Repo {
    /// The commit `rev` names, or `None` when it names nothing.
    fn verified_commit(&self, rev: &str) -> Result<Option<String>, GitError> {
        let spec = format!("{rev}^{{commit}}");
        match self.read(&["rev-parse", "--verify", "--quiet", "--end-of-options", &spec]) {
            Ok(bytes) => Ok(Some(text(&bytes).trim().to_owned()).filter(|oid| is_oid(oid))),
            Err(GitError::Failed(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub fn branch_compare(&self, base_ref: &str) -> Result<BranchCompare, GitError> {
        let mut result = BranchCompare::empty(base_ref, CompareStatus::Ready);
        let Some(head) = self.verified_commit("HEAD")? else {
            result.status = CompareStatus::UnbornHead;
            return Ok(result);
        };
        result.head_oid = Some(head.clone());
        if !usable_ref(base_ref) {
            result.status = CompareStatus::InvalidBase;
            return Ok(result);
        }
        let Some(base) = self.verified_commit(base_ref)? else {
            result.status = CompareStatus::InvalidBase;
            return Ok(result);
        };
        result.base_oid = Some(base.clone());
        let merge_base = match self.read(&["merge-base", &base, &head]) {
            Ok(bytes) => Some(text(&bytes).trim().to_owned()).filter(|oid| is_oid(oid)),
            Err(GitError::Failed(_)) => None,
            Err(error) => return Err(error),
        };
        let Some(merge_base) = merge_base else {
            result.status = CompareStatus::NoMergeBase;
            return Ok(result);
        };
        result.merge_base = Some(merge_base.clone());
        let range = format!("{base}...{head}");
        let counts = text(&self.read(&["rev-list", "--left-right", "--count", &range, "--"])?);
        let (behind, ahead) = parse_left_right(&counts)
            .ok_or_else(|| GitError::Failed(format!("Unexpected rev-list output: {counts:?}")))?;
        (result.behind, result.ahead) = (behind, ahead);
        if merge_base != head {
            result.files = self.range_files(&merge_base, &head)?;
        }
        result.totals = self.worktree_totals(&merge_base)?;
        Ok(result)
    }

    fn range_files(&self, merge_base: &str, head: &str) -> Result<Vec<FileChange>, GitError> {
        let diff = |format: &'static str| {
            self.read(&[
                "diff",
                "-z",
                format,
                "-M",
                "-C",
                "--no-ext-diff",
                "--no-textconv",
                "--no-color",
                merge_base,
                head,
                "--",
            ])
        };
        let mut files = parse_name_status(&diff("--name-status")?);
        merge_counts(&mut files, parse_numstat(&diff("--numstat")?));
        Ok(files)
    }

    /// Lines added and removed between `merge_base` and the working tree, plus untracked text files.
    fn worktree_totals(&self, merge_base: &str) -> Result<(u32, u32), GitError> {
        if !is_oid(merge_base) {
            return Err(GitError::Failed(format!("Invalid commit ID: {merge_base:?}")));
        }
        let tracked = self.read(&[
            "diff",
            "-z",
            "--numstat",
            "-M",
            "--no-ext-diff",
            "--no-textconv",
            merge_base,
            "--",
        ])?;
        let (mut added, removed) = sum_numstat(&tracked);
        let status = parse_status(&self.status_output()?, STATUS_LIMIT);
        for entry in status
            .unstaged
            .iter()
            .filter(|entry| entry.status == ChangeKind::Untracked)
        {
            if let Some(bytes) = untracked_text(self.root(), &entry.path) {
                added += bytes.split_inclusive(|&b| b == b'\n').count() as u32;
            }
        }
        Ok((added, removed))
    }

    /// `<remote>/<branch>` for the repository's default branch (or the local one without a
    /// usable remote), as a ref `branch_compare` can use.
    pub fn default_compare_base(&self) -> Result<Option<String>, GitError> {
        if let Some(remote) = self.push_remote()? {
            if let Some(name) = self.default_base(&remote)? {
                if self.ref_exists(&format!("refs/remotes/{remote}/{name}"))? {
                    return Ok(Some(format!("{remote}/{name}")));
                }
                if self.ref_exists(&format!("refs/heads/{name}"))? {
                    return Ok(Some(name));
                }
            }
        }
        for name in ["main", "master"] {
            if self.ref_exists(&format!("refs/heads/{name}"))? {
                return Ok(Some(name.to_owned()));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::test_repo::TestRepo;

    fn feature_repo() -> TestRepo {
        let fixture = TestRepo::new();
        fixture.file("keep.txt", "a\nb\nc\n");
        fixture.file("old.txt", "one\ntwo\nthree\nfour\n");
        fixture.file("gone.txt", "x\n");
        fixture.commit("base");
        fixture.git(&["switch", "-q", "-c", "feature"]);
        fixture.file("keep.txt", "a\nB\nc\nd\n");
        fixture.git(&["mv", "old.txt", "new.txt"]);
        fixture.git(&["rm", "-q", "gone.txt"]);
        fixture.file("fresh.txt", "1\n2\n");
        fixture.commit("feature work");
        fixture
    }

    #[test]
    fn choose_base_prefers_pinned_then_pr_then_default() {
        assert_eq!(
            choose_base(Some("dev"), Some("origin/main"), Some("main")).as_deref(),
            Some("dev")
        );
        assert_eq!(
            choose_base(None, Some("origin/main"), Some("main")).as_deref(),
            Some("origin/main")
        );
        assert_eq!(choose_base(None, None, Some("main")).as_deref(), Some("main"));
        assert_eq!(choose_base(None, None, None), None);
        // Blank and option-like values fall through to the next source.
        assert_eq!(
            choose_base(Some("  "), Some("--evil"), Some("main")).as_deref(),
            Some("main")
        );
        assert_eq!(choose_base(Some(" dev "), None, None).as_deref(), Some("dev"));
    }

    #[test]
    fn left_right_counts_parse_strictly() {
        assert_eq!(parse_left_right("3\t5\n"), Some((3, 5)));
        assert_eq!(parse_left_right("0 0"), Some((0, 0)));
        assert_eq!(parse_left_right(""), None);
        assert_eq!(parse_left_right("1"), None);
        assert_eq!(parse_left_right("1 2 3"), None);
        assert_eq!(parse_left_right("a b"), None);
    }

    #[test]
    fn compares_files_counts_and_ahead_behind() {
        let fixture = feature_repo();
        fixture.git(&["switch", "-q", "main"]);
        fixture.file("main-only.txt", "m\n");
        fixture.commit("main moves");
        fixture.git(&["switch", "-q", "feature"]);
        let compare = fixture.open().branch_compare("main").unwrap();
        assert_eq!(compare.status, CompareStatus::Ready);
        assert_eq!((compare.ahead, compare.behind), (1, 1));
        assert_eq!(compare.head_oid.as_deref(), Some(fixture.git(&["rev-parse", "HEAD"]).trim()));
        assert_eq!(
            compare.merge_base.as_deref(),
            Some(fixture.git(&["rev-parse", "main~1"]).trim())
        );
        let file = |path: &str| compare.files.iter().find(|f| f.path == path).unwrap();
        assert_eq!(compare.files.len(), 4, "{:?}", compare.files);
        assert_eq!(file("keep.txt").status, ChangeKind::Modified);
        assert_eq!((file("keep.txt").added, file("keep.txt").removed), (Some(2), Some(1)));
        assert_eq!(file("new.txt").status, ChangeKind::Renamed);
        assert_eq!(file("new.txt").orig_path.as_deref(), Some("old.txt"));
        assert_eq!(file("gone.txt").status, ChangeKind::Deleted);
        assert_eq!(file("fresh.txt").status, ChangeKind::Added);
        // No working tree changes: the totals match the committed lines.
        assert_eq!(compare.totals, (4, 2));
        assert!(!compare.is_empty());
    }

    #[test]
    fn totals_include_staged_unstaged_and_untracked_changes() {
        let fixture = feature_repo();
        fixture.file("keep.txt", "a\nB\nc\nd\ne\n");
        fixture.file("untracked.txt", "u1\nu2\nu3\n");
        fixture.file("bin.dat", b"\0\0\0");
        let compare = fixture.open().branch_compare("main").unwrap();
        // 4 committed + 1 unstaged line + 3 untracked lines; the binary file counts nothing.
        assert_eq!(compare.totals, (4 + 1 + 3, 2));
        assert_eq!(compare.files.len(), 4);
    }

    #[test]
    fn same_commit_short_circuits_but_keeps_working_tree_totals() {
        let fixture = feature_repo();
        let repo = fixture.open();
        let clean = repo.branch_compare("feature").unwrap();
        assert_eq!(clean.status, CompareStatus::Ready);
        assert!(clean.files.is_empty());
        assert!(clean.is_empty());
        assert_eq!(clean.merge_base, clean.head_oid);
        fixture.file("keep.txt", "changed\n");
        let dirty = repo.branch_compare("feature").unwrap();
        assert!(dirty.files.is_empty());
        assert!(dirty.totals.0 > 0 && dirty.totals.1 > 0);
        assert!(!dirty.is_empty());
    }

    #[test]
    fn behind_only_branch_has_no_files() {
        let fixture = feature_repo();
        fixture.git(&["switch", "-q", "main"]);
        let compare = fixture.open().branch_compare("feature").unwrap();
        assert_eq!((compare.ahead, compare.behind), (0, 1));
        assert!(compare.files.is_empty());
        assert_eq!(compare.merge_base, compare.head_oid);
    }

    #[test]
    fn reports_invalid_unborn_and_unrelated_bases() {
        let fixture = TestRepo::new();
        let repo = fixture.open();
        assert_eq!(
            repo.branch_compare("main").unwrap().status,
            CompareStatus::UnbornHead
        );
        fixture.file("a.txt", "a\n");
        fixture.commit("first");
        for bad in ["nope", "", "--output=x", "origin/missing", "a.txt"] {
            let compare = repo.branch_compare(bad).unwrap();
            assert_eq!(compare.status, CompareStatus::InvalidBase, "{bad:?}");
            assert!(compare.files.is_empty());
        }
        fixture.git(&["switch", "-q", "--orphan", "other"]);
        fixture.file("b.txt", "b\n");
        fixture.commit("unrelated");
        let compare = repo.branch_compare("main").unwrap();
        assert_eq!(compare.status, CompareStatus::NoMergeBase);
        assert!(compare.base_oid.is_some());
        assert_eq!(compare.merge_base, None);
    }

    #[test]
    fn default_compare_base_prefers_the_remote_branch_then_local_main() {
        let fixture = TestRepo::new();
        fixture.file("a.txt", "a\n");
        fixture.commit("first");
        let repo = fixture.open();
        assert_eq!(repo.default_compare_base().unwrap().as_deref(), Some("main"));
        let remote = fixture.temp.path().join("remote.git");
        let remote = remote.to_str().unwrap();
        fixture.git(&["init", "--bare", "-q", "-b", "main", remote]);
        fixture.git(&["remote", "add", "origin", remote]);
        fixture.git(&["push", "-q", "origin", "main"]);
        fixture.git(&["fetch", "-q", "origin"]);
        assert_eq!(
            repo.default_compare_base().unwrap().as_deref(),
            Some("origin/main")
        );
        let fresh = TestRepo::new();
        assert_eq!(fresh.open().default_compare_base().unwrap(), None);
    }
}
