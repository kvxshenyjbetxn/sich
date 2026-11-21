//! UI-side handle to the daemon: shared state (terminals, connection status) plus commands.
//!
//! The connection itself lives in `daemon_conn` (a tokio task with auto-reconnect).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::{SinkExt as _, StreamExt as _};

use parking_lot::Mutex;
use sich_daemon::generation;
use sich_proto::{ClientMsg, GroupId, GroupInfo, PROTO_VERSION, ProjectId, ProjectInfo, ServerMsg, TermId, TerminalInfo, decode, encode, frame_codec};

use sich_term::TermSession;
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::codec::Framed;

use crate::{daemon_conn, daemon_restart};
use crate::session::Session;
use crate::workspace::PendingWorktree;

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
    pub restore_token: Option<u64>,
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
    pub daemon_build: Option<String>,
    pub restart_result: Option<RestartResult>,
    pub projects: Vec<ProjectInfo>,
    pub groups: Vec<GroupInfo>,
    /// Incremented on every `Projects` message (0 = none received yet).
    pub projects_rev: u64,
    pub pending_worktrees: VecDeque<PendingWorktree>,
    pub created_worktrees: Vec<(PathBuf, PathBuf)>,
    /// Last `OpFailed` from the daemon: (op, message, when).
    pub last_error: Option<(String, String, Instant)>,
    pub failed_splits: Vec<u64>,
}

/// Commands from the UI to the connection task.
pub(crate) enum Cmd {
    Restart(daemon_restart::Request),
    Restore { worktree: Option<PathBuf>, cwd: Option<PathBuf>, cols: u16, rows: u16, data: RestoreData },
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
    Resume(Arc<()>, TermId, Vec<u8>),
    Resize(TermId, u16, u16),
}

pub(crate) struct RestoreData {
    pub token: u64,
    pub scrollback: Option<String>,
    pub separator: &'static str,
    pub interruption: Option<&'static str>,
    pub resume: Option<String>,
    pub split: bool,
}

impl RestoreData {
    pub(crate) fn fresh_split(token: u64) -> Self {
        Self { token, scrollback: None, separator: "", interruption: None, resume: None, split: true }
    }
}

pub(crate) enum RestartResult {
    Stopped(Session),
    Failed,
}

pub struct Snapshot {
    pub status: ConnStatus,
    pub terminals: Vec<TermEntry>,
    pub legacy_terminals: bool,
}

pub(crate) struct ProjectsUpdate {
    pub revision: u64,
    pub projects: Vec<ProjectInfo>,
    pub groups: Vec<GroupInfo>,
    pub created_worktrees: Vec<(PathBuf, PathBuf)>,
}

/// On the wire, terminal IDs are local to their endpoint and process. The UI uses
/// monotonically assigned keys, so stale views cannot act on a terminal that reused an ID.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct TerminalOwner {
    pub socket: String,
    pub id: TermId,
    pub pid: u32,
    pub created_ms: u64,
}

#[derive(Default)]
struct TerminalKeys {
    next: TermId,
    by_owner: HashMap<TerminalOwner, TermId>,
}

impl TerminalKeys {
    fn key(&mut self, owner: TerminalOwner) -> TermId {
        *self.by_owner.entry(owner).or_insert_with(|| {
            self.next += 1;
            self.next
        })
    }

    fn owner(&self, key: TermId) -> Option<&TerminalOwner> {
        self.by_owner.iter().find_map(|(owner, id)| (*id == key).then_some(owner))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Endpoint {
    socket: String,
    incompatible: bool,
    omp_status: bool,
    subagent_models: bool,
}

fn legacy_endpoints(base: &str, primary: &str, entries: &[generation::Entry]) -> Vec<Endpoint> {
    entries.iter()
        .filter(|entry| entry.base() == Some(base) && entry.socket != primary)
        .map(|entry| Endpoint {
            socket: entry.socket.clone(),
            incompatible: entry.proto_version != PROTO_VERSION,
            omp_status: entry.omp_status,
            subagent_models: entry.subagent_models,
        })
        .collect()
}

/// The previous release listened directly on the base. Probe it once per GUI startup.
async fn probe_legacy(base: &str) -> Option<Endpoint> {
    tokio::time::timeout(Duration::from_secs(1), async {
        let mut stream = Framed::new(sich_daemon::connect(base).await.ok()?, frame_codec());
        stream.send(encode(&ClientMsg::Hello { proto_version: PROTO_VERSION, client_name: "sich-app-discover".into() }).ok()?).await.ok()?;
        let msg: ServerMsg = decode(&stream.next().await?.ok()?).ok()?;
        if matches!(msg, ServerMsg::Welcome { .. }) || daemon_conn::classify_incompatible(&msg, false).is_some() {
            Some(Endpoint {
                socket: base.into(),
                incompatible: daemon_conn::classify_incompatible(&msg, false).is_some(),
                omp_status: false,
                subagent_models: false,
            })
        } else {
            None
        }
    }).await.ok().flatten()
}

struct Peer {
    endpoint: Endpoint,
    cmd_tx: mpsc::UnboundedSender<Cmd>,
    state: Arc<Mutex<ClientState>>,
    task: JoinHandle<()>,
}

impl Peer {
    fn start(rt: &Handle, endpoint: Endpoint, wake: Wake, legacy: bool) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let state = Arc::new(Mutex::new(ClientState {
            status: ConnStatus::Reconnecting,
            terminals: Vec::new(),
            synced: false,
            first_list_count: None,
            auto_create: false,
            daemon_pid: None,
            daemon_build: None,
            restart_result: None,
            projects: Vec::new(),
            groups: Vec::new(),
            projects_rev: 0,
            pending_worktrees: VecDeque::new(),
            created_worktrees: Vec::new(),
            last_error: None,
            failed_splits: Vec::new(),
        }));
        let name = if legacy { generation::RETIRE_CLIENT } else { "sich-app" };
        let models = if endpoint.subagent_models { sich_proto::SUBAGENT_MODELS_CLIENT_SUFFIX } else { "" };
        let suffix = if endpoint.omp_status { sich_proto::OMP_CLIENT_SUFFIX } else { "" };
        let conn = daemon_conn::Conn {
            socket: endpoint.socket.clone(),
            client_name: format!("{name}{models}{suffix}"),
            rt: rt.clone(),
            state: state.clone(),
            cmd_tx: cmd_tx.clone(),
            wake: wake.clone(),
        };
        let task = rt.spawn(async move {
            daemon_conn::run(conn, cmd_rx, legacy).await;
            wake();
        });
        Self { endpoint, cmd_tx, state, task }
    }

    fn incompatible(&self) -> Option<ConnStatus> {
        if self.task.is_finished() {
            return None;
        }
        let status = self.state.lock().status;
        matches!(status, ConnStatus::Incompatible { .. }).then_some(status)
    }

    fn blocks_spawn(&self) -> bool {
        self.incompatible().is_some()
            || (self.endpoint.incompatible && !self.task.is_finished()
                && self.state.lock().status == ConnStatus::Reconnecting)
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Pool {
    base: String,
    primary: Peer,
    legacy: Mutex<Vec<Peer>>,
    keys: Mutex<TerminalKeys>,
    discovered: std::sync::atomic::AtomicBool,
    exiting: std::sync::atomic::AtomicBool,
}

#[derive(Clone)]
pub struct DaemonPool(Arc<Pool>);

impl DaemonPool {
    pub fn start(rt: &Handle, base: String, wake: Wake) -> Self {
        Self::start_on(rt, base, sich_app::daemon_build_id(), wake, true)
    }

    fn start_on(rt: &Handle, base: String, build: &str, wake: Wake, spawn_primary: bool) -> Self {
        let socket = generation::socket(&base, build);
        let primary = Peer::start(rt, Endpoint {
            socket, incompatible: false, omp_status: true, subagent_models: true,
        }, wake.clone(), false);
        let pool = Arc::new(Pool {
            base, primary, legacy: Mutex::new(Vec::new()), keys: Mutex::default(),
            discovered: false.into(), exiting: false.into(),
        });
        let weak = Arc::downgrade(&pool);
        let rt_handle = rt.clone();
        rt.spawn(async move {
            let Some(pool) = weak.upgrade() else { return };
            let legacy = probe_legacy(&pool.base).await;
            if let Some(endpoint) = legacy {
                pool.legacy.lock().push(Peer::start(&rt_handle, endpoint, wake.clone(), true));
            }
            drop(pool);
            let mut tick = tokio::time::interval(Duration::from_millis(500));
            loop {
                tick.tick().await;
                let Some(pool) = weak.upgrade() else { break };
                if pool.exiting.load(std::sync::atomic::Ordering::Acquire) {
                    break;
                }
                let endpoints = legacy_endpoints(&pool.base, &pool.primary.endpoint.socket, &generation::entries());
                let blocked = {
                    let mut legacy = pool.legacy.lock();
                    legacy.retain(|peer| !peer.task.is_finished() || peer.state.lock().restart_result.is_some());
                    for endpoint in endpoints {
                        if !legacy.iter().any(|peer| peer.endpoint.socket == endpoint.socket) {
                            legacy.push(Peer::start(&rt_handle, endpoint, wake.clone(), true));
                        }
                    }
                    legacy.iter().any(Peer::blocks_spawn)
                };
                pool.discovered.store(true, std::sync::atomic::Ordering::Release);
                wake();
                if spawn_primary && !blocked && pool.primary.state.lock().status == ConnStatus::Reconnecting {
                    if let Err(err) = crate::cli::ensure_daemon(&pool.base).await {
                        tracing::warn!("could not start daemon: {err:#}");
                    }
                }
            }
        });
        Self(pool)
    }

    pub fn snapshot(&self) -> Snapshot {
        let legacy = self.0.legacy.lock();
        let status = legacy.iter().find_map(Peer::incompatible)
            .unwrap_or_else(|| self.0.primary.state.lock().status);
        let mut terminals = Vec::new();
        let mut keys = self.0.keys.lock();
        let mut present = HashSet::new();
        let mut legacy_terminals = false;
        for peer in std::iter::once(&self.0.primary).chain(legacy.iter()) {
            if peer.task.is_finished() {
                continue;
            }
            let st = peer.state.lock();
            let Some(pid) = st.daemon_pid else { continue };
            legacy_terminals |= peer.endpoint.socket != self.0.primary.endpoint.socket && !st.terminals.is_empty();
            for entry in &st.terminals {
                let owner = TerminalOwner { socket: peer.endpoint.socket.clone(), id: entry.info.id, pid, created_ms: entry.info.created_unix_ms };
                let mut entry = entry.clone();
                entry.info.id = keys.key(owner.clone());
                present.insert(owner);
                terminals.push(entry);
            }
        }
        keys.by_owner.retain(|owner, _| present.contains(owner));
        Snapshot { status, terminals, legacy_terminals }
    }

    pub(crate) fn owner(&self, key: TermId) -> Option<TerminalOwner> {
        self.0.keys.lock().owner(key).cloned()
    }

    pub(crate) fn primary_socket(&self) -> &str {
        &self.0.primary.endpoint.socket
    }

    pub(crate) fn connected_peers(&self) -> Vec<(String, u32)> {
        let legacy = self.0.legacy.lock();
        std::iter::once(&self.0.primary).chain(legacy.iter())
            .filter(|peer| !peer.task.is_finished())
            .filter_map(|peer| match peer.state.lock().status {
                ConnStatus::Connected { pid } => Some((peer.endpoint.socket.clone(), pid)),
                _ => None,
            })
            .collect()
    }

    pub(crate) fn shutdown_all(&self) {
        self.0.exiting.store(true, std::sync::atomic::Ordering::Release);
        let peers = self.connected_peers();
        let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
        rt.block_on(async {
            for (socket, pid) in peers {
                let request = daemon_restart::Request { expected: ConnStatus::Connected { pid }, session: None };
                if let Err(err) = request.save_and_shutdown(&socket).await {
                    tracing::warn!("could not stop daemon {socket}: {err:#}");
                }
            }
        });
    }

    fn send_to_owner(&self, key: TermId, cmd: impl FnOnce(TermId) -> Cmd) {
        let Some(owner) = self.owner(key) else { return };
        let legacy = self.0.legacy.lock();
        let peer = std::iter::once(&self.0.primary).chain(legacy.iter())
            .find(|p| p.endpoint.socket == owner.socket && !p.task.is_finished());
        if let Some(peer) = peer {
            let state = peer.state.lock();
            if state.daemon_pid == Some(owner.pid)
                && state.terminals.iter().any(|t| t.info.id == owner.id && t.info.created_unix_ms == owner.created_ms)
            {
                let _ = peer.cmd_tx.send(cmd(owner.id));
            }
        }
    }

    /// Creates a terminal whose cwd is `worktree` (when given).
    pub fn create_terminal_in(&self, cols: u16, rows: u16, worktree: Option<PathBuf>) {
        let _ = self.0.primary.cmd_tx.send(Cmd::Create { cols, rows, worktree, cwd: None });
    }

    pub fn restore_terminal(&self, worktree: Option<PathBuf>, cwd: Option<PathBuf>, data: RestoreData) {
        let _ = self.0.primary.cmd_tx.send(Cmd::Restore {
            worktree, cwd, cols: DEFAULT_COLS, rows: DEFAULT_ROWS, data,
        });
    }

    pub(crate) fn open_split(&self, worktree: Option<PathBuf>, cwd: Option<PathBuf>, size: (u16, u16), token: u64) -> bool {
        let state = self.0.primary.state.lock();
        let data = RestoreData::fresh_split(token);
        matches!(state.status, ConnStatus::Connected { .. })
            && self.0.primary.cmd_tx.send(Cmd::Restore { worktree, cwd, cols: size.0, rows: size.1, data }).is_ok()
    }

    pub(crate) fn take_failed_splits(&self) -> Vec<u64> {
        std::mem::take(&mut self.0.primary.state.lock().failed_splits)
    }

    pub fn daemon_pid(&self) -> Option<u32> {
        self.0.primary.state.lock().daemon_pid
    }

    /// Terminal count of the first list received after startup; `None` until it arrives.
    pub fn first_list_count(&self) -> Option<usize> {
        if !self.terminals_synced() {
            return None;
        }
        let legacy = self.0.legacy.lock();
        let count = |peer: &Peer| {
            peer.state.lock().first_list_count.unwrap_or_default()
        };
        Some(count(&self.0.primary) + legacy.iter().filter(|p| !p.task.is_finished()).map(count).sum::<usize>())
    }

    /// Takes newly confirmed worktrees along with the latest Projects snapshot.
    pub(crate) fn projects_since(&self, revision: u64) -> Option<ProjectsUpdate> {
        let mut st = self.0.primary.state.lock();
        (st.projects_rev != revision).then(|| ProjectsUpdate {
            revision: st.projects_rev,
            projects: st.projects.clone(),
            groups: st.groups.clone(),
            created_worktrees: std::mem::take(&mut st.created_worktrees),
        })
    }

    pub fn restart_daemon(&self, request: daemon_restart::Request) {
        let legacy = self.0.legacy.lock();
        let peer = legacy.iter().find(|peer| peer.incompatible().is_some()).unwrap_or(&self.0.primary);
        let _ = peer.cmd_tx.send(Cmd::Restart(request));
    }

    pub fn take_restart_result(&self) -> Option<RestartResult> {
        let primary = self.0.primary.state.lock().restart_result.take();
        primary.or_else(|| {
            self.0.legacy.lock().iter().find_map(|p| p.state.lock().restart_result.take())
        })
    }

    pub fn last_error(&self) -> Option<(String, String, Instant)> {
        let legacy = self.0.legacy.lock();
        std::iter::once(&self.0.primary).chain(legacy.iter())
            .filter_map(|p| p.state.lock().last_error.clone()).max_by_key(|(_, _, at)| *at)
    }

    pub fn refresh_projects(&self) {
        let _ = self.0.primary.cmd_tx.send(Cmd::RefreshProjects);
    }

    pub fn add_project(&self, path: PathBuf, group: Option<GroupId>) {
        let _ = self.0.primary.cmd_tx.send(Cmd::AddProject { path, group });
    }

    pub fn create_group(&self, name: String) {
        let _ = self.0.primary.cmd_tx.send(Cmd::CreateGroup(name));
    }

    pub fn rename_group(&self, id: GroupId, name: String) {
        let _ = self.0.primary.cmd_tx.send(Cmd::RenameGroup(id, name));
    }

    pub fn remove_group(&self, id: GroupId) {
        let _ = self.0.primary.cmd_tx.send(Cmd::RemoveGroup(id));
    }

    pub fn set_project_group(&self, project_id: ProjectId, group: Option<GroupId>) {
        let _ = self.0.primary.cmd_tx.send(Cmd::SetProjectGroup { project_id, group });
    }

    pub fn remove_project(&self, id: ProjectId) {
        let _ = self.0.primary.cmd_tx.send(Cmd::RemoveProject(id));
    }

    pub fn create_worktree(&self, project_id: ProjectId, name: String, base: Option<String>) {
        let _ = self.0.primary.cmd_tx.send(Cmd::CreateWorktree { project_id, name, base });
    }

    pub fn remove_worktree(
        &self,
        project_id: ProjectId,
        path: PathBuf,
        force: bool,
        delete_branch: bool,
    ) {
        let _ = self.0.primary.cmd_tx.send(Cmd::RemoveWorktree { project_id, path, force, delete_branch });
    }

    pub fn kill_terminal(&self, id: TermId) {
        self.send_to_owner(id, Cmd::Kill);
    }

    /// Types bytes into a terminal (orchestration pointers).
    pub fn send_input(&self, id: TermId, bytes: Vec<u8>) {
        self.send_to_owner(id, |id| Cmd::Input(id, bytes));
    }

    /// Whether the first terminal list arrived (the list is complete from then on).
    pub fn terminals_synced(&self) -> bool {
        let synced = |peer: &Peer| {
            let st = peer.state.lock();
            st.synced && matches!(st.status, ConnStatus::Connected { .. })
        };
        self.0.discovered.load(std::sync::atomic::Ordering::Acquire)
            && synced(&self.0.primary)
            && self.0.legacy.lock().iter().all(|p| p.task.is_finished() || synced(p))
    }

    /// Tells the daemon the user has looked at the terminal (clears its `unread`).
    pub fn mark_seen(&self, id: TermId) {
        self.send_to_owner(id, Cmd::MarkSeen);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner(socket: &str, id: TermId, pid: u32, created_ms: u64) -> TerminalOwner {
        TerminalOwner { socket: socket.into(), id, pid, created_ms }
    }

    fn entry(base: &str, build: &str, proto_version: u32) -> generation::Entry {
        generation::Entry {
            pid: 1,
            socket: generation::socket(base, build),
            daemon_build_id: build.into(),
            proto_version,
            started_ms: 0,
            omp_status: false,
            subagent_activity: false,
            subagent_models: false,
        }
    }

    #[test]
    fn same_wire_id_on_two_daemons_gets_two_keys() {
        let mut keys = TerminalKeys::default();
        let a = keys.key(owner("base-a", 1, 10, 100));
        let b = keys.key(owner("base-b", 1, 20, 100));
        assert_ne!(a, b);
        assert_eq!(keys.key(owner("base-a", 1, 10, 100)), a, "keys are stable");
        assert_eq!(keys.owner(b).map(|o| o.socket.as_str()), Some("base-b"));
        assert!(keys.owner(999).is_none());
    }

    #[test]
    fn a_reused_wire_id_after_a_daemon_restart_gets_a_new_key() {
        let mut keys = TerminalKeys::default();
        let before = keys.key(owner("base-a", 1, 10, 100));
        let after_restart = keys.key(owner("base-a", 1, 11, 100));
        let reused_in_place = keys.key(owner("base-a", 1, 10, 200));
        assert_ne!(before, after_restart);
        assert_ne!(before, reused_in_place);
    }

    #[test]
    fn legacy_endpoints_are_other_generations_of_the_same_base() {
        let entries = [
            entry("base", "new", PROTO_VERSION),
            entry("base", "old", PROTO_VERSION),
            entry("base", "future", PROTO_VERSION + 1),
            entry("other", "old", PROTO_VERSION),
        ];
        assert_eq!(
            legacy_endpoints("base", "base-new", &entries),
            vec![
                Endpoint { socket: "base-old".into(), incompatible: false, omp_status: false, subagent_models: false },
                Endpoint { socket: "base-future".into(), incompatible: true, omp_status: false, subagent_models: false },
            ],
        );
        assert!(legacy_endpoints("base", "base-new", &[]).is_empty());
    }

    fn unique_base() -> String {
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        format!("sich-pool-test-{}-{stamp}", std::process::id())
    }

    /// Isolates the in-process daemons from the user's real config and data directories.
    fn isolate() {
        static DIR: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
        DIR.get_or_init(|| {
            let dir = tempfile::tempdir().unwrap();
            // SAFETY: only this test reads these overrides; they are set once before it starts a daemon.
            unsafe {
                std::env::set_var("SICH_CONFIG_DIR", dir.path().join("config"));
                std::env::set_var("SICH_DATA_DIR", dir.path().join("data"));
                std::env::set_var("SICH_WORKTREE_ROOT", dir.path().join("worktrees"));
            }
            dir
        });
    }

    async fn raw_create(socket: &str) {
        let mut conn = Framed::new(sich_daemon::connect(socket).await.unwrap(), frame_codec());
        let hello = ClientMsg::Hello { proto_version: PROTO_VERSION, client_name: "pool-test".into() };
        conn.send(encode(&hello).unwrap()).await.unwrap();
        let _ = conn.next().await;
        let (shell, args): (&str, &[&str]) =
            if cfg!(windows) { ("cmd.exe", &["/d", "/k"]) } else { ("/bin/sh", &["-i"]) };
        let create = ClientMsg::CreateTerminal {
            cwd: None,
            shell: Some(shell.into()),
            args: args.iter().map(|a| (*a).into()).collect(),
            cols: 80,
            rows: 24,
            worktree: None,
        };
        conn.send(encode(&create).unwrap()).await.unwrap();
        while let Some(Ok(frame)) = conn.next().await {
            if matches!(decode::<ServerMsg>(&frame), Ok(ServerMsg::TerminalCreated(_))) {
                return;
            }
        }
    }

    async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !check() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn reachable(socket: &str) -> bool {
        sich_daemon::connect(socket).await.is_ok()
    }

    #[test]
    fn pool_creates_on_the_primary_and_drives_legacy_terminals_on_their_owner() {
        isolate();
        let base = unique_base();
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        rt.block_on(async {
            // The previous release listened on the bare base name: it must be discovered and kept.
            let legacy = tokio::spawn({
                let base = base.clone();
                async move { sich_daemon::run_with_build(&base, "legacy-build").await }
            });
            let primary = tokio::spawn({
                let base = base.clone();
                async move { sich_daemon::run_generation(&base, "primary-build").await }
            });
            let primary_socket = generation::socket(&base, "primary-build");
            for socket in [&base, &primary_socket] {
                let deadline = Instant::now() + Duration::from_secs(10);
                while !reachable(socket).await {
                    assert!(Instant::now() < deadline, "daemon did not start");
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
            raw_create(&base).await;

            let pool = DaemonPool::start_on(rt.handle(), base.clone(), "primary-build", Arc::new(|| {}), false);
            assert_eq!(pool.primary_socket(), primary_socket);
            eventually("legacy terminal in the pool", || {
                pool.terminals_synced() && pool.snapshot().terminals.len() == 1
            })
            .await;
            let snap = pool.snapshot();
            assert!(snap.legacy_terminals);
            let legacy_key = snap.terminals[0].info.id;
            let legacy_owner = pool.owner(legacy_key).unwrap();
            assert_eq!(legacy_owner.socket, base);

            // New terminals go to the primary even though both daemons number from 1.
            pool.create_terminal_in(80, 24, None);
            eventually("new terminal", || pool.snapshot().terminals.len() == 2).await;
            let new_key = pool.snapshot().terminals.iter().map(|t| t.info.id).find(|k| *k != legacy_key).unwrap();
            let new_owner = pool.owner(new_key).unwrap();
            assert_eq!(new_owner.socket, primary_socket);
            assert_eq!(new_owner.id, legacy_owner.id, "wire ids collide across daemons");

            // Killing the legacy terminal reaches only its daemon, which then retires itself.
            pool.kill_terminal(legacy_key);
            let deadline = Instant::now() + Duration::from_secs(15);
            while reachable(&base).await {
                assert!(Instant::now() < deadline, "legacy daemon did not retire");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            tokio::time::timeout(Duration::from_secs(5), legacy).await.expect("legacy task ends").unwrap().unwrap();
            eventually("only the primary terminal remains", || {
                pool.snapshot().terminals.iter().map(|t| t.info.id).collect::<Vec<_>>() == vec![new_key]
            })
            .await;
            assert!(!pool.snapshot().legacy_terminals);
            assert!(!primary.is_finished(), "the primary keeps running");

            pool.kill_terminal(new_key);
            eventually("primary terminal closed", || pool.snapshot().terminals.is_empty()).await;
            primary.abort();
        });
    }
}
