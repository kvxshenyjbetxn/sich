//! Native worktree notifications and Git metadata classification.

use std::path::{Path, PathBuf};

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::{GitError, Repo};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WatchSignal {
    pub files_changed: bool,
    pub refs_changed: bool,
}

impl WatchSignal {
    pub const FILES: Self = Self {
        files_changed: true,
        refs_changed: false,
    };
    pub const REFS: Self = Self {
        files_changed: true,
        refs_changed: true,
    };

    pub fn merge(self, other: Self) -> Self {
        Self {
            files_changed: self.files_changed || other.files_changed,
            refs_changed: self.refs_changed || other.refs_changed,
        }
    }
}

#[derive(Debug)]
struct WatchRoots {
    root: PathBuf,
    git_dir: PathBuf,
    common_dir: PathBuf,
}

impl WatchRoots {
    fn path_signal(&self, path: &Path) -> WatchSignal {
        if path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().ends_with(".lock"))
        {
            return WatchSignal::default();
        }
        for directory in [&self.git_dir, &self.common_dir] {
            if let Ok(relative) = path.strip_prefix(directory) {
                return git_signal(relative);
            }
        }
        let Ok(relative) = path.strip_prefix(&self.root) else {
            return WatchSignal::default();
        };
        let Some(first) = relative.components().next() else {
            return WatchSignal::FILES;
        };
        match first.as_os_str().to_str() {
            Some(".git") if relative == Path::new(".git") => WatchSignal::REFS,
            Some(".git" | "node_modules" | "target" | ".venv" | "dist" | "build") => {
                WatchSignal::default()
            }
            _ => WatchSignal::FILES,
        }
    }

    fn signal(&self, event: &Event) -> WatchSignal {
        if event.need_rescan() {
            return WatchSignal::REFS;
        }
        if matches!(event.kind, EventKind::Access(_)) {
            return WatchSignal::default();
        }
        event
            .paths
            .iter()
            .fold(WatchSignal::default(), |signal, path| {
                signal.merge(self.path_signal(path))
            })
    }
}

fn git_signal(path: &Path) -> WatchSignal {
    if path.starts_with("refs")
        || path == Path::new("logs/HEAD")
        || path == Path::new("logs/refs/stash")
        || path == Path::new("rebase-merge")
        || path == Path::new("rebase-apply")
    {
        return WatchSignal::REFS;
    }
    match path.to_str() {
        Some("HEAD" | "packed-refs") => WatchSignal::REFS,
        Some("index" | "FETCH_HEAD" | "MERGE_HEAD" | "CHERRY_PICK_HEAD" | "REVERT_HEAD") => {
            WatchSignal::FILES
        }
        _ => WatchSignal::default(),
    }
}

pub fn start(
    repo: &Repo,
    on_change: impl Fn(WatchSignal) + Send + 'static,
) -> Result<RecommendedWatcher, GitError> {
    let roots = WatchRoots {
        root: repo.root.clone(),
        git_dir: repo.git_dir.clone(),
        common_dir: repo.common_dir.clone(),
    };
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<Event>| {
        let signal = event.map_or(WatchSignal::REFS, |event| roots.signal(&event));
        if signal != WatchSignal::default() {
            on_change(signal);
        }
    })
    .map_err(watch_error)?;
    watcher
        .watch(&repo.root, RecursiveMode::Recursive)
        .map_err(watch_error)?;
    for (index, path) in [&repo.git_dir, &repo.common_dir].into_iter().enumerate() {
        if path.starts_with(&repo.root) || index == 1 && path == &repo.git_dir {
            continue;
        }
        watcher
            .watch(path, RecursiveMode::NonRecursive)
            .map_err(watch_error)?;
        // refs can be absent immediately after initialization or when all refs are packed.
        let refs = path.join("refs");
        if refs.is_dir() {
            watcher
                .watch(&refs, RecursiveMode::Recursive)
                .map_err(watch_error)?;
        }
        // Reflogs are optional. The filter only forwards logs/HEAD from this directory.
        let logs = path.join("logs");
        if logs.is_dir() {
            watcher
                .watch(&logs, RecursiveMode::NonRecursive)
                .map_err(watch_error)?;
        }
    }
    Ok(watcher)
}

fn watch_error(error: notify::Error) -> GitError {
    GitError::Failed(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::runner::test_repo::TestRepo;

    #[test]
    fn classifies_worktree_and_metadata_paths() {
        let roots = WatchRoots {
            root: PathBuf::from("work"),
            git_dir: PathBuf::from("meta/worktrees/work"),
            common_dir: PathBuf::from("meta"),
        };
        for path in [
            "meta/worktrees/work/HEAD",
            "meta/worktrees/work/logs/HEAD",
            "meta/refs/remotes/origin/main",
            "meta/packed-refs",
            "meta/refs/stash",
            "meta/logs/refs/stash",
            "meta/worktrees/work/rebase-merge",
            "meta/worktrees/work/rebase-apply",
        ] {
            assert_eq!(
                roots.path_signal(Path::new(path)),
                WatchSignal::REFS,
                "{path}"
            );
        }
        for path in [
            "meta/worktrees/work/index",
            "meta/worktrees/work/MERGE_HEAD",
            "meta/worktrees/work/CHERRY_PICK_HEAD",
            "meta/worktrees/work/REVERT_HEAD",
            "meta/worktrees/work/FETCH_HEAD",
            "work/src/main.rs",
            "work/src/build/mod.rs",
        ] {
            assert_eq!(
                roots.path_signal(Path::new(path)),
                WatchSignal::FILES,
                "{path}"
            );
        }
        for path in [
            "work/node_modules/pkg/file",
            "work/build/file",
            "work/dist/file",
            "work/target/file",
            "work/.venv/file",
            "meta/objects/id",
            "meta/logs/refs/main",
            "meta/worktrees/work/rebase-merge/done",
            "meta/worktrees/work/index.lock",
            "work/cargo.lock",
            "something-else",
        ] {
            assert_eq!(
                roots.path_signal(Path::new(path)),
                WatchSignal::default(),
                "{path}"
            );
        }
    }

    #[test]
    fn native_watcher_reports_file_edits_and_commits_in_main_and_linked_worktrees() {
        let repo = TestRepo::new();
        repo.file("file", "one\n");
        repo.commit("initial");
        let linked = repo.temp.path().join("linked");
        repo.git(&["worktree", "add", "-qb", "linked", linked.to_str().unwrap()]);
        for path in [&repo.dir, &linked] {
            let (tx, rx) = mpsc::channel();
            let open = repo.open_at(path);
            let _watcher = start(&open, move |signal| {
                let _ = tx.send(signal);
            })
            .unwrap();
            std::fs::write(path.join("file"), "two\n").unwrap();
            let wait_for = |predicate: fn(WatchSignal) -> bool| {
                let deadline = Instant::now() + Duration::from_secs(2);
                loop {
                    let signal = rx
                        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                        .unwrap();
                    if predicate(signal) {
                        return;
                    }
                }
            };
            wait_for(|signal| signal.files_changed);
            open.stage_all().unwrap();
            open.commit("second").unwrap();
            wait_for(|signal| signal.refs_changed);
        }
    }
}
