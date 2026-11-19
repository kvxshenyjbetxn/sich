//! Thin wrapper over the `git` CLI. All functions block; call them from `spawn_blocking`.

use std::path::{Path, PathBuf};
use std::process::Command;

use sich_proto::WorktreeInfo;

/// Result with a user-readable error message.
pub type GitResult<T> = Result<T, String>;

fn command(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir).env("GIT_TERMINAL_PROMPT", "0");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd
}

/// Runs `git -C <dir> <args>`; returns trimmed stdout, or stderr as the error.
pub fn run(dir: &Path, args: &[&str]) -> GitResult<String> {
    let out = command(dir).args(args).output().map_err(|e| format!("failed to run git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(if err.is_empty() { format!("git {} failed", args.join(" ")) } else { err })
    }
}

/// Canonical path without the Windows `\\?\` prefix; falls back to the input when missing.
pub fn normalize(path: &Path) -> PathBuf {
    dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Compares two paths after normalization (case-insensitive on Windows).
pub fn same_path(a: &Path, b: &Path) -> bool {
    let (a, b) = (normalize(a), normalize(b));
    if cfg!(windows) {
        a.to_string_lossy().eq_ignore_ascii_case(&b.to_string_lossy())
    } else {
        a == b
    }
}

/// Repository root containing `path`, if it is inside a git work tree.
pub fn toplevel(path: &Path) -> Option<PathBuf> {
    let top = run(path, &["rev-parse", "--show-toplevel"]).ok()?;
    if top.is_empty() { None } else { Some(normalize(Path::new(&top))) }
}

pub fn is_dirty(path: &Path) -> bool {
    path.is_dir() && run(path, &["status", "--porcelain"]).is_ok_and(|s| !s.is_empty())
}

/// Parses `git worktree list --porcelain` (first entry = main worktree, prunable skipped).
pub fn list_worktrees(repo: &Path) -> GitResult<Vec<WorktreeInfo>> {
    let text = run(repo, &["worktree", "list", "--porcelain"])?;
    let text = text.replace("\r\n", "\n");
    let mut list: Vec<WorktreeInfo> = Vec::new();
    for (idx, block) in text.split("\n\n").enumerate() {
        let (mut path, mut branch, mut head, mut prunable) = (None, None, String::new(), false);
        for line in block.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                path = Some(normalize(Path::new(p)));
            } else if let Some(h) = line.strip_prefix("HEAD ") {
                head = h.chars().take(7).collect();
            } else if let Some(b) = line.strip_prefix("branch ") {
                branch = Some(b.strip_prefix("refs/heads/").unwrap_or(b).to_string());
            } else if line.starts_with("prunable") {
                prunable = true;
            }
        }
        let Some(path) = path else { continue };
        if prunable {
            continue;
        }
        let dirty = is_dirty(&path);
        list.push(WorktreeInfo { path, branch, head, is_main: idx == 0, dirty });
    }
    Ok(list)
}

/// Rejects names that are invalid branches or unsafe as a folder name.
pub fn validate_name(repo: &Path, name: &str) -> GitResult<()> {
    if name.is_empty() || name.contains(['/', '\\']) || name.contains("..") {
        return Err("Invalid name: must not be empty or contain '/', '\\' or '..'".into());
    }
    run(repo, &["check-ref-format", "--branch", name])
        .map(|_| ())
        .map_err(|_| format!("Invalid branch name: {name}"))
}

pub fn branch_exists(repo: &Path, name: &str) -> bool {
    run(repo, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{name}")]).is_ok()
}

/// Creates `target` as a worktree on branch `name` (new from `base`/HEAD, or the existing one).
pub fn add_worktree(repo: &Path, target: &Path, name: &str, base: Option<&str>) -> GitResult<()> {
    let target_s = target.to_string_lossy().to_string();
    if branch_exists(repo, name) {
        if list_worktrees(repo)?.iter().any(|w| w.branch.as_deref() == Some(name)) {
            return Err(format!("Branch '{name}' is already checked out in another worktree"));
        }
        run(repo, &["worktree", "add", &target_s, name]).map(|_| ())
    } else {
        let base = base.filter(|b| !b.is_empty()).unwrap_or("HEAD");
        run(repo, &["worktree", "add", "-b", name, &target_s, base]).map(|_| ())
    }
}

/// Removes a worktree. A local-changes failure without `force` gets a friendly message.
pub fn remove_worktree(repo: &Path, path: &Path, force: bool) -> GitResult<()> {
    let p = path.to_string_lossy().to_string();
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(&p);
    run(repo, &args).map(|_| ()).map_err(|e| {
        if !force && (e.contains("--force") || e.contains("modified or untracked")) {
            "Worktree has uncommitted changes".to_string()
        } else {
            e
        }
    })
}

pub fn delete_branch(repo: &Path, branch: &str, force: bool) -> GitResult<()> {
    run(repo, &["branch", if force { "-D" } else { "-d" }, branch]).map(|_| ())
}
