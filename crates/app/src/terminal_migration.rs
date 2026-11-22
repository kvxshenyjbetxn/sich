//! Moves one terminal between compatible daemon generations using the existing v7 IPC.

use std::collections::VecDeque;
use std::time::Duration;

use anyhow::Context as _;
use futures::{SinkExt as _, StreamExt as _};
use sich_daemon::ClientStream;
use sich_proto::{ClientMsg, PROTO_VERSION, ServerMsg, TermId, TerminalInfo, decode, encode, frame_codec};
use tokio::sync::oneshot;
use tokio::time::{Instant, timeout};
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use crate::daemon_client::TerminalOwner;

// Same history budget as a full restart, without writing a shared scrollback file.
const HISTORY_LIMIT: usize = 512 * 1024;
pub(crate) const IPC_TIMEOUT: Duration = Duration::from_secs(3);
pub(crate) const RESTORE_TIMEOUT: Duration = Duration::from_secs(15);

type Stream = Framed<ClientStream, LengthDelimitedCodec>;

pub(crate) struct Migration {
    source: Stream,
    id: TermId,
    pub history: VecDeque<u8>,
    pub done: oneshot::Sender<anyhow::Result<()>>,
    pub deadline: Instant,
}

fn migration_abandoned(done_closed: bool, now: Instant, deadline: Instant) -> bool {
    done_closed || now > deadline
}

async fn send(stream: &mut Stream, msg: ClientMsg) -> anyhow::Result<()> {
    stream.send(encode(&msg)?).await.context("send terminal migration request")
}

async fn recv(stream: &mut Stream) -> anyhow::Result<ServerMsg> {
    let msg = decode(&stream.next().await.context("source daemon disconnected")??)?;
    if let ServerMsg::Error { message } = &msg {
        anyhow::bail!("source daemon: {message}");
    }
    Ok(msg)
}

impl Migration {
    pub(crate) fn is_abandoned(&self) -> bool {
        migration_abandoned(self.done.is_closed(), Instant::now(), self.deadline)
    }

    pub async fn capture(
        owner: &TerminalOwner,
        last_seq: u64,
        done: oneshot::Sender<anyhow::Result<()>>,
    ) -> anyhow::Result<(Self, TerminalInfo)> {
        timeout(IPC_TIMEOUT, Self::capture_inner(owner, last_seq, done))
            .await.context("timed out capturing terminal history")?
    }

    async fn capture_inner(
        owner: &TerminalOwner,
        last_seq: u64,
        done: oneshot::Sender<anyhow::Result<()>>,
    ) -> anyhow::Result<(Self, TerminalInfo)> {
        let mut source = Framed::new(sich_daemon::connect(&owner.socket).await?, frame_codec());
        send(&mut source, ClientMsg::Hello { proto_version: PROTO_VERSION, client_name: "sich-terminal-migration".into() }).await?;
        anyhow::ensure!(matches!(recv(&mut source).await?,
            ServerMsg::Welcome { proto_version: PROTO_VERSION, daemon_pid } if daemon_pid == owner.pid),
            "source daemon changed before terminal migration");
        send(&mut source, ClientMsg::ListTerminals).await?;
        let id = owner.id;
        let info = loop {
            if let ServerMsg::Terminals(terms) = recv(&mut source).await? {
                break terms.into_iter().find(|term| term.id == id && term.created_unix_ms == owner.created_ms)
                    .context("source terminal changed before migration")?;
            }
        };
        send(&mut source, ClientMsg::Subscribe {
            id, resume_from_seq: Some(last_seq.saturating_sub(HISTORY_LIMIT as u64)),
        }).await?;
        // The daemon handles these in order. Unsubscribe bounds the amount of live output
        // in the snapshot; the marker is queued after the last subscribed byte.
        send(&mut source, ClientMsg::Unsubscribe { id }).await?;
        send(&mut source, ClientMsg::GetDaemonInfo).await?;
        let mut history = VecDeque::new();
        loop {
            match recv(&mut source).await? {
                ServerMsg::DaemonInfo { .. } => {
                    let deadline = Instant::now() + RESTORE_TIMEOUT;
                    return Ok((Self { source, id, history, done, deadline }, info));
                }
                ServerMsg::TerminalRemoved { id: removed } if removed == id => anyhow::bail!("source terminal was closed"),
                ServerMsg::Output { id: output, bytes, .. } if output == id => {
                    let bytes = &bytes[bytes.len().saturating_sub(HISTORY_LIMIT)..];
                    let extra = (history.len() + bytes.len()).saturating_sub(HISTORY_LIMIT);
                    history.drain(..extra);
                    history.extend(bytes);
                }
                _ => {}
            }
        }
    }

    /// Called only after the destination acknowledges a replacement with the history replayed.
    pub async fn close_source(self) -> bool {
        let Self { mut source, id, done, .. } = self;
        let result = async {
            send(&mut source, ClientMsg::KillTerminal { id }).await?;
            loop {
                if matches!(recv(&mut source).await?, ServerMsg::TerminalRemoved { id: removed } if removed == id) {
                    return Ok(());
                }
            }
        };
        let result = timeout(IPC_TIMEOUT, result).await.context("timed out closing the source terminal").and_then(|r| r);
        let closed = result.is_ok();
        let _ = done.send(result);
        closed
    }

    #[cfg(test)]
    pub(crate) async fn test_pair(
        id: TermId,
        done: oneshot::Sender<anyhow::Result<()>>,
    ) -> (Self, Stream) {
        use interprocess::local_socket::tokio::prelude::*;
        use interprocess::local_socket::{GenericFilePath, GenericNamespaced, ListenerOptions};
        use std::sync::atomic::{AtomicU64, Ordering};

        static NEXT_SOCKET: AtomicU64 = AtomicU64::new(0);
        let scratch = tempfile::tempdir().unwrap();
        let name = if GenericNamespaced::is_supported() {
            let index = NEXT_SOCKET.fetch_add(1, Ordering::Relaxed);
            format!("sich-migration-test-{}-{index}", std::process::id()).to_ns_name::<GenericNamespaced>()
        } else {
            scratch.path().join("migration.sock").to_fs_name::<GenericFilePath>()
        }.unwrap();
        let listener = ListenerOptions::new().name(name.borrow()).create_tokio().unwrap();
        let (source, peer) = tokio::join!(ClientStream::connect(name), listener.accept());
        let migration = Self {
            source: Framed::new(source.unwrap(), frame_codec()),
            id,
            history: VecDeque::new(),
            done,
            deadline: Instant::now() + RESTORE_TIMEOUT,
        };
        (migration, Framed::new(peer.unwrap(), frame_codec()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_is_abandoned_if_the_receiver_is_gone_or_the_deadline_has_passed() {
        let now = Instant::now();
        let future = now + Duration::from_millis(1);
        assert!(!migration_abandoned(false, now, future));
        assert!(!migration_abandoned(false, now, now));
        assert!(migration_abandoned(false, future, now));
        assert!(migration_abandoned(true, now, future));
    }
}
