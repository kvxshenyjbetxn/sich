//! UI-side handle to the daemon: shared state (terminals, connection status) plus commands.
//!
//! The connection itself lives in `daemon_conn` (a tokio task with auto-reconnect).

use std::sync::Arc;

use parking_lot::Mutex;
use sich_proto::{TermId, TerminalInfo};

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
    pub daemon_pid: Option<u32>,
}

/// Commands from the UI to the connection task.
pub(crate) enum Cmd {
    Create { cols: u16, rows: u16 },
    Kill(TermId),
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
}

impl DaemonClient {
    /// Starts the connection task on `rt`.
    pub fn start(rt: &Handle, socket: String, wake: Wake) -> DaemonClient {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let state = Arc::new(Mutex::new(ClientState {
            status: ConnStatus::Reconnecting,
            terminals: Vec::new(),
            synced: false,
            daemon_pid: None,
        }));
        let conn = daemon_conn::Conn {
            socket,
            rt: rt.clone(),
            state: state.clone(),
            cmd_tx: cmd_tx.clone(),
            wake,
        };
        rt.spawn(daemon_conn::run(conn, cmd_rx));
        DaemonClient { cmd_tx, state }
    }

    pub fn snapshot(&self) -> Snapshot {
        let st = self.state.lock();
        Snapshot { status: st.status, terminals: st.terminals.clone() }
    }

    pub fn create_terminal(&self, cols: u16, rows: u16) {
        let _ = self.cmd_tx.send(Cmd::Create { cols, rows });
    }

    pub fn kill_terminal(&self, id: TermId) {
        let _ = self.cmd_tx.send(Cmd::Kill(id));
    }
}
