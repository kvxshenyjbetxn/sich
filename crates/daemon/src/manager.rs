//! Daemon-wide state: terminal registry and the connection hub.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Context as _;
use parking_lot::Mutex;
use sich_proto::{ServerMsg, TermId, TerminalInfo};
use tokio_util::sync::CancellationToken;

use crate::hub::{ConnId, Hub};
use crate::terminal::{SpawnRequest, Terminal};

pub struct Manager {
    pub hub: Arc<Hub>,
    pub shutdown: CancellationToken,
    pub projects: crate::projects::Registry,
    /// Socket name this daemon listens on; handed to terminals as `SICH_SOCKET`.
    pub socket_name: String,
    pub build_id: String,
    pub started_unix_ms: u64,
    /// Serializes worktree removal with terminal creation in a project.
    pub(crate) worktree_operations: tokio::sync::Mutex<()>,
    scrollback_lock: Mutex<()>,
    terminals: Mutex<BTreeMap<TermId, Arc<Terminal>>>,
    next_term: AtomicU64,
    next_conn: AtomicU64,
}

impl Manager {
    pub fn new(socket_name: &str, build_id: &str) -> Arc<Self> {
        Arc::new(Self {
            hub: Arc::new(Hub::default()),
            projects: crate::projects::Registry::load(),
            socket_name: socket_name.to_string(),
            build_id: build_id.into(),
            started_unix_ms: crate::terminal::now_ms(),
            worktree_operations: tokio::sync::Mutex::new(()),
            scrollback_lock: Mutex::new(()),
            shutdown: CancellationToken::new(),
            terminals: Mutex::new(BTreeMap::new()),
            next_term: AtomicU64::new(1),
            next_conn: AtomicU64::new(1),
        })
    }

    pub fn next_conn_id(&self) -> ConnId {
        self.next_conn.fetch_add(1, Ordering::Relaxed)
    }

    pub fn list(&self) -> Vec<TerminalInfo> {
        self.terminals.lock().values().map(|t| t.info()).collect()
    }

    pub fn get(&self, id: TermId) -> Option<Arc<Terminal>> {
        self.terminals.lock().get(&id).cloned()
    }

    /// Spawns a terminal (blocking work is moved off the async runtime) and broadcasts it.
    pub async fn create(
        self: &Arc<Self>,
        req: SpawnRequest,
        before_broadcast: impl FnOnce(&TerminalInfo),
    ) -> anyhow::Result<()> {
        let _operation = self.worktree_operations.lock().await;
        let me = self.clone();
        let term = tokio::task::spawn_blocking(move || {
            let id = me.next_term.fetch_add(1, Ordering::Relaxed);
            Terminal::spawn(id, req, me.hub.clone(), &me.socket_name)
        })
        .await
        .context("spawn task")??;
        let info = term.info();
        self.terminals.lock().insert(term.id(), term);
        before_broadcast(&info);
        self.hub.broadcast(ServerMsg::TerminalCreated(info));
        Ok(())
    }

    /// Kills (if alive) and removes a terminal. Returns false if it does not exist.
    pub fn kill(&self, id: TermId) -> bool {
        let Some(term) = self.terminals.lock().remove(&id) else { return false };
        term.kill();
        term.clear_agent();
        self.hub.broadcast(ServerMsg::TerminalRemoved { id });
        true
    }

    /// Feeds a hook payload to the terminal's agent tracker. Unknown terminals are ignored.
    pub fn agent_hook(&self, id: TermId, agent: sich_proto::AgentKind, payload: &str) {
        if let Some(t) = self.get(id) {
            t.agent_hook(agent, payload);
        }
    }

    pub fn mark_seen(&self, id: TermId) {
        if let Some(t) = self.get(id) {
            t.mark_seen();
        }
    }

    /// Kills and removes every terminal that belongs to `worktree`.
    pub fn kill_in_worktree(&self, worktree: &std::path::Path) {
        let ids: Vec<TermId> = self
            .terminals
            .lock()
            .values()
            .filter(|t| t.info().worktree.is_some_and(|w| crate::git::same_path(&w, worktree)))
            .map(|t| t.id())
            .collect();
        for id in ids {
            self.kill(id);
        }
    }

    /// Drops all subscriptions of a closed connection.
    pub fn drop_connection(&self, conn: ConnId) {
        self.hub.unregister(conn);
        for t in self.terminals.lock().values() {
            t.unsubscribe(conn);
        }
    }

    pub async fn save_scrollback(self: &Arc<Self>) -> Vec<(TermId, String)> {
        let Some(dir) = crate::scrollback::directory() else { return Vec::new() };
        let mgr = self.clone();
        match tokio::task::spawn_blocking(move || {
            let _save = mgr.scrollback_lock.lock();
            let terminals: Vec<_> = mgr.terminals.lock().values().rev().cloned().collect();
            let mut files = Vec::new();
            for term in terminals {
                if files.len() == crate::scrollback::MAX_FILES {
                    break;
                }
                let Some(bytes) = term.scrollback_tail() else { continue };
                match crate::scrollback::save(&dir, term.id(), mgr.started_unix_ms, &bytes) {
                    Ok(name) => files.push((term.id(), name)),
                    Err(err) => tracing::warn!(id = term.id(), "could not save scrollback: {err}"),
                }
            }
            crate::scrollback::prune(&dir, std::time::SystemTime::now());
            files.retain(|(_, name)| dir.join(name).is_file());
            files
        }).await {
            Ok(files) => files,
            Err(err) => {
                tracing::warn!("scrollback task failed: {err}");
                Vec::new()
            }
        }
    }

    pub fn kill_all(&self) {
        let all = std::mem::take(&mut *self.terminals.lock());
        for t in all.values() {
            t.kill();
        }
    }
}
