//! Sich's sidebar preferences, shared by the GUI and the orchestration CLI.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sich_proto::{ProjectInfo, WorktreeInfo};

use crate::{settings, workspace::normalize_path};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Store(BTreeMap<String, Project>);

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Project {
    // The CLI can register a worktree before the GUI sees the project for the first time.
    initialized: bool,
    paths: BTreeMap<String, Visibility>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Visibility {
    Shown,
    Hidden,
}

/// Records a successfully created Sich worktree without requiring a running GUI.
pub fn record_created(project_root: &Path, path: &Path) -> anyhow::Result<()> {
    import(project_root, path)
}

/// Makes a worktree visible in the sidebar.
pub fn import(project_root: &Path, path: &Path) -> anyhow::Result<()> {
    set_external(project_root, &[path], true).map(|_| ())
}

/// Dismisses a worktree from the external worktree row.
pub fn hide_external(project_root: &Path, path: &Path) -> anyhow::Result<()> {
    set_external(project_root, &[path], false).map(|_| ())
}

/// Returns visible worktrees and worktrees that still need an import or hide decision.
pub fn visible<'a>(
    project_root: &Path,
    all_worktrees: &'a [WorktreeInfo],
    store: &Store,
) -> (Vec<&'a WorktreeInfo>, Vec<&'a WorktreeInfo>) {
    let Some(project) = store
        .0
        .get(&normalize_path(project_root))
        .filter(|p| p.initialized)
    else {
        return (all_worktrees.iter().collect(), Vec::new());
    };

    let mut shown = Vec::new();
    let mut new_external = Vec::new();
    for worktree in all_worktrees {
        if worktree.is_main {
            shown.push(worktree);
            continue;
        }
        match project.paths.get(&normalize_path(&worktree.path)) {
            Some(Visibility::Shown) => shown.push(worktree),
            Some(Visibility::Hidden) => {}
            None => new_external.push(worktree),
        }
    }
    (shown, new_external)
}

pub(crate) fn sidebar_worktrees<'a>(
    project: &'a ProjectInfo,
    store: &Store,
    show_all: bool,
    selected: Option<&Path>,
) -> (Vec<&'a WorktreeInfo>, Vec<&'a WorktreeInfo>) {
    if show_all {
        return (project.worktrees.iter().collect(), Vec::new());
    }
    let (mut shown, external) = visible(&project.path, &project.worktrees, store);
    if let Some(selected) = selected.map(normalize_path)
        && !shown.iter().any(|w| normalize_path(&w.path) == selected)
        && let Some(worktree) = project
            .worktrees
            .iter()
            .find(|w| normalize_path(&w.path) == selected)
    {
        shown.push(worktree);
    }
    (shown, external)
}

pub(crate) fn set_external(root: &Path, paths: &[&Path], show: bool) -> anyhow::Result<Store> {
    set_external_at(&config_path()?, root, paths, show)
}

fn set_external_at(file: &Path, root: &Path, paths: &[&Path], show: bool) -> anyhow::Result<Store> {
    update(file, |store| store.set_paths(root, paths, show))
}

pub(crate) fn reconcile(projects: &[ProjectInfo]) -> anyhow::Result<Store> {
    update(&config_path()?, |store| {
        for project in projects.iter().filter(|p| p.error.is_none()) {
            store.reconcile_project(project, |path| !matches!(path.try_exists(), Ok(false)));
        }
    })
}

impl Store {
    fn set_paths(&mut self, root: &Path, paths: &[&Path], show: bool) {
        let project = self.0.entry(normalize_path(root)).or_default();
        for path in paths {
            let path = normalize_path(path);
            if show {
                project.paths.insert(path, Visibility::Shown);
            } else {
                // A CLI creation may have made this path visible since the GUI last read it.
                project.paths.entry(path).or_insert(Visibility::Hidden);
            }
        }
    }

    fn reconcile_project(&mut self, project: &ProjectInfo, exists: impl Fn(&Path) -> bool) {
        let entry = self.0.entry(normalize_path(&project.path)).or_default();
        let listed: BTreeSet<_> = project
            .worktrees
            .iter()
            .filter(|w| !w.is_main)
            .map(|w| normalize_path(&w.path))
            .collect();
        if !entry.initialized {
            for path in &listed {
                entry.paths.entry(path.clone()).or_insert(Visibility::Shown);
            }
            entry.initialized = true;
        }
        // Git's snapshot can predate a CLI registration. Keep that entry if its directory
        // exists, even when we haven't seen it in the daemon's list yet.
        entry
            .paths
            .retain(|path, _| listed.contains(path) || exists(Path::new(path)));
    }
}

fn config_path() -> anyhow::Result<PathBuf> {
    settings::config_file("worktrees.json").ok_or_else(|| anyhow::anyhow!("no config directory"))
}

/// Serializes the entire read-modify-write across processes. The OS releases the lock if a
/// writer exits, and the lock file stays separate from the JSON file we atomically replace.
fn update(path: &Path, change: impl FnOnce(&mut Store)) -> anyhow::Result<Store> {
    let dir = path.parent().expect("config file has a parent directory");
    fs::create_dir_all(dir)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.with_extension("lock"))?;
    lock.lock()?;

    let mut store = match fs::read(path) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(store) => store,
            Err(error) => {
                tracing::warn!("invalid worktree preferences {}: {error}", path.display());
                let mut backup = tempfile::NamedTempFile::new_in(dir)?;
                backup.write_all(&bytes)?;
                if let Err(error) = backup.persist_noclobber(path.with_extension("json.corrupt"))
                    && error.error.kind() != io::ErrorKind::AlreadyExists
                {
                    return Err(error.into());
                }
                Store::default()
            }
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => Store::default(),
        Err(error) => return Err(error.into()),
    };
    let before = store.clone();
    change(&mut store);
    if store != before {
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        serde_json::to_writer_pretty(&mut tmp, &store)?;
        persist_store(tmp, path)?;
    }
    Ok(store)
}

fn persist_store(mut tmp: tempfile::NamedTempFile, path: &Path) -> io::Result<()> {
    // Windows can deny the atomic replacement while a reader still has the old file open.
    for _ in 0..20 {
        match tmp.persist(path) {
            Ok(_) => return Ok(()),
            Err(error)
                if cfg!(windows) && error.error.kind() == io::ErrorKind::PermissionDenied =>
            {
                tmp = error.file;
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(error) => return Err(error.error),
        }
    }
    tmp.persist(path).map(|_| ()).map_err(|error| error.error)
}

#[cfg(test)]
mod tests;
