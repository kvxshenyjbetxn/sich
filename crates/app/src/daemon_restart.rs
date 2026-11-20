//! The daemon restart runs on its connection task, which can only reconnect once shutdown
//! has released the endpoint. The session reaches disk before any shutdown request.

use std::path::Path;
use std::time::Duration;

use anyhow::Context as _;
use futures::{SinkExt as _, StreamExt as _};
use sich_proto::{ClientMsg, PROTO_VERSION, ServerMsg, decode, encode, frame_codec};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::time::{Instant, sleep, timeout};
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use crate::daemon_client::ConnStatus;
use crate::session::Session;

const FIRST_SCROLLBACK_PROTO: u32 = 7;
const IPC_TIMEOUT: Duration = Duration::from_secs(10);
const RELEASE_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) struct Request {
    pub expected: ConnStatus,
    pub session: Option<Session>,
}

impl Request {
    pub async fn save_and_shutdown(self, socket: &str) -> anyhow::Result<Session> {
        timeout(IPC_TIMEOUT, async {
            let framed = Framed::new(sich_daemon::connect(socket).await?, frame_codec());
            self.shutdown_peer(framed, crate::session::file_path().as_deref(), Some(socket))
                .await
        })
        .await
        .context("daemon restart request timed out")?
    }

    async fn shutdown_peer<S: AsyncRead + AsyncWrite + Unpin>(
        self,
        mut framed: Framed<S, LengthDelimitedCodec>,
        session_path: Option<&Path>,
        socket: Option<&str>,
    ) -> anyhow::Result<Session> {
        let (version, pid) = match self.expected {
            ConnStatus::Connected { pid } => (Some(PROTO_VERSION), Some(pid)),
            ConnStatus::Incompatible { version, pid } => (version, pid),
            ConnStatus::Reconnecting => anyhow::bail!("daemon connection changed before restart"),
        };
        let session = if let Some(proto_version) = version.filter(|v| *v >= FIRST_SCROLLBACK_PROTO)
        {
            framed
                .send(encode(&ClientMsg::Hello {
                    proto_version,
                    client_name: "sich-restart".into(),
                })?)
                .await?;
            let welcome = framed
                .next()
                .await
                .context("daemon closed before Welcome")??;
            anyhow::ensure!(
                matches!(decode::<ServerMsg>(&welcome)?, ServerMsg::Welcome { proto_version: v, daemon_pid }
                    if v == proto_version && Some(daemon_pid) == pid),
                "daemon changed before restart",
            );
            framed.send(encode(&ClientMsg::SaveScrollback)?).await?;
            let files = loop {
                let frame = framed
                    .next()
                    .await
                    .context("daemon closed while saving scrollback")??;
                if let Ok(ServerMsg::ScrollbackSaved { files }) = decode(&frame) {
                    break files;
                }
            };
            if let Some(session) = &self.session {
                anyhow::ensure!(
                    session.daemon_pid == pid,
                    "saved terminals belong to a different daemon"
                );
            }
            let mut session = self
                .session
                .or_else(|| session_path.and_then(Session::load_from))
                .unwrap_or_default();
            if session.attach_owner_scrollback(socket, pid, &files) {
                session
                    .save_to(session_path.context("no config directory")?)
                    .context("save session before shutdown")?;
            }
            session
        } else {
            // Pre-v7 daemons cannot supply snapshots or agent IDs. Keep the last saved session.
            session_path
                .and_then(Session::load_from)
                .unwrap_or_default()
        };

        framed
            .send(encode(&ClientMsg::Shutdown)?)
            .await
            .context("request daemon shutdown")?;
        Ok(session)
    }
}

pub(crate) async fn wait_for_exit(socket: &str) -> anyhow::Result<()> {
    let deadline = Instant::now() + RELEASE_TIMEOUT;
    loop {
        let probe = timeout(Duration::from_millis(250), sich_daemon::connect(socket)).await;
        if matches!(probe, Ok(Err(ref err)) if matches!(err.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused))
        {
            return Ok(());
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "old daemon did not release its socket"
        );
        sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{SavedTerminal, SavedWorkspace};
    use tokio::io::DuplexStream;

    type Stream = Framed<DuplexStream, LengthDelimitedCodec>;

    async fn recv(peer: &mut Stream) -> Option<ClientMsg> {
        let frame = timeout(Duration::from_secs(2), peer.next())
            .await
            .unwrap()?;
        Some(decode(&frame.unwrap()).unwrap())
    }

    async fn answer_snapshots(peer: &mut Stream) {
        assert!(matches!(
            recv(peer).await,
            Some(ClientMsg::Hello {
                proto_version: PROTO_VERSION,
                ..
            })
        ));
        peer.send(
            encode(&ServerMsg::Welcome {
                proto_version: PROTO_VERSION,
                daemon_pid: 12,
            })
            .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(recv(peer).await, Some(ClientMsg::SaveScrollback));
        peer.send(
            encode(&ServerMsg::ScrollbackSaved {
                files: vec![(4, "4-123.bin".into())],
            })
            .unwrap(),
        )
        .await
        .unwrap();
    }

    fn session() -> Session {
        Session {
            daemon_pid: Some(12),
            workspaces: vec![SavedWorkspace {
                terminals: vec![SavedTerminal {
                    id: Some(4),
                    cwd: Path::new("somewhere").into(),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn writes_snapshot_mapping_before_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        session().save_to(&path).unwrap();
        let (client, peer) = tokio::io::duplex(8192);
        let client_path = path.clone();
        let result = tokio::spawn(async move {
            let request = Request {
                expected: ConnStatus::Connected { pid: 12 },
                session: Some(session()),
            };
            request
                .shutdown_peer(Framed::new(client, frame_codec()), Some(&client_path), None)
                .await
        });
        let mut peer = Framed::new(peer, frame_codec());
        answer_snapshots(&mut peer).await;
        assert_eq!(recv(&mut peer).await, Some(ClientMsg::Shutdown));
        let saved = Session::load_from(&path).unwrap();
        assert_eq!(
            saved.workspaces[0].terminals[0].scrollback.as_deref(),
            Some("4-123.bin")
        );
        assert_eq!(result.await.unwrap().unwrap(), saved);
    }

    #[tokio::test]
    async fn a_failed_session_save_does_not_shutdown_the_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        std::fs::create_dir(&path).unwrap();
        let (client, peer) = tokio::io::duplex(8192);
        let result = tokio::spawn(async move {
            let request = Request {
                expected: ConnStatus::Connected { pid: 12 },
                session: Some(session()),
            };
            request
                .shutdown_peer(Framed::new(client, frame_codec()), Some(&path), None)
                .await
        });
        let mut peer = Framed::new(peer, frame_codec());
        answer_snapshots(&mut peer).await;
        assert_eq!(recv(&mut peer).await, None);
        assert_eq!(
            result.await.unwrap().unwrap_err().to_string(),
            "save session before shutdown"
        );
    }

    #[tokio::test]
    async fn legacy_daemon_gets_shutdown_as_the_first_frame_and_existing_session_is_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        session().save_to(&path).unwrap();
        let (client, peer) = tokio::io::duplex(8192);
        let result = tokio::spawn(async move {
            let request = Request {
                expected: ConnStatus::Incompatible {
                    pid: Some(12),
                    version: Some(6),
                },
                session: None,
            };
            request
                .shutdown_peer(Framed::new(client, frame_codec()), Some(&path), None)
                .await
        });
        assert_eq!(
            recv(&mut Framed::new(peer, frame_codec())).await,
            Some(ClientMsg::Shutdown)
        );
        assert_eq!(result.await.unwrap().unwrap(), session());
    }
}
