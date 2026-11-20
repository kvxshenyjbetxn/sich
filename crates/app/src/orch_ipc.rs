use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use futures::{SinkExt as _, StreamExt as _};
use sich_proto::{ClientMsg, PROTO_VERSION, ServerMsg, TerminalInfo, decode, encode, frame_codec};
use tokio_util::codec::{Framed, LengthDelimitedCodec};

pub struct Link {
    framed: Framed<sich_daemon::ClientStream, LengthDelimitedCodec>,
}

impl Link {
    pub async fn connect(socket: &str, client_name: &str) -> Result<Self> {
        let stream = sich_daemon::connect(socket)
            .await
            .context("the Sich daemon is not running")?;
        let mut link = Self {
            framed: Framed::new(stream, frame_codec()),
        };
        link.send(&ClientMsg::Hello {
            proto_version: PROTO_VERSION,
            client_name: client_name.into(),
        })
        .await?;
        match link.recv(Duration::from_secs(10)).await? {
            Some(ServerMsg::Welcome { proto_version, .. }) if proto_version == PROTO_VERSION => {
                Ok(link)
            }
            Some(ServerMsg::Welcome { proto_version, .. }) => {
                bail!(
                    "the daemon speaks protocol {proto_version}, expected {PROTO_VERSION}; restart Sich"
                )
            }
            other => bail!("unexpected daemon answer: {other:?}"),
        }
    }

    pub async fn send(&mut self, msg: &ClientMsg) -> Result<()> {
        self.framed
            .send(encode(msg)?)
            .await
            .context("send to the daemon")
    }

    /// Next server message, `None` on timeout.
    pub async fn recv(&mut self, timeout: Duration) -> Result<Option<ServerMsg>> {
        match tokio::time::timeout(timeout, self.framed.next()).await {
            Err(_) => Ok(None),
            Ok(None) => bail!("the daemon closed the connection"),
            Ok(Some(frame)) => Ok(Some(decode(&frame.context("read frame")?)?)),
        }
    }

    pub async fn list_terminals(&mut self) -> Result<Vec<TerminalInfo>> {
        self.send(&ClientMsg::ListTerminals).await?;
        let deadline = Instant::now() + Duration::from_secs(10);
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            if let Some(ServerMsg::Terminals(list)) = self.recv(left).await? {
                return Ok(list);
            }
        }
        bail!("the daemon did not answer ListTerminals")
    }
}
