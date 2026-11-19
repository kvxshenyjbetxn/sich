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
    terminals: Mutex<BTreeMap<TermId, Arc<Terminal>>>,
    next_term: AtomicU64,
    next_conn: AtomicU64,
}

impl Manager {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            hub: Arc::new(Hub::default()),
            projects: crate::projects::Registry::load(),
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
    pub async fn create(self: &Arc<Self>, req: SpawnRequest) -> anyhow::Result<TerminalInfo> {
        let me = self.clone();
        let term = tokio::task::spawn_blocking(move || {
            let id = me.next_term.fetch_add(1, Ordering::Relaxed);
            Terminal::spawn(id, req, me.hub.clone())
        })
        .await
        .context("spawn task")??;
        let info = term.info();
        self.terminals.lock().insert(term.id(), term);
        self.hub.broadcast(ServerMsg::TerminalCreated(info.clone()));
        Ok(info)
    }

    /// Kills (if alive) and removes a terminal. Returns false if it does not exist.
    pub fn kill(&self, id: TermId) -> bool {
        let Some(term) = self.terminals.lock().remove(&id) else { return false };
        term.kill();
        self.hub.broadcast(ServerMsg::TerminalRemoved { id });
        true
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

    pub fn kill_all(&self) {
        let all = std::mem::take(&mut *self.terminals.lock());
        for t in all.values() {
            t.kill();
        }
    }
}
