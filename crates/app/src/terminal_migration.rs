//! Moves one terminal between compatible daemon generations using the existing v7 IPC.

use std::collections::VecDeque;
use std::time::Duration;

use anyhow::Context as _;
use futures::{SinkExt as _, StreamExt as _};
use sich_daemon::ClientStream;
use sich_proto::{ClientMsg, PROTO_VERSION, ServerMsg, TermId, TerminalInfo, decode, encode, frame_codec};
use tokio::sync::oneshot;
use tokio::time::timeout;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use crate::daemon_client::TerminalOwner;

// Same history budget as a full restart, without writing a shared scrollback file.
const HISTORY_LIMIT: usize = 512 * 1024;
const IPC_TIMEOUT: Duration = Duration::from_secs(3);
pub(crate) const RESTORE_TIMEOUT: Duration = Duration::from_secs(15);

type Stream = Framed<ClientStream, LengthDelimitedCodec>;

pub(crate) struct Migration {
    source: Stream,
    id: TermId,
    pub history: VecDeque<u8>,
    pub done: oneshot::Sender<anyhow::Result<()>>,
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
                ServerMsg::DaemonInfo { .. } => return Ok((Self { source, id, history, done }, info)),
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
}
