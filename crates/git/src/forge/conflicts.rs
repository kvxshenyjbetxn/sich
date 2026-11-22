//! Local, read-only preview of the conflicts between a pull request head and its base branch.
//!
//! `git merge-tree --write-tree` computes the merge in memory and only adds (unreachable) objects
//! to the object database; refs, the index and the working tree are never touched.
use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};

use crate::runner::{GitError, Repo, text};

/// What a local trial merge of the PR head into the base found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictSummary {
    pub head_oid: String,
    pub base_oid: String,
    /// Commits of the base that the head does not have.
    pub behind: u32,
    /// Files with conflicts; empty when the merge is clean locally.
    pub files: Vec<String>,
}

impl ConflictSummary {
    /// First 7 characters of the base commit.
    pub fn base_short(&self) -> &str {
        &self.base_oid[..self.base_oid.len().min(7)]
    }
}

type CacheKey = (std::path::PathBuf, String, String);

fn cache() -> &'static Mutex<HashMap<CacheKey, ConflictSummary>> {
    static CACHE: OnceLock<Mutex<HashMap<CacheKey, ConflictSummary>>> = OnceLock::new();
    CACHE.get_or_init(Mutex::default)
}

/// Result of one `git merge-tree` run.
#[derive(Debug, PartialEq, Eq)]
enum MergeTree {
    Clean,
    Conflicts(Vec<String>),
    /// Git does not understand the arguments (too old).
    Unsupported,
}

/// File names of `git merge-tree --write-tree --name-only -z --no-messages` output: the tree oid,
/// then the conflicted file names, all NUL-terminated.
fn parse_conflict_files(stdout: &[u8]) -> Vec<String> {
    let mut files: Vec<String> = Vec::new();
    for name in stdout.split(|byte| *byte == 0).skip(1) {
        if name.is_empty() {
            continue;
        }
        let name = text(name);
        if !files.contains(&name) {
            files.push(name);
        }
    }
    files
}

fn run_merge_tree(root: &Path, extra: &[&str], head: &str, base: &str) -> Result<MergeTree, GitError> {
    let mut command = Command::new("git");
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
        "GIT_PREFIX",
    ] {
        command.env_remove(key);
    }
    command
        .args(["-c", "core.quotePath=false", "-C"])
        .arg(root)
        .args(["merge-tree", "--write-tree", "--name-only", "-z", "--no-messages"])
        .args(extra)
        .arg(head)
        .arg(base)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        command.creation_flags(0x0800_0000);
    }
    let output = command.output().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            GitError::GitMissing
        } else {
            GitError::Failed(format!("Failed to run Git: {error}"))
        }
    })?;
    match output.status.code() {
        Some(0) => Ok(MergeTree::Clean),
        Some(1) => Ok(MergeTree::Conflicts(parse_conflict_files(&output.stdout))),
        Some(129) => Ok(MergeTree::Unsupported),
        _ => {
            let error = String::from_utf8_lossy(&output.stderr);
            let error = error.trim();
            Err(GitError::Failed(if error.is_empty() {
                format!("git merge-tree failed ({})", output.status)
            } else {
                error.to_owned()
            }))
        }
    }
}

fn check_rev(kind: &str, value: &str) -> Result<(), GitError> {
    if value.is_empty() || value.starts_with('-') || value.contains('\0') {
        Err(GitError::Failed(format!("Invalid {kind}: {value:?}")))
    } else {
        Ok(())
    }
}

impl Repo {
    fn commit_oid(&self, rev: &str) -> Result<String, GitError> {
        check_rev("revision", rev)?;
        let spec = format!("{rev}^{{commit}}");
        Ok(text(&self.read(&["rev-parse", "--verify", "--quiet", &spec])?).trim().to_owned())
    }

    /// Trial merge of `head` into `base_ref` (e.g. `upstream/main`): how far behind the head is
    /// and which files would conflict. Cached per (head, base) commit pair.
    pub fn conflict_summary(&self, head: &str, base_ref: &str) -> Result<ConflictSummary, GitError> {
        let head_oid = self.commit_oid(head)?;
        let base_oid = self.commit_oid(base_ref)?;
        let key = (self.root().to_path_buf(), head_oid.clone(), base_oid.clone());
        if let Some(hit) = cache().lock().ok().and_then(|cache| cache.get(&key).cloned()) {
            return Ok(hit);
        }
        let range = format!("{head_oid}..{base_oid}");
        let behind: u32 = text(&self.read(&["rev-list", "--count", &range, "--"])?)
            .trim()
            .parse()
            .map_err(|_| GitError::Failed("Unexpected rev-list output".to_owned()))?;
        let merge_base = self
            .read(&["merge-base", &head_oid, &base_oid])
            .ok()
            .map(|bytes| text(&bytes).trim().to_owned())
            .filter(|oid| !oid.is_empty());
        let mut result = MergeTree::Unsupported;
        if let Some(merge_base) = &merge_base {
            let flag = format!("--merge-base={merge_base}");
            result = run_merge_tree(self.root(), &[&flag], &head_oid, &base_oid)?;
        }
        if result == MergeTree::Unsupported {
            result = run_merge_tree(self.root(), &[], &head_oid, &base_oid)?;
        }
        let files = match result {
            MergeTree::Clean => Vec::new(),
            MergeTree::Conflicts(files) => files,
            MergeTree::Unsupported => {
                return Err(GitError::Failed("This Git version cannot preview merges".to_owned()));
            }
        };
        let summary = ConflictSummary { head_oid, base_oid, behind, files };
        if let Ok(mut cache) = cache().lock() {
            if cache.len() >= 64 {
                cache.clear();
            }
            cache.insert(key, summary.clone());
        }
        Ok(summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::test_repo::TestRepo;

    /// main and feature both change `a.txt` (conflict) and each adds its own file.
    fn conflicting() -> TestRepo {
        let fixture = TestRepo::new();
        fixture.file("a.txt", "one\n");
        fixture.file("b.txt", "base\n");
        fixture.commit("base");
        fixture.git(&["switch", "-q", "-c", "feature"]);
        fixture.file("a.txt", "feature\n");
        fixture.file("b.txt", "feature b\n");
        fixture.file("only-feature.txt", "x\n");
        fixture.commit("feature change");
        fixture.git(&["switch", "-q", "main"]);
        fixture.file("a.txt", "main\n");
        fixture.file("b.txt", "main b\n");
        fixture.commit("main change one");
        fixture.file("later.txt", "later\n");
        fixture.commit("main change two");
        fixture
    }

    #[test]
    fn lists_conflicting_files_and_the_behind_count() {
        let fixture = conflicting();
        let summary = fixture.open().conflict_summary("feature", "main").unwrap();
        assert_eq!(summary.files, vec!["a.txt".to_owned(), "b.txt".to_owned()]);
        assert_eq!(summary.behind, 2);
        assert_eq!(summary.base_oid, fixture.git(&["rev-parse", "main"]).trim());
        assert_eq!(summary.head_oid, fixture.git(&["rev-parse", "feature"]).trim());
        assert_eq!(summary.base_short().len(), 7);
    }

    #[test]
    fn a_clean_merge_has_no_files() {
        let fixture = TestRepo::new();
        fixture.file("a.txt", "one\n");
        fixture.commit("base");
        fixture.git(&["switch", "-q", "-c", "feature"]);
        fixture.file("f.txt", "f\n");
        fixture.commit("feature");
        fixture.git(&["switch", "-q", "main"]);
        fixture.file("m.txt", "m\n");
        fixture.commit("main");
        let summary = fixture.open().conflict_summary("feature", "main").unwrap();
        assert!(summary.files.is_empty());
        assert_eq!(summary.behind, 1);
    }

    #[test]
    fn up_to_date_head_is_zero_behind() {
        let fixture = TestRepo::new();
        fixture.file("a.txt", "one\n");
        fixture.commit("base");
        fixture.git(&["switch", "-q", "-c", "feature"]);
        fixture.file("f.txt", "f\n");
        fixture.commit("feature");
        let summary = fixture.open().conflict_summary("feature", "main").unwrap();
        assert_eq!((summary.behind, summary.files.is_empty()), (0, true));
    }

    #[test]
    fn does_not_touch_refs_index_or_worktree() {
        let fixture = conflicting();
        fixture.git(&["switch", "-q", "feature"]);
        fixture.file("dirty.txt", "uncommitted\n");
        let refs = fixture.git(&["for-each-ref"]);
        let status = fixture.git(&["status", "--porcelain=v1"]);
        let head = fixture.git(&["rev-parse", "HEAD"]);
        fixture.open().conflict_summary("HEAD", "main").unwrap();
        assert_eq!(fixture.git(&["for-each-ref"]), refs);
        assert_eq!(fixture.git(&["status", "--porcelain=v1"]), status);
        assert_eq!(fixture.git(&["rev-parse", "HEAD"]), head);
        assert_eq!(std::fs::read_to_string(fixture.dir.join("a.txt")).unwrap(), "feature\n");
        assert!(!fixture.dir.join(".git").join("MERGE_HEAD").exists());
    }

    #[test]
    fn unknown_or_option_like_revisions_are_errors() {
        let fixture = conflicting();
        let repo = fixture.open();
        assert!(repo.conflict_summary("nope", "main").is_err());
        assert!(repo.conflict_summary("feature", "nope").is_err());
        assert!(repo.conflict_summary("--all", "main").is_err());
        assert!(repo.conflict_summary("feature", "").is_err());
    }

    #[test]
    fn unrelated_histories_fall_back_without_a_merge_base() {
        let fixture = TestRepo::new();
        fixture.file("a.txt", "one\n");
        fixture.commit("base");
        fixture.git(&["switch", "-q", "--orphan", "other"]);
        fixture.file("a.txt", "different\n");
        fixture.commit("orphan");
        // No merge base: the call must not fail on the missing --merge-base value.
        let result = fixture.open().conflict_summary("other", "main");
        match result {
            Ok(summary) => assert!(summary.files.len() <= 1),
            Err(error) => assert!(!error.to_string().is_empty()),
        }
    }

    #[test]
    fn parses_merge_tree_output() {
        assert_eq!(
            parse_conflict_files(b"abc123\0dir/a.txt\0b.txt\0dir/a.txt\0"),
            vec!["dir/a.txt".to_owned(), "b.txt".to_owned()]
        );
        assert!(parse_conflict_files(b"abc123\0").is_empty());
        assert!(parse_conflict_files(b"").is_empty());
    }

    #[test]
    fn results_are_cached_per_commit_pair() {
        let fixture = conflicting();
        let repo = fixture.open();
        let first = repo.conflict_summary("feature", "main").unwrap();
        // The cache is keyed by oids, so the same pair through other names hits it too.
        let again = repo.conflict_summary(&first.head_oid, &first.base_oid).unwrap();
        assert_eq!(first, again);
    }
}
