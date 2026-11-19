//! Project registry (persisted as JSON) and the project/worktree operations.

use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sich_proto::{GroupId, GroupInfo, ProjectId, ProjectInfo, ServerMsg, WorktreeInfo};

use crate::git;
use crate::hub::ConnHandle;
use crate::manager::Manager;

#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    id: ProjectId,
    name: String,
    path: PathBuf,
    #[serde(default)]
    group: Option<GroupId>,
}

#[derive(Clone, Serialize, Deserialize)]
struct GroupEntry {
    id: GroupId,
    name: String,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    groups: Vec<GroupEntry>,
    #[serde(default)]
    projects: Vec<Entry>,
    /// Next group id; ids are never reused within the file.
    #[serde(default)]
    next_group_id: GroupId,
}

/// Old files are a bare array of projects; new ones are an object with groups.
#[derive(Deserialize)]
#[serde(untagged)]
enum FileFormat {
    Legacy(Vec<Entry>),
    Current(State),
}

fn parse_state(bytes: &[u8]) -> State {
    let mut st = match serde_json::from_slice::<FileFormat>(bytes) {
        Ok(FileFormat::Legacy(projects)) => State { projects, ..State::default() },
        Ok(FileFormat::Current(st)) => st,
        Err(_) => State::default(),
    };
    let max = st.groups.iter().map(|g| g.id).max().unwrap_or(0);
    st.next_group_id = st.next_group_id.max(max + 1);
    st
}

pub struct Registry {
    file: PathBuf,
    state: Mutex<State>,
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
            crate::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("sich").join("projects.json");
        let state = parse_state(&std::fs::read(&file).unwrap_or_default());
        Self { file, state: Mutex::new(state) }
    }

    fn save(&self, state: &State) {
        let result = (|| -> std::io::Result<()> {
            if let Some(dir) = self.file.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = self.file.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_vec_pretty(state)?)?;
            std::fs::rename(&tmp, &self.file)
        })();
        if let Err(e) = result {
            tracing::warn!("save {}: {e}", self.file.display());
        }
    }

    fn entries(&self) -> Vec<Entry> {
        self.state.lock().projects.clone()
    }

    fn groups(&self) -> Vec<GroupInfo> {
        self.state.lock().groups.iter().map(|g| GroupInfo { id: g.id, name: g.name.clone() }).collect()
    }

    fn get(&self, id: ProjectId) -> Option<Entry> {
        self.state.lock().projects.iter().find(|e| e.id == id).cloned()
    }

    /// Adds a project unless the path is already present. Unknown group -> error, no change.
    fn add(&self, name: String, path: PathBuf, group: Option<GroupId>) -> Result<(), String> {
        let mut st = self.state.lock();
        if let Some(g) = group
            && !st.groups.iter().any(|x| x.id == g)
        {
            return Err("Unknown group".into());
        }
        if st.projects.iter().any(|e| git::same_path(&e.path, &path)) {
            return Ok(());
        }
        let id = st.projects.iter().map(|e| e.id).max().unwrap_or(0) + 1;
        st.projects.push(Entry { id, name, path, group });
        self.save(&st);
        Ok(())
    }

    fn remove(&self, id: ProjectId) {
        let mut st = self.state.lock();
        st.projects.retain(|e| e.id != id);
        self.save(&st);
    }

    fn create_group(&self, name: &str) -> Result<(), String> {
        let mut st = self.state.lock();
        let name = valid_group_name(&st.groups, name, None)?;
        let id = st.next_group_id.max(1);
        st.next_group_id = id + 1;
        st.groups.push(GroupEntry { id, name });
        self.save(&st);
        Ok(())
    }

    fn rename_group(&self, id: GroupId, name: &str) -> Result<(), String> {
        let mut st = self.state.lock();
        if !st.groups.iter().any(|g| g.id == id) {
            return Err("Unknown group".into());
        }
        let name = valid_group_name(&st.groups, name, Some(id))?;
        if let Some(g) = st.groups.iter_mut().find(|g| g.id == id) {
            g.name = name;
        }
        self.save(&st);
        Ok(())
    }

    fn remove_group(&self, id: GroupId) -> Result<(), String> {
        let mut st = self.state.lock();
        if !st.groups.iter().any(|g| g.id == id) {
            return Err("Unknown group".into());
        }
        st.groups.retain(|g| g.id != id);
        for p in st.projects.iter_mut().filter(|p| p.group == Some(id)) {
            p.group = None;
        }
        self.save(&st);
        Ok(())
    }

    fn set_project_group(&self, project_id: ProjectId, group: Option<GroupId>) -> Result<(), String> {
        let mut st = self.state.lock();
        if let Some(g) = group
            && !st.groups.iter().any(|x| x.id == g)
        {
            return Err("Unknown group".into());
        }
        let Some(p) = st.projects.iter_mut().find(|p| p.id == project_id) else {
            return Err("Unknown project".into());
        };
        p.group = group;
        self.save(&st);
        Ok(())
    }
}

/// Trims and checks a group name (non-empty, unique case-insensitively except for `own`).
fn valid_group_name(groups: &[GroupEntry], name: &str, own: Option<GroupId>) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("Group name is empty".into());
    }
    let lower = name.to_lowercase();
    if groups.iter().any(|g| Some(g.id) != own && g.name.to_lowercase() == lower) {
        return Err("A group with this name already exists".into());
    }
    Ok(name.to_string())
}

fn scan_one(e: &Entry) -> ProjectInfo {
    let mut info = ProjectInfo {
        id: e.id,
        name: e.name.clone(),
        path: e.path.clone(),
        is_git: false,
        worktrees: Vec::new(),
        error: None,
        group: e.group,
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

pub async fn list(mgr: &Arc<Manager>) -> ServerMsg {
    ServerMsg::Projects { projects: scan(mgr).await, groups: mgr.projects.groups() }
}

pub async fn broadcast(mgr: &Arc<Manager>) {
    let list = scan(mgr).await;
    mgr.hub.broadcast(ServerMsg::Projects { projects: list, groups: mgr.projects.groups() });
}

fn fail(conn: &ConnHandle, op: &str, message: impl Into<String>) {
    conn.send(ServerMsg::OpFailed { op: op.into(), message: message.into() });
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    tokio::task::spawn_blocking(f).await.ok()
}

pub async fn add_project(mgr: &Arc<Manager>, conn: &ConnHandle, path: PathBuf, group: Option<GroupId>) {
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
            match mgr.projects.add(name, path, group) {
                Ok(()) => broadcast(mgr).await,
                Err(msg) => fail(conn, "add_project", msg),
            }
        }
        Some(Err(msg)) => fail(conn, "add_project", msg),
        None => fail(conn, "add_project", "internal error"),
    }
}

async fn group_result(mgr: &Arc<Manager>, conn: &ConnHandle, op: &str, res: Result<(), String>) {
    match res {
        Ok(()) => broadcast(mgr).await,
        Err(msg) => fail(conn, op, msg),
    }
}

pub async fn create_group(mgr: &Arc<Manager>, conn: &ConnHandle, name: String) {
    let r = mgr.projects.create_group(&name);
    group_result(mgr, conn, "create_group", r).await;
}

pub async fn rename_group(mgr: &Arc<Manager>, conn: &ConnHandle, id: GroupId, name: String) {
    let r = mgr.projects.rename_group(id, &name);
    group_result(mgr, conn, "rename_group", r).await;
}

pub async fn remove_group(mgr: &Arc<Manager>, conn: &ConnHandle, id: GroupId) {
    let r = mgr.projects.remove_group(id);
    group_result(mgr, conn, "remove_group", r).await;
}

pub async fn set_project_group(
    mgr: &Arc<Manager>,
    conn: &ConnHandle,
    project_id: ProjectId,
    group: Option<GroupId>,
) {
    let r = mgr.projects.set_project_group(project_id, group);
    group_result(mgr, conn, "set_project_group", r).await;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_legacy_array_file() {
        let st = parse_state(br#"[{"id":3,"name":"a","path":"/a"},{"id":5,"name":"b","path":"/b"}]"#);
        assert_eq!(st.projects.len(), 2);
        assert!(st.projects.iter().all(|p| p.group.is_none()));
        assert!(st.groups.is_empty());
        assert_eq!(st.next_group_id, 1);
    }

    #[test]
    fn loads_object_without_groups_or_group_field() {
        let st = parse_state(br#"{"projects":[{"id":1,"name":"a","path":"/a"}]}"#);
        assert_eq!(st.projects.len(), 1);
        assert!(st.groups.is_empty());
    }

    #[test]
    fn next_group_id_never_below_existing() {
        let st = parse_state(br#"{"groups":[{"id":4,"name":"x"}],"next_group_id":2}"#);
        assert_eq!(st.next_group_id, 5);
    }

    #[test]
    fn garbage_file_loads_empty() {
        let st = parse_state(b"not json");
        assert!(st.projects.is_empty() && st.groups.is_empty());
    }

    #[test]
    fn group_name_validation() {
        let groups = vec![GroupEntry { id: 1, name: "Game".into() }];
        assert!(valid_group_name(&groups, "  ", None).is_err());
        assert!(valid_group_name(&groups, "game", None).is_err());
        assert_eq!(valid_group_name(&groups, " web ", None).unwrap(), "web");
        assert_eq!(valid_group_name(&groups, "GAME", Some(1)).unwrap(), "GAME");
    }
}
