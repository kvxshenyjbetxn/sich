use std::collections::HashSet;
use std::path::PathBuf;

use crate::runner::{GitError, Repo, literal_path, text};

impl Repo {
    pub fn stage(&self, paths: &[String]) -> Result<(), GitError> {
        self.with_paths("add", &[], paths)
    }

    pub fn unstage(&self, paths: &[String]) -> Result<(), GitError> {
        if self.has_head()? {
            self.with_paths("reset", &["-q"], paths)
        } else {
            self.with_paths("rm", &["--cached", "-q", "-f", "--ignore-unmatch"], paths)
        }
    }

    pub fn stage_all(&self) -> Result<(), GitError> {
        self.write(&["add", "-A"], None)
    }

    pub fn unstage_all(&self) -> Result<(), GitError> {
        if self.has_head()? {
            self.write(&["reset", "-q"], None)
        } else {
            self.write(
                &["rm", "--cached", "-qr", "-f", "--ignore-unmatch", "--", "."],
                None,
            )
        }
    }

    pub fn discard(&self, tracked: &[String], untracked: &[String]) -> Result<(), GitError> {
        let paths = self.untracked_to_remove(untracked)?;
        self.with_paths("restore", &["--worktree"], tracked)?;
        for path in paths {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    pub fn discard_all(&self) -> Result<(), GitError> {
        let status = crate::status::parse_status(&self.status_output()?, usize::MAX);
        let (mut tracked, mut untracked) = (Vec::new(), Vec::new());
        for file in status.unstaged {
            if file.status == crate::ChangeKind::Untracked {
                untracked.push(file.path);
            } else {
                if file.status == crate::ChangeKind::Renamed {
                    tracked.extend(file.orig_path);
                }
                tracked.push(file.path);
            }
        }
        self.discard(&tracked, &untracked)
    }

    pub fn commit(&self, message: &str) -> Result<(), GitError> {
        if message.trim().is_empty() {
            return Err(GitError::Failed("Commit message is empty".into()));
        }
        self.write(&["commit", "-F", "-"], Some(message.as_bytes()))
    }

    pub fn fetch(&self) -> Result<(), GitError> {
        self.write(&["fetch", "--prune"], None)
    }

    pub fn pull(&self) -> Result<(), GitError> {
        self.write(&["pull", "--ff-only"], None)
    }

    pub fn push(&self) -> Result<(), GitError> {
        self.push_with(&["push"])
    }

    pub fn push_force_with_lease(&self) -> Result<(), GitError> {
        self.push_with(&["push", "--force-with-lease"])
    }

    pub fn pull_rebase(&self) -> Result<(), GitError> {
        self.write(&["pull", "--rebase"], None)
    }

    pub fn commit_amend(&self, message: &str) -> Result<(), GitError> {
        if message.trim().is_empty() {
            return Err(GitError::Failed("Commit message is empty".into()));
        }
        self.write(&["commit", "--amend", "-F", "-"], Some(message.as_bytes()))
    }

    pub fn last_commit_message(&self) -> Result<Option<String>, GitError> {
        if !self.has_head()? {
            return Ok(None);
        }
        let output = self.read(&["log", "-1", "--format=%B"])?;
        Ok(Some(text(&output).trim_end().to_owned()))
    }

    pub fn undo_last_commit(&self) -> Result<(), GitError> {
        if !self.has_head()? {
            return Err(GitError::Failed("No commits to undo".into()));
        }
        match self.read(&["rev-parse", "--verify", "--quiet", "HEAD~1"]) {
            Ok(_) => {}
            Err(GitError::Failed(_)) => {
                return Err(GitError::Failed("Cannot undo the initial commit".into()));
            }
            Err(error) => return Err(error),
        }
        self.write(&["reset", "--soft", "HEAD~1"], None)
    }

    /// Runs `base` (`push` or `push --force-with-lease`), adding the upstream when missing.
    fn push_with(&self, base: &[&str]) -> Result<(), GitError> {
        let branch = match self.read(&["symbolic-ref", "HEAD"]) {
            Ok(branch) => text(&branch),
            Err(GitError::Failed(error)) if error.contains("not a symbolic ref") => {
                return self.write(base, None);
            }
            Err(error) => return Err(error),
        };
        let upstream = self.read(&["for-each-ref", "--format=%(upstream)", branch.trim_end()])?;
        if upstream.trim_ascii().is_empty() {
            let mut args = base.to_vec();
            args.extend(["--set-upstream", "origin", "HEAD"]);
            self.write(&args, None)
        } else {
            self.write(base, None)
        }
    }

    pub(crate) fn has_head(&self) -> Result<bool, GitError> {
        match self.read(&["rev-parse", "--verify", "HEAD"]) {
            Ok(_) => Ok(true),
            Err(GitError::Failed(error)) if error.contains("Needed a single revision") => Ok(false),
            Err(error) => Err(error),
        }
    }

    fn with_paths(&self, verb: &str, options: &[&str], paths: &[String]) -> Result<(), GitError> {
        if paths.is_empty() {
            return Ok(());
        }
        let mut stdin = Vec::new();
        for path in paths {
            literal_path(path)?;
            stdin.extend_from_slice(path.as_bytes());
            stdin.push(0);
        }
        let mut args = vec![verb];
        args.extend_from_slice(options);
        args.extend(["--pathspec-from-file=-", "--pathspec-file-nul"]);
        self.write(&args, Some(&stdin))
    }

    fn untracked_to_remove(&self, paths: &[String]) -> Result<Vec<PathBuf>, GitError> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let tracked = self.read(&["ls-files", "-z"])?;
        let tracked: HashSet<String> = tracked
            .split(|&b| b == 0)
            .map(|path| platform_path_key(text(path)))
            .collect();
        let root = self.root();
        paths
            .iter()
            .map(|path| {
                let invalid = || GitError::Failed(format!("Cannot discard this path: {path}"));
                let absolute = root.join(literal_path(path)?);
                let parent =
                    dunce::canonicalize(absolute.parent().expect("validated relative path"))?;
                if !parent.starts_with(root) {
                    return Err(invalid());
                }
                let target = parent.join(absolute.file_name().expect("validated relative path"));
                let metadata = std::fs::symlink_metadata(&target)?;
                if metadata.is_dir() {
                    return Err(invalid());
                }
                // Normalize file aliases while keeping symlinks themselves as the deletion target.
                let target = if metadata.is_symlink() {
                    target
                } else {
                    dunce::canonicalize(target)?
                };
                let relative = target.strip_prefix(root).map_err(|_| invalid())?;
                let actual = relative
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("/");
                // A symlink to another directory inside the worktree may also alias Git metadata.
                literal_path(&actual)?;
                if tracked.contains(&platform_path_key(actual)) {
                    return Err(GitError::Failed(format!("File is now tracked: {path}")));
                }
                Ok(target)
            })
            .collect()
    }
}

fn platform_path_key(path: String) -> String {
    if cfg!(windows) {
        path.to_lowercase()
    } else {
        path
    }
}
