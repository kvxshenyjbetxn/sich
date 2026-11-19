//! Project registry (persisted as JSON) and the project/worktree operations.

use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sich_proto::{ProjectId, ProjectInfo, ServerMsg, WorktreeInfo};

use crate::git;
use crate::hub::ConnHandle;
use crate::manager::Manager;

#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    id: ProjectId,
    name: String,
    path: PathBuf,
}

pub struct Registry {
    file: PathBuf,
    items: Mutex<Vec<Entry>>,
}

fn config_dir() -> Option<PathBuf> {
    match std::env::var_os("SICH_CONFIG_DIR") {
        Some(d) if !d.is_empty() => Some(PathBuf::from(d)),
        _ => dirs::config_dir(),
    }
}

fn worktree_root() -> PathBuf {
    match std::env::var_os("SICH_WORKTREE_ROOT") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join("sich").join("worktrees"),
    }
}

impl Registry {
    pub fn load() -> Self {
        let file =
            config_dir().unwrap_or_else(|| PathBuf::from(".")).join("sich").join("projects.json");
        let items = std::fs::read(&file)
            .ok()
            .and_then(|b| serde_json::from_slice::<Vec<Entry>>(&b).ok())
            .unwrap_or_default();
        Self { file, items: Mutex::new(items) }
    }

    fn save(&self, items: &[Entry]) {
        let result = (|| -> std::io::Result<()> {
            if let Some(dir) = self.file.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = self.file.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_vec_pretty(items)?)?;
            std::fs::rename(&tmp, &self.file)
        })();
        if let Err(e) = result {
            tracing::warn!("save {}: {e}", self.file.display());
        }
    }

    fn entries(&self) -> Vec<Entry> {
        self.items.lock().clone()
    }

    fn get(&self, id: ProjectId) -> Option<Entry> {
        self.items.lock().iter().find(|e| e.id == id).cloned()
    }

    /// Adds a project unless the path is already present.
    fn add(&self, name: String, path: PathBuf) {
        let mut items = self.items.lock();
        if items.iter().any(|e| git::same_path(&e.path, &path)) {
            return;
        }
        let id = items.iter().map(|e| e.id).max().unwrap_or(0) + 1;
        items.push(Entry { id, name, path });
        self.save(&items);
    }

    fn remove(&self, id: ProjectId) {
        let mut items = self.items.lock();
        items.retain(|e| e.id != id);
        self.save(&items);
    }
}

fn scan_one(e: &Entry) -> ProjectInfo {
    let mut info = ProjectInfo {
        id: e.id,
        name: e.name.clone(),
        path: e.path.clone(),
        is_git: false,
        worktrees: Vec::new(),
        error: None,
    };
    if !e.path.is_dir() {
        info.error = Some("Folder not found".into());
        return info;
    }
    if git::toplevel(&e.path).is_some() {
        info.is_git = true;
        match git::list_worktrees(&e.path) {
            Ok(w) => info.worktrees = w,
            Err(msg) => info.error = Some(msg),
        }
    } else {
        info.worktrees.push(WorktreeInfo {
            path: e.path.clone(),
            branch: None,
            head: String::new(),
            is_main: true,
            dirty: false,
        });
    }
    info
}

async fn scan(mgr: &Arc<Manager>) -> Vec<ProjectInfo> {
    let entries = mgr.projects.entries();
    tokio::task::spawn_blocking(move || entries.iter().map(scan_one).collect())
        .await
        .unwrap_or_default()
}

pub async fn list(mgr: &Arc<Manager>) -> Vec<ProjectInfo> {
    scan(mgr).await
}

pub async fn broadcast(mgr: &Arc<Manager>) {
    let list = scan(mgr).await;
    mgr.hub.broadcast(ServerMsg::Projects(list));
}

fn fail(conn: &ConnHandle, op: &str, message: impl Into<String>) {
    conn.send(ServerMsg::OpFailed { op: op.into(), message: message.into() });
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    tokio::task::spawn_blocking(f).await.ok()
}

pub async fn add_project(mgr: &Arc<Manager>, conn: &ConnHandle, path: PathBuf) {
    let found = blocking(move || {
        let canon = dunce::canonicalize(&path).map_err(|_| "Folder not found".to_string())?;
        if !canon.is_dir() {
            return Err("Not a folder".to_string());
        }
        Ok(git::toplevel(&canon).unwrap_or(canon))
    })
    .await;
    match found {
        Some(Ok(path)) => {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| path.to_string_lossy().to_string());
            mgr.projects.add(name, path);
            broadcast(mgr).await;
        }
        Some(Err(msg)) => fail(conn, "add_project", msg),
        None => fail(conn, "add_project", "internal error"),
    }
}

pub async fn remove_project(mgr: &Arc<Manager>, id: ProjectId) {
    mgr.projects.remove(id);
    broadcast(mgr).await;
}

pub async fn create_worktree(
    mgr: &Arc<Manager>,
    conn: &ConnHandle,
    project_id: ProjectId,
    name: String,
    base: Option<String>,
) {
    const OP: &str = "create_worktree";
    let Some(entry) = mgr.projects.get(project_id) else {
        return fail(conn, OP, "Unknown project");
    };
    let root = worktree_root();
    let res = blocking(move || -> Result<(), String> {
        if git::toplevel(&entry.path).is_none() {
            return Err("Project is not a git repository".into());
        }
        git::validate_name(&entry.path, &name)?;
        let repo_name = entry.path.file_name().map(PathBuf::from).unwrap_or_else(|| "repo".into());
        let target = root.join(repo_name).join(&name);
        if target.exists() {
            return Err(format!("Folder already exists: {}", target.display()));
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("create folder: {e}"))?;
        }
        git::add_worktree(&entry.path, &target, &name, base.as_deref())
    })
    .await;
    match res {
        Some(Ok(())) => broadcast(mgr).await,
        Some(Err(msg)) => fail(conn, OP, msg),
        None => fail(conn, OP, "internal error"),
    }
}

pub async fn remove_worktree(
    mgr: &Arc<Manager>,
    conn: &ConnHandle,
    project_id: ProjectId,
    path: PathBuf,
    force: bool,
    delete_branch: bool,
) {
    const OP: &str = "remove_worktree";
    let Some(entry) = mgr.projects.get(project_id) else {
        return fail(conn, OP, "Unknown project");
    };
    let (repo, wt) = (entry.path.clone(), path.clone());
    let info = blocking(move || {
        let list = git::list_worktrees(&repo).ok()?;
        list.into_iter().find(|w| git::same_path(&w.path, &wt))
    })
    .await
    .flatten();
    if info.as_ref().is_some_and(|w| w.is_main) || git::same_path(&entry.path, &path) {
        return fail(conn, OP, "Cannot remove the main worktree");
    }

    // Refuse before killing terminals, so a failed non-forced removal loses nothing.
    if !force && info.as_ref().is_some_and(|w| w.dirty) {
        return fail(conn, OP, "Worktree has uncommitted changes");
    }

    mgr.kill_in_worktree(&path);

    let branch = info.and_then(|w| w.branch);
    let (repo, wt) = (entry.path.clone(), path.clone());
    let res = blocking(move || -> Result<(), String> {
        git::remove_worktree(&repo, &wt, force)?;
        match branch {
            Some(b) if delete_branch => git::delete_branch(&repo, &b, force),
            _ => Ok(()),
        }
    })
    .await;
    match res {
        Some(Err(msg)) => fail(conn, OP, msg),
        Some(Ok(())) => {}
        None => fail(conn, OP, "internal error"),
    }
    broadcast(mgr).await;
}
