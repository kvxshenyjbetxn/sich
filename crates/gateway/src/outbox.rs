//! Per-phone outbound queue. Pushing never blocks (the daemon reader must not wait for a
//! slow phone); terminal output is capped and can be purged on overflow.

use std::collections::VecDeque;

use parking_lot::Mutex;
use sich_remote_proto::{GatewayMsg, TermRef};
use tokio::sync::Notify;

/// Queued terminal output per phone before it is dropped and resynchronized.
pub const OUTPUT_CAP: usize = 8 * 1024 * 1024;

#[derive(Default)]
struct Inner {
    queue: VecDeque<GatewayMsg>,
    /// Total payload bytes of queued `Output` messages.
    output_bytes: usize,
    closed: bool,
}

pub struct Outbox {
    inner: Mutex<Inner>,
    notify: Notify,
    cap: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Push {
    Queued,
    /// The queue is over its cap; the chunk was not queued.
    Overflow,
}

fn output_len(msg: &GatewayMsg) -> usize {
    match msg {
        GatewayMsg::Output { bytes, .. } => bytes.len(),
        _ => 0,
    }
}

impl Default for Outbox {
    fn default() -> Self {
        Self::with_cap(OUTPUT_CAP)
    }
}

impl Outbox {
    pub fn with_cap(cap: usize) -> Self {
        Self { inner: Mutex::new(Inner::default()), notify: Notify::new(), cap }
    }

    /// Queues any message, ignoring the output cap (control messages, resync replay).
    pub fn push(&self, msg: GatewayMsg) {
        let mut inner = self.inner.lock();
        if inner.closed {
            return;
        }
        inner.output_bytes += output_len(&msg);
        inner.queue.push_back(msg);
        drop(inner);
        self.notify.notify_one();
    }

    /// Queues terminal output unless that would exceed the cap.
    pub fn push_output(&self, term: &TermRef, seq: u64, bytes: Vec<u8>) -> Push {
        let mut inner = self.inner.lock();
        if inner.closed {
            return Push::Queued;
        }
        if inner.output_bytes + bytes.len() > self.cap {
            return Push::Overflow;
        }
        inner.output_bytes += bytes.len();
        inner.queue.push_back(GatewayMsg::Output { term: term.clone(), seq, bytes });
        drop(inner);
        self.notify.notify_one();
        Push::Queued
    }

    /// Drops queued output (and resets) of one terminal.
    pub fn purge_terminal(&self, term: &TermRef) {
        let mut inner = self.inner.lock();
        let mut freed = 0;
        inner.queue.retain(|m| match m {
            GatewayMsg::Output { term: t, bytes, .. } if t == term => {
                freed += bytes.len();
                false
            }
            GatewayMsg::Reset { term: t, .. } if t == term => false,
            _ => true,
        });
        inner.output_bytes -= freed;
    }

    pub fn queued_output_bytes(&self) -> usize {
        self.inner.lock().output_bytes
    }

    /// Next message; waits when empty. `None` once closed and drained.
    pub async fn pop(&self) -> Option<GatewayMsg> {
        loop {
            let notified = self.notify.notified();
            {
                let mut inner = self.inner.lock();
                if let Some(msg) = inner.queue.pop_front() {
                    inner.output_bytes -= output_len(&msg);
                    return Some(msg);
                }
                if inner.closed {
                    return None;
                }
            }
            notified.await;
        }
    }

    /// No more messages are accepted; `pop` drains what is queued, then returns `None`.
    pub fn close(&self) {
        self.inner.lock().closed = true;
        self.notify.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term(id: u64) -> TermRef {
        TermRef { daemon: "d".into(), id }
    }

    #[test]
    fn cap_applies_to_output_only() {
        let o = Outbox::with_cap(10);
        assert_eq!(o.push_output(&term(1), 0, vec![0; 6]), Push::Queued);
        assert_eq!(o.push_output(&term(1), 6, vec![0; 6]), Push::Overflow);
        assert_eq!(o.queued_output_bytes(), 6);
        o.push(GatewayMsg::Pong { nonce: 1 });
        o.push(GatewayMsg::Output { term: term(1), seq: 0, bytes: vec![0; 100] });
        assert_eq!(o.queued_output_bytes(), 106, "forced pushes ignore the cap");
    }

    #[test]
    fn purge_removes_only_that_terminals_output_and_resets() {
        let o = Outbox::with_cap(100);
        o.push_output(&term(1), 0, vec![1; 5]);
        o.push_output(&term(2), 0, vec![2; 7]);
        o.push(GatewayMsg::Reset { term: term(1), cols: 1, rows: 1 });
        o.push(GatewayMsg::Pong { nonce: 3 });
        o.purge_terminal(&term(1));
        assert_eq!(o.queued_output_bytes(), 7);
        let inner = o.inner.lock();
        assert_eq!(inner.queue.len(), 2);
    }

    #[tokio::test]
    async fn pop_waits_drains_then_ends_after_close() {
        let o = std::sync::Arc::new(Outbox::default());
        let o2 = o.clone();
        let waiter = tokio::spawn(async move { o2.pop().await });
        tokio::task::yield_now().await;
        o.push(GatewayMsg::Pong { nonce: 9 });
        assert_eq!(waiter.await.unwrap(), Some(GatewayMsg::Pong { nonce: 9 }));
        o.push_output(&term(1), 0, vec![1; 4]);
        o.close();
        o.push(GatewayMsg::Pong { nonce: 10 });
        assert!(matches!(o.pop().await, Some(GatewayMsg::Output { .. })));
        assert_eq!(o.queued_output_bytes(), 0);
        assert_eq!(o.pop().await, None);
    }
}
