//! UI-side handle to the daemon: shared state (terminals, connection status) plus commands.
//!
//! The connection itself lives in `daemon_conn` (a tokio task with auto-reconnect).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;
use sich_proto::{GroupId, GroupInfo, ProjectId, ProjectInfo, TermId, TerminalInfo};

use sich_term::TermSession;
use tokio::runtime::Handle;
use tokio::sync::mpsc;

use crate::daemon_conn;

/// Wakes the UI thread (must be cheap and non-blocking).
pub type Wake = Arc<dyn Fn() + Send + Sync>;

/// Size used for terminals when no view size is known yet.
pub const DEFAULT_COLS: u16 = 120;
pub const DEFAULT_ROWS: u16 = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnStatus {
    Connected { pid: u32 },
    Reconnecting,
    /// The running daemon speaks another protocol; no reconnecting until the user restarts it.
    /// `pid`/`version` are unknown for an old v1 daemon (it only sends `Error`).
    Incompatible { pid: Option<u32>, version: Option<u32> },
}

#[derive(Clone)]
pub struct TermEntry {
    pub info: TerminalInfo,
    pub session: TermSession,
}

pub(crate) struct ClientState {
    pub status: ConnStatus,
    pub terminals: Vec<TermEntry>,
    /// Set once the first terminal list arrived (used for the "create one if empty" rule).
    pub synced: bool,
    /// How many terminals the daemon had in the first list after startup (`None` = not yet).
    pub first_list_count: Option<usize>,
    /// Create a Home terminal when the daemon starts out empty (off while a session restore
    /// is going to fill it).
    pub auto_create: bool,
    pub daemon_pid: Option<u32>,
    pub projects: Vec<ProjectInfo>,
    pub groups: Vec<GroupInfo>,
    /// Incremented on every `Projects` message (0 = none received yet).
    pub projects_rev: u64,
    /// Last `OpFailed` from the daemon: (op, message, when).
    pub last_error: Option<(String, String, Instant)>,
}

/// Commands from the UI to the connection task.
pub(crate) enum Cmd {
    /// Leave the `Incompatible` state and connect again.
    Reconnect,
    Create { cols: u16, rows: u16, worktree: Option<PathBuf>, cwd: Option<PathBuf> },
    RefreshProjects,
    AddProject { path: PathBuf, group: Option<GroupId> },
    CreateGroup(String),
    RenameGroup(GroupId, String),
    RemoveGroup(GroupId),
    SetProjectGroup { project_id: ProjectId, group: Option<GroupId> },
    RemoveProject(ProjectId),
    CreateWorktree { project_id: ProjectId, name: String, base: Option<String> },
    RemoveWorktree { project_id: ProjectId, path: PathBuf, force: bool, delete_branch: bool },
    Kill(TermId),
    MarkSeen(TermId),
    Input(TermId, Vec<u8>),
    Resize(TermId, u16, u16),
}

pub struct Snapshot {
    pub status: ConnStatus,
    pub terminals: Vec<TermEntry>,
}

#[derive(Clone)]
pub struct DaemonClient {
    cmd_tx: mpsc::UnboundedSender<Cmd>,
    state: Arc<Mutex<ClientState>>,
    rt: Handle,
    socket: String,
    wake: Wake,
}

impl DaemonClient {
    /// Starts the connection task on `rt`.
    pub fn start(rt: &Handle, socket: String, wake: Wake, auto_create: bool) -> DaemonClient {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let state = Arc::new(Mutex::new(ClientState {
            status: ConnStatus::Reconnecting,
            terminals: Vec::new(),
            synced: false,
            first_list_count: None,
            auto_create,
            daemon_pid: None,
            projects: Vec::new(),
            groups: Vec::new(),
            projects_rev: 0,
            last_error: None,
        }));
        let conn = daemon_conn::Conn {
            socket: socket.clone(),
            rt: rt.clone(),
            state: state.clone(),
            cmd_tx: cmd_tx.clone(),
            wake: wake.clone(),
        };
        rt.spawn(daemon_conn::run(conn, cmd_rx));
        DaemonClient { cmd_tx, state, rt: rt.clone(), socket, wake }
    }

    pub fn snapshot(&self) -> Snapshot {
        let st = self.state.lock();
        Snapshot { status: st.status, terminals: st.terminals.clone() }
    }

    /// Creates a terminal whose cwd is `worktree` (when given).
    pub fn create_terminal_in(&self, cols: u16, rows: u16, worktree: Option<PathBuf>) {
        let _ = self.cmd_tx.send(Cmd::Create { cols, rows, worktree, cwd: None });
    }

    /// Like [`create_terminal_in`](Self::create_terminal_in) but starting in `cwd` (session restore).
    pub fn create_terminal_at(&self, cols: u16, rows: u16, worktree: Option<PathBuf>, cwd: Option<PathBuf>) {
        let _ = self.cmd_tx.send(Cmd::Create { cols, rows, worktree, cwd });
    }

    /// Terminal count of the first list received after startup; `None` until it arrives.
    pub fn first_list_count(&self) -> Option<usize> {
        self.state.lock().first_list_count
    }

    /// Projects plus the revision counter (changes on every `Projects` message).
    pub fn projects_with_rev(&self) -> (u64, Vec<ProjectInfo>, Vec<GroupInfo>) {
        let st = self.state.lock();
        (st.projects_rev, st.projects.clone(), st.groups.clone())
    }

    /// Kills the incompatible daemon (by `pid`, else by scanning processes), starts a new one
    /// and resumes connecting.
    pub fn restart_daemon(&self, pid: Option<u32>) {
        let tx = self.cmd_tx.clone();
        let state = self.state.clone();
        let wake = self.wake.clone();
        let socket = self.socket.clone();
        self.rt.spawn(async move {
            if let Err(e) = crate::daemon_kill::restart(&socket, pid).await {
                tracing::error!("restart daemon failed: {e:#}");
                state.lock().last_error =
                    Some(("restart daemon".into(), format!("{e:#}"), Instant::now()));
            }
            let _ = tx.send(Cmd::Reconnect);
            wake();
        });
    }

    pub fn last_error(&self) -> Option<(String, String, Instant)> {
        self.state.lock().last_error.clone()
    }

    pub fn refresh_projects(&self) {
        let _ = self.cmd_tx.send(Cmd::RefreshProjects);
    }

    pub fn add_project(&self, path: PathBuf, group: Option<GroupId>) {
        let _ = self.cmd_tx.send(Cmd::AddProject { path, group });
    }

    pub fn create_group(&self, name: String) {
        let _ = self.cmd_tx.send(Cmd::CreateGroup(name));
    }

    pub fn rename_group(&self, id: GroupId, name: String) {
        let _ = self.cmd_tx.send(Cmd::RenameGroup(id, name));
    }

    pub fn remove_group(&self, id: GroupId) {
        let _ = self.cmd_tx.send(Cmd::RemoveGroup(id));
    }

    pub fn set_project_group(&self, project_id: ProjectId, group: Option<GroupId>) {
        let _ = self.cmd_tx.send(Cmd::SetProjectGroup { project_id, group });
    }

    pub fn remove_project(&self, id: ProjectId) {
        let _ = self.cmd_tx.send(Cmd::RemoveProject(id));
    }

    pub fn create_worktree(&self, project_id: ProjectId, name: String, base: Option<String>) {
        let _ = self.cmd_tx.send(Cmd::CreateWorktree { project_id, name, base });
    }

    pub fn remove_worktree(
        &self,
        project_id: ProjectId,
        path: PathBuf,
        force: bool,
        delete_branch: bool,
    ) {
        let _ = self.cmd_tx.send(Cmd::RemoveWorktree { project_id, path, force, delete_branch });
    }

    pub fn kill_terminal(&self, id: TermId) {
        let _ = self.cmd_tx.send(Cmd::Kill(id));
    }

    /// Tells the daemon the user has looked at the terminal (clears its `unread`).
    pub fn mark_seen(&self, id: TermId) {
        let _ = self.cmd_tx.send(Cmd::MarkSeen(id));
    }
}
