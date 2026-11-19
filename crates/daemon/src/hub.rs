//! Connection handles and broadcast registry.

use std::collections::HashMap;

use parking_lot::Mutex;
use sich_proto::ServerMsg;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub type ConnId = u64;

/// Write side of a client connection. Sending never blocks: when the queue is full the
/// connection is closed (the client will reconnect and resume).
#[derive(Clone)]
pub struct ConnHandle {
    pub id: ConnId,
    tx: mpsc::Sender<ServerMsg>,
    cancel: CancellationToken,
}

impl ConnHandle {
    pub fn new(id: ConnId, tx: mpsc::Sender<ServerMsg>, cancel: CancellationToken) -> Self {
        Self { id, tx, cancel }
    }

    /// Returns false if the message could not be queued (connection closed or dropped).
    pub fn send(&self, msg: ServerMsg) -> bool {
        match self.tx.try_send(msg) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!(conn = self.id, "client queue full, closing connection");
                self.cancel.cancel();
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }
}

#[derive(Default)]
pub struct Hub {
    conns: Mutex<HashMap<ConnId, ConnHandle>>,
}

impl Hub {
    pub fn register(&self, conn: ConnHandle) {
        self.conns.lock().insert(conn.id, conn);
    }

    pub fn unregister(&self, id: ConnId) {
        self.conns.lock().remove(&id);
    }

    pub fn broadcast(&self, msg: ServerMsg) {
        let conns: Vec<ConnHandle> = self.conns.lock().values().cloned().collect();
        for c in conns {
            c.send(msg.clone());
        }
    }
}
