//! Git checks and ownership metadata for orchestration worktrees.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sich_proto::TerminalInfo;

use crate::git::{self, GitResult};

pub const CLIENT_NAME: &str = "sich-orch-worktree";
const MARKER: &str = "sich-worker-worktree.json";

pub fn client_name(owner: Option<&str>) -> String {
    owner.map_or_else(
        || CLIENT_NAME.to_owned(),
        |id| format!("{CLIENT_NAME}/{id}"),
    )
}

pub(crate) fn client_owner(client_name: &str) -> Option<&str> {
    let id = client_name.strip_prefix(CLIENT_NAME)?.strip_prefix('/')?;
    let hex = id.strip_prefix("d_")?;
    (hex.len() == 8 && hex.bytes().all(|b| b.is_ascii_hexdigit())).then_some(id)
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkerWorktree {
    pub identity: String,
    pub branch: String,
    pub base_branch: String,
}

fn git_dir(path: &Path) -> GitResult<PathBuf> {
    git::run(path, &["rev-parse", "--absolute-git-dir"]).map(PathBuf::from)
}

impl WorkerWorktree {
    pub fn read(path: &Path) -> GitResult<Option<Self>> {
        let marker = git_dir(path)?.join(MARKER);
        match std::fs::read(marker) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| e.to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }

    fn write(path: &Path, branch: &str, base_branch: &str, owner: &str) -> GitResult<()> {
        use std::io::Write as _;

        // Keep ownership in Git's metadata so it never dirties the checkout or travels in commits.
        let dir = git_dir(path)?;
        let mut file = tempfile::NamedTempFile::new_in(&dir).map_err(|e| e.to_string())?;
        let marker = Self {
            identity: owner.into(),
            branch: branch.into(),
            base_branch: base_branch.into(),
        };
        file.write_all(&serde_json::to_vec(&marker).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        file.persist_noclobber(dir.join(MARKER))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

pub fn current_branch(path: &Path) -> GitResult<String> {
    let reference = git::run(path, &["symbolic-ref", "--quiet", "HEAD"])?;
    reference
        .strip_prefix("refs/heads/")
        .map(str::to_owned)
        .ok_or_else(|| "HEAD is not a local branch".into())
}

pub fn same_path(a: &Path, b: &Path) -> bool {
    git::same_path(a, b)
}

pub fn root(path: &Path) -> Option<PathBuf> {
    git::toplevel(path)
}

pub fn contains_live_terminal(path: &Path, terminals: &[TerminalInfo]) -> bool {
    let root = git::normalize(path);
    terminals.iter().any(|term| {
        term.exited.is_none()
            && (term.worktree.as_ref().is_some_and(|w| same_path(&root, w))
                || git::normalize(&term.cwd)
                    .ancestors()
                    .any(|dir| same_path(&root, dir)))
    })
}

/// Returns (dirty, merged). Errors leave the worktree ineligible for automatic removal.
pub fn cleanup_status(path: &Path, branch: &str, base_branch: &str) -> GitResult<(bool, bool)> {
    let status = git::run(
        path,
        &[
            "status",
            "--porcelain",
            "--untracked-files=normal",
            "--ignore-submodules=none",
        ],
    )?;
    if !status.is_empty() {
        return Ok((true, false));
    }
    let branch = format!("refs/heads/{branch}");
    let base = format!("refs/heads/{base_branch}");
    let base_oid = git::run(path, &["show-ref", "--verify", "--hash", &base])?;
    let merged_into = format!("--merged={base_oid}");
    let merged = git::run(
        path,
        &["for-each-ref", &merged_into, "--format=%(refname)", &branch],
    )?;
    Ok((false, merged.lines().any(|reference| reference == branch)))
}

pub(crate) fn validate_name(repo: &Path, name: &str) -> GitResult<()> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_/.".contains(&c))
        || name.split('/').any(|part| part.is_empty())
    {
        return Err(format!("Invalid worktree branch name: {name}"));
    }
    git::run(repo, &["check-ref-format", "--branch", name]).map(|_| ())
}

pub(crate) fn add(
    repo: &Path,
    target: &Path,
    branch: &str,
    base: Option<&str>,
    owner: &str,
) -> GitResult<()> {
    let base = base.ok_or("A coordinator base branch is required")?;
    let base_branch = base
        .strip_prefix("refs/heads/")
        .ok_or("Expected a local base branch ref")?;
    let target_str = target.to_string_lossy();
    // -b is exclusive even if another Git process creates the branch after the check.
    // The local upstream makes Git's branch -d guard use the same base as orchestration.
    git::run(
        repo,
        &[
            "worktree",
            "add",
            "--track",
            "-b",
            branch,
            &target_str,
            base,
        ],
    )?;
    if let Err(e) = WorkerWorktree::write(target, branch, base_branch, owner) {
        if git::remove_worktree(repo, target, false).is_ok() {
            let _ = git::delete_branch(repo, branch, false);
        }
        return Err(e);
    }
    Ok(())
}

pub(crate) fn validate_removal(
    path: &Path,
    branch: Option<&str>,
    owner: &str,
    force: bool,
    terminals: &[TerminalInfo],
) -> GitResult<()> {
    let marker = WorkerWorktree::read(path)?.ok_or("Not an orchestration worktree")?;
    if marker.identity != owner {
        return Err("The worktree belongs to a different dispatch".into());
    }
    if branch != Some(&marker.branch) {
        return Err("The worker branch is no longer checked out in this worktree".into());
    }
    if !force {
        if contains_live_terminal(path, terminals) {
            return Err("A live terminal is still using the worktree".into());
        }
        match cleanup_status(path, &marker.branch, &marker.base_branch)? {
            (false, true) => {}
            (true, _) => return Err("Worktree has uncommitted changes".into()),
            (false, false) => {
                return Err("Worker branch has not been merged into its base branch".into());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(path: &Path, args: &[&str]) {
        let hooks = path.join("no-test-hooks");
        let hooks = format!("core.hooksPath={}", hooks.display());
        let mut cfg = vec![
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgSign=false",
            "-c",
            &hooks,
        ];
        cfg.extend_from_slice(args);
        git::run(path, &cfg).unwrap();
    }

    #[test]
    fn cleanup_uses_local_base_and_sees_untracked_but_not_ignored_files() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("main");
        let worktree = temp.path().join("worker");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "--template=", "-b", "other"]);
        let hooks = temp.path().join("no-test-hooks");
        git(
            &repo,
            &[
                "config",
                "--local",
                "core.hooksPath",
                hooks.to_str().unwrap(),
            ],
        );
        std::fs::write(repo.join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(repo.join("tracked.txt"), "original\n").unwrap();
        git(&repo, &["add", ".gitignore", "tracked.txt"]);
        git(&repo, &["commit", "-m", "Initial"]);
        git(&repo, &["branch", "coordinator"]);
        add(
            &repo,
            &worktree,
            "workers/test",
            Some("refs/heads/coordinator"),
            "d_00000001",
        )
        .unwrap();

        let status = || cleanup_status(&worktree, "workers/test", "coordinator").unwrap();
        assert_eq!(status(), (false, true));
        git(&worktree, &["commit", "--allow-empty", "-m", "Worker work"]);
        assert_eq!(status(), (false, false));
        git(&repo, &["branch", "-f", "coordinator", "workers/test"]);
        assert_eq!(status(), (false, true));
        assert_eq!(
            cleanup_status(&worktree, "workers/test", "other").unwrap(),
            (false, false)
        );
        assert!(cleanup_status(&worktree, "workers/test", "missing").is_err());

        std::fs::write(worktree.join("tracked.txt"), "edit\n").unwrap();
        assert!(status().0);
        git(&worktree, &["restore", "tracked.txt"]);
        std::fs::write(worktree.join("untracked.txt"), "new\n").unwrap();
        assert!(status().0);
        std::fs::remove_file(worktree.join("untracked.txt")).unwrap();
        std::fs::write(worktree.join("ignored.txt"), "ignored\n").unwrap();
        assert_eq!(status(), (false, true));

        assert!(
            validate_removal(&worktree, Some("workers/test"), "d_00000002", true, &[]).is_err()
        );
        validate_removal(&worktree, Some("workers/test"), "d_00000001", false, &[]).unwrap();
        git::remove_worktree(&repo, &worktree, false).unwrap();
        git::delete_branch(&repo, "workers/test", false).unwrap();
    }
}
