//! Local control channel `<base>-gateway` (named pipe / unix socket): used by `sich phone ...`
//! and later the GUI. Length-prefixed postcard frames; `Hello` first on both sides.

use std::collections::VecDeque;
use std::io;
use std::sync::Arc;

use anyhow::Context as _;
use bytes::Bytes;
use futures::{SinkExt as _, StreamExt as _};
use interprocess::local_socket::tokio::{Stream, prelude::*};
use interprocess::local_socket::{GenericFilePath, GenericNamespaced, Name};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::sync::mpsc;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use crate::hub::Hub;
use crate::store::Device;

pub const CONTROL_VERSION: u32 = 1;
const MAX_CONTROL_FRAME: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub endpoint_id: String,
    pub host_name: String,
    pub build_id: String,
    pub gateway_version: String,
    pub devices: Vec<Device>,
    /// Phones connected right now.
    pub connected: u32,
    /// Direct addresses the gateway is reachable at.
    pub addrs: Vec<String>,
    pub relay: Option<String>,
    /// UDP port the endpoint is bound to.
    pub port: u16,
    /// Set when `devices.json` is unreadable (all phones are refused).
    pub device_list_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingInfo {
    /// `sich://pair?...` URL for the QR code.
    pub url: String,
    pub expires_unix_ms: u64,
}

/// A phone presented a valid secret and waits for the desktop user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairRequest {
    pub request_id: u64,
    pub device_name: String,
    pub endpoint_id: String,
}

/// Client -> gateway. Append-only; `Hello` stays first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlReq {
    Hello { version: u32, build_id: String },
    Status,
    /// Replies `Pairing`; later `PairRequest` events arrive on this connection.
    StartPairing,
    Confirm { request_id: u64, allow: bool },
    ListDevices,
    Revoke { endpoint_id: String },
    Shutdown,
}

/// Gateway -> client. Append-only; `Hello` stays first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlResp {
    Hello { version: u32, build_id: String, gateway_version: String },
    Status(Status),
    Pairing(PairingInfo),
    PairRequest(PairRequest),
    Ok,
    Devices(Vec<Device>),
    Revoked(bool),
    Error(String),
}

/// Pipe name of the gateway that belongs to a daemon base name.
pub fn pipe_name(base: &str) -> String {
    format!("{base}-gateway")
}

pub(crate) fn to_name(name: &str) -> io::Result<Name<'static>> {
    if GenericNamespaced::is_supported() {
        name.to_string().to_ns_name::<GenericNamespaced>()
    } else {
        std::env::temp_dir().join(format!("{name}.sock")).to_fs_name::<GenericFilePath>()
    }
}

fn codec() -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder().big_endian().length_field_length(4).max_frame_length(MAX_CONTROL_FRAME).new_codec()
}

fn enc<T: Serialize>(msg: &T) -> anyhow::Result<Bytes> {
    Ok(Bytes::from(postcard::to_stdvec(msg)?))
}

fn dec<T: DeserializeOwned>(frame: &[u8]) -> anyhow::Result<T> {
    Ok(postcard::from_bytes(frame)?)
}

type Conn = Framed<Stream, LengthDelimitedCodec>;

// ----- server -----

pub(crate) async fn serve(listener: interprocess::local_socket::tokio::Listener, hub: Arc<Hub>) {
    use interprocess::local_socket::traits::tokio::Listener as _;
    loop {
        tokio::select! {
            _ = hub.shutdown.cancelled() => break,
            accepted = listener.accept() => match accepted {
                Ok(stream) => {
                    let hub = hub.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle(stream, hub).await {
                            tracing::debug!("control connection ended: {e:#}");
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!("control accept failed: {e}");
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            },
        }
    }
}

async fn next_event(events: &mut Option<mpsc::UnboundedReceiver<PairRequest>>) -> Option<PairRequest> {
    match events {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

async fn handle(stream: Stream, hub: Arc<Hub>) -> anyhow::Result<()> {
    let mut conn: Conn = Framed::new(stream, codec());
    let first = conn.next().await.context("closed before Hello")?.context("read Hello")?;
    match dec::<ControlReq>(&first)? {
        ControlReq::Hello { .. } => {
            let hello = ControlResp::Hello {
                version: CONTROL_VERSION,
                build_id: hub.build_id().to_string(),
                gateway_version: env!("CARGO_PKG_VERSION").to_string(),
            };
            conn.send(enc(&hello)?).await?;
        }
        // Like the daemon: any version can stop any gateway.
        ControlReq::Shutdown => {
            hub.shutdown.cancel();
            return Ok(());
        }
        _ => anyhow::bail!("expected Hello"),
    }
    let conn_id = hub.next_id();
    let mut events: Option<mpsc::UnboundedReceiver<PairRequest>> = None;
    let result = async {
        loop {
            tokio::select! {
                frame = conn.next() => {
                    let Some(frame) = frame else { return Ok(()) };
                    let req: ControlReq = dec(&frame.context("read frame")?)?;
                    let shutdown = req == ControlReq::Shutdown;
                    let resp = respond(&hub, conn_id, &mut events, req).await;
                    conn.send(enc(&resp)?).await?;
                    if shutdown {
                        return Ok(());
                    }
                }
                Some(request) = next_event(&mut events) => {
                    conn.send(enc(&ControlResp::PairRequest(request))?).await?;
                }
            }
        }
    }
    .await;
    hub.control_closed(conn_id);
    result
}

async fn respond(
    hub: &Arc<Hub>,
    conn_id: u64,
    events: &mut Option<mpsc::UnboundedReceiver<PairRequest>>,
    req: ControlReq,
) -> ControlResp {
    match req {
        ControlReq::Hello { .. } => ControlResp::Error("unexpected Hello".into()),
        ControlReq::Status => ControlResp::Status(hub.status()),
        ControlReq::StartPairing => {
            if let Some(reason) = hub.store.lock().poisoned() {
                return ControlResp::Error(format!("device list is unusable: {reason}"));
            }
            let (tx, rx) = mpsc::unbounded_channel();
            *events = Some(rx);
            let (url, expires_unix_ms) = hub.start_pairing(conn_id, tx).await;
            ControlResp::Pairing(PairingInfo { url, expires_unix_ms })
        }
        ControlReq::Confirm { request_id, allow } => {
            if hub.answer_pair(request_id, allow) {
                ControlResp::Ok
            } else {
                ControlResp::Error("no such pair request (expired?)".into())
            }
        }
        ControlReq::ListDevices => ControlResp::Devices(hub.store.lock().devices().to_vec()),
        ControlReq::Revoke { endpoint_id } => match hub.revoke(&endpoint_id) {
            Ok(removed) => ControlResp::Revoked(removed),
            Err(e) => ControlResp::Error(format!("{e:#}")),
        },
        ControlReq::Shutdown => {
            hub.shutdown.cancel();
            ControlResp::Ok
        }
    }
}

// ----- client -----

/// Client of the gateway control pipe. Requests are sequential; pair requests that arrive in
/// between are queued for `next_pair_request`.
pub struct ControlClient {
    conn: Conn,
    build_id: String,
    gateway_version: String,
    events: VecDeque<PairRequest>,
}

impl ControlClient {
    /// Connects to the gateway of `base` (the daemon base name) and exchanges `Hello`.
    /// `build_id` of the caller is sent along.
    pub async fn connect(base: &str) -> anyhow::Result<ControlClient> {
        Self::connect_as(base, "").await
    }

    pub async fn connect_as(base: &str, own_build_id: &str) -> anyhow::Result<ControlClient> {
        let stream = Stream::connect(to_name(&pipe_name(base))?).await.context("gateway is not running")?;
        let mut conn: Conn = Framed::new(stream, codec());
        conn.send(enc(&ControlReq::Hello { version: CONTROL_VERSION, build_id: own_build_id.to_string() })?).await?;
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), conn.next())
            .await
            .context("gateway did not answer")?
            .context("gateway closed the connection")?
            .context("read Hello")?;
        match dec::<ControlResp>(&frame)? {
            ControlResp::Hello { build_id, gateway_version, .. } => {
                Ok(ControlClient { conn, build_id, gateway_version, events: VecDeque::new() })
            }
            other => anyhow::bail!("unexpected reply to Hello: {other:?}"),
        }
    }

    /// Build id of the running gateway (to decide whether it must be replaced).
    pub fn gateway_build_id(&self) -> &str {
        &self.build_id
    }

    pub fn gateway_version(&self) -> &str {
        &self.gateway_version
    }

    async fn read(&mut self) -> anyhow::Result<ControlResp> {
        let frame = self.conn.next().await.context("gateway closed the connection")?.context("read frame")?;
        dec(&frame)
    }

    async fn request(&mut self, req: ControlReq) -> anyhow::Result<ControlResp> {
        self.conn.send(enc(&req)?).await?;
        loop {
            match self.read().await? {
                ControlResp::PairRequest(request) => self.events.push_back(request),
                ControlResp::Error(message) => anyhow::bail!("{message}"),
                resp => return Ok(resp),
            }
        }
    }

    pub async fn status(&mut self) -> anyhow::Result<Status> {
        match self.request(ControlReq::Status).await? {
            ControlResp::Status(status) => Ok(status),
            other => anyhow::bail!("unexpected reply: {other:?}"),
        }
    }

    /// Starts a pairing round (replacing a previous one); pair requests then arrive through
    /// `next_pair_request` on this connection. Keep the connection open while pairing.
    pub async fn start_pairing(&mut self) -> anyhow::Result<PairingInfo> {
        match self.request(ControlReq::StartPairing).await? {
            ControlResp::Pairing(info) => Ok(info),
            other => anyhow::bail!("unexpected reply: {other:?}"),
        }
    }

    /// Waits for the next phone that wants to pair; `None` when the gateway went away.
    pub async fn next_pair_request(&mut self) -> anyhow::Result<Option<PairRequest>> {
        if let Some(request) = self.events.pop_front() {
            return Ok(Some(request));
        }
        loop {
            let Some(frame) = self.conn.next().await else { return Ok(None) };
            match dec::<ControlResp>(&frame.context("read frame")?)? {
                ControlResp::PairRequest(request) => return Ok(Some(request)),
                _ => continue,
            }
        }
    }

    /// Answers a pair request: allow or deny.
    pub async fn confirm(&mut self, request_id: u64, allow: bool) -> anyhow::Result<()> {
        match self.request(ControlReq::Confirm { request_id, allow }).await? {
            ControlResp::Ok => Ok(()),
            other => anyhow::bail!("unexpected reply: {other:?}"),
        }
    }

    pub async fn list_devices(&mut self) -> anyhow::Result<Vec<Device>> {
        match self.request(ControlReq::ListDevices).await? {
            ControlResp::Devices(devices) => Ok(devices),
            other => anyhow::bail!("unexpected reply: {other:?}"),
        }
    }

    /// Removes a device (full endpoint id) and drops its connections. `false` if unknown.
    pub async fn revoke(&mut self, endpoint_id: &str) -> anyhow::Result<bool> {
        match self.request(ControlReq::Revoke { endpoint_id: endpoint_id.to_string() }).await? {
            ControlResp::Revoked(removed) => Ok(removed),
            other => anyhow::bail!("unexpected reply: {other:?}"),
        }
    }

    /// Asks the gateway to exit.
    pub async fn shutdown(&mut self) -> anyhow::Result<()> {
        match self.request(ControlReq::Shutdown).await {
            Ok(_) => Ok(()),
            // The gateway may close the pipe before the answer arrives.
            Err(_) => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_name_is_base_plus_gateway() {
        assert_eq!(pipe_name("sich-daemon"), "sich-daemon-gateway");
        assert!(to_name(&pipe_name("sich-daemon")).is_ok());
    }

    #[test]
    fn hello_stays_the_first_variant() {
        let req = ControlReq::Hello { version: 1, build_id: String::new() };
        assert_eq!(postcard::to_stdvec(&req).unwrap()[0], 0);
        let resp = ControlResp::Hello { version: 1, build_id: String::new(), gateway_version: String::new() };
        assert_eq!(postcard::to_stdvec(&resp).unwrap()[0], 0);
    }

    #[test]
    fn messages_round_trip() {
        let reqs = [
            ControlReq::Hello { version: 1, build_id: "b".into() },
            ControlReq::Status,
            ControlReq::StartPairing,
            ControlReq::Confirm { request_id: 7, allow: true },
            ControlReq::ListDevices,
            ControlReq::Revoke { endpoint_id: "e".into() },
            ControlReq::Shutdown,
        ];
        for req in reqs {
            assert_eq!(dec::<ControlReq>(&enc(&req).unwrap()).unwrap(), req);
        }
        let device = Device { endpoint_id: "e".into(), name: "n".into(), paired_unix_ms: 1, last_seen_unix_ms: 2 };
        let status = Status {
            endpoint_id: "e".into(),
            host_name: "h".into(),
            build_id: "b".into(),
            gateway_version: "v".into(),
            devices: vec![device.clone()],
            connected: 2,
            addrs: vec!["1.2.3.4:5".into()],
            relay: Some("https://r".into()),
            port: 6769,
            device_list_error: None,
        };
        let resps = [
            ControlResp::Hello { version: 1, build_id: "b".into(), gateway_version: "v".into() },
            ControlResp::Status(status),
            ControlResp::Pairing(PairingInfo { url: "sich://pair".into(), expires_unix_ms: 9 }),
            ControlResp::PairRequest(PairRequest { request_id: 1, device_name: "d".into(), endpoint_id: "e".into() }),
            ControlResp::Ok,
            ControlResp::Devices(vec![device]),
            ControlResp::Revoked(true),
            ControlResp::Error("x".into()),
        ];
        for resp in resps {
            assert_eq!(dec::<ControlResp>(&enc(&resp).unwrap()).unwrap(), resp);
        }
    }
}
