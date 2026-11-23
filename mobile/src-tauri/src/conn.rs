//! The connection manager: the ONLY owner of the connection to the gateway.
//!
//! The UI talks to it through [`ConnectionManager`] (a cheap command handle) and receives
//! events through the [`Ui`] trait. A single actor task owns the iroh endpoint, the active
//! connection, the current terminal subscription and the reconnect logic, so there is never a
//! stale connection handle anywhere else.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::{SinkExt, StreamExt};
use iroh::endpoint::{Connection, QuicTransportConfig, presets};
use iroh::{Endpoint, EndpointAddr, RelayMode, RelayUrl, SecretKey, TransportAddr};
use sich_remote_proto::{
    ALPN, GatewayMsg, MIN_COMPATIBLE, PairUrl, PhoneMsg, REMOTE_PROTO_VERSION, TermRef, Workspace, compatible,
    decode_frame, encode_frame, frame_codec,
};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tokio_util::codec::{FramedRead, FramedWrite};

use crate::backoff::{Backoff, PING_INTERVAL, is_dead};
use crate::hosts::{Host, HostStore};
use crate::ui::{ConnState, OutputSink, Ui, reject_code};

/// How much terminal history we ask the gateway to replay on subscribe.
pub const REPLAY_BYTES: u32 = 256 * 1024;
/// Largest upload we accept before the 16 MiB frame limit would bite.
pub const MAX_UPLOAD_BYTES: usize = 12 * 1024 * 1024;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// The desktop user has 2 minutes to confirm; leave a little slack.
const PAIR_CONFIRM_TIMEOUT: Duration = Duration::from_secs(150);
/// After an app resume the link may be dead: expect any traffic within this window.
const RESUME_PROBE: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Default)]
pub struct ConnConfig {
    /// No relay / discovery (tests, LAN only).
    pub relay_disabled: bool,
    pub device_name: String,
}

/// Which gateway we want to be connected to.
#[derive(Debug, Clone)]
enum Target {
    Host(Host),
    Pair(PairUrl),
}

struct Sub {
    term: TermRef,
    cols: u16,
    rows: u16,
    fit: bool,
    sink: Arc<dyn OutputSink>,
}

enum Cmd {
    Pair(PairUrl),
    Connect(Host),
    Stop,
    RetryNow,
    Resumed,
    Subscribe { term: TermRef, cols: u16, rows: u16, fit: bool, sink: Arc<dyn OutputSink> },
    Unsubscribe(TermRef),
    Input { term: TermRef, bytes: Vec<u8> },
    SendText { term: TermRef, text: String, images: Vec<String>, submit: bool },
    Resize { term: TermRef, cols: u16, rows: u16 },
    SetFit { term: TermRef, fit: bool },
    MarkSeen(TermRef),
    Upload { file_name: String, data: Vec<u8>, reply: oneshot::Sender<Result<String, String>> },
}

/// Cheap handle used by Tauri commands. Never holds a connection.
#[derive(Clone)]
pub struct ConnectionManager {
    tx: mpsc::UnboundedSender<Cmd>,
    state: Arc<Mutex<ConnState>>,
    workspace: Arc<Mutex<Option<Workspace>>>,
}

impl ConnectionManager {
    /// Spawn the actor on `rt`.
    pub fn start(
        cfg: ConnConfig,
        ui: Arc<dyn Ui>,
        store: Arc<Mutex<HostStore>>,
        key: SecretKey,
        rt: &tokio::runtime::Handle,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let state = Arc::new(Mutex::new(ConnState::Idle));
        let workspace = Arc::new(Mutex::new(None));
        let actor = Actor {
            cfg,
            ui,
            store,
            key,
            endpoint: None,
            state: state.clone(),
            workspace: workspace.clone(),
            target: None,
            halted: false,
            sub: None,
            backoff: Backoff::new(),
            next_req: 1,
            next_nonce: 1,
        };
        rt.spawn(actor.run(rx));
        Self { tx, state, workspace }
    }

    pub fn state(&self) -> ConnState {
        self.state.lock().unwrap().clone()
    }

    pub fn workspace(&self) -> Option<Workspace> {
        self.workspace.lock().unwrap().clone()
    }

    pub fn pair(&self, url: PairUrl) {
        let _ = self.tx.send(Cmd::Pair(url));
    }

    pub fn connect(&self, host: Host) {
        let _ = self.tx.send(Cmd::Connect(host));
    }

    /// Drop the connection and stop reconnecting.
    pub fn stop(&self) {
        let _ = self.tx.send(Cmd::Stop);
    }

    pub fn retry_now(&self) {
        let _ = self.tx.send(Cmd::RetryNow);
    }

    /// The app came back to the foreground.
    pub fn resumed(&self) {
        let _ = self.tx.send(Cmd::Resumed);
    }

    pub fn subscribe(&self, term: TermRef, cols: u16, rows: u16, fit: bool, sink: Arc<dyn OutputSink>) {
        let _ = self.tx.send(Cmd::Subscribe { term, cols, rows, fit, sink });
    }

    pub fn unsubscribe(&self, term: TermRef) {
        let _ = self.tx.send(Cmd::Unsubscribe(term));
    }

    pub fn input(&self, term: TermRef, bytes: Vec<u8>) {
        let _ = self.tx.send(Cmd::Input { term, bytes });
    }

    pub fn send_text(&self, term: TermRef, text: String, images: Vec<String>, submit: bool) {
        let _ = self.tx.send(Cmd::SendText { term, text, images, submit });
    }

    pub fn resize(&self, term: TermRef, cols: u16, rows: u16) {
        let _ = self.tx.send(Cmd::Resize { term, cols, rows });
    }

    pub fn set_fit(&self, term: TermRef, fit: bool) {
        let _ = self.tx.send(Cmd::SetFit { term, fit });
    }

    pub fn mark_seen(&self, term: TermRef) {
        let _ = self.tx.send(Cmd::MarkSeen(term));
    }

    /// Upload an image; resolves with the gateway's upload id.
    pub async fn upload(&self, file_name: String, data: Vec<u8>) -> Result<String, String> {
        if data.len() > MAX_UPLOAD_BYTES {
            return Err("image too large".into());
        }
        let (reply, rx) = oneshot::channel();
        self.tx.send(Cmd::Upload { file_name, data, reply }).map_err(|_| "manager stopped".to_string())?;
        match tokio::time::timeout(Duration::from_secs(120), rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err("connection lost".into()),
            Err(_) => Err("upload timed out".into()),
        }
    }
}

/// Build an iroh address from the parts stored in a pairing link / host record.
/// Unparsable direct addresses are skipped (discovery may still find the host).
pub fn to_endpoint_addr(endpoint_id: &str, addrs: &[String], relay: Option<&str>) -> Result<EndpointAddr, String> {
    let id: iroh::EndpointId = endpoint_id.parse().map_err(|e| format!("bad endpoint id: {e}"))?;
    let mut parts: Vec<TransportAddr> =
        addrs.iter().filter_map(|a| a.parse::<SocketAddr>().ok()).map(TransportAddr::Ip).collect();
    if let Some(r) = relay.filter(|r| !r.is_empty()) {
        let url: RelayUrl = r.parse().map_err(|e| format!("bad relay url: {e}"))?;
        parts.push(TransportAddr::Relay(url));
    }
    Ok(EndpointAddr::from_parts(id, parts))
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

enum Flow {
    Continue,
    /// Target changed or an immediate reconnect was requested.
    Restart,
    /// Check the link is alive right now (resume, manual retry while connected).
    Probe,
}

enum End {
    Rejected { reason: String, detail: Option<String> },
    Lost(String),
    Switched,
    Closed,
}

struct Session {
    out: mpsc::UnboundedSender<PhoneMsg>,
    established: bool,
    uploads: HashMap<u64, oneshot::Sender<Result<String, String>>>,
}

impl Session {
    fn send(&self, msg: PhoneMsg) {
        if self.established {
            let _ = self.out.send(msg);
        }
    }
}

/// Aborts the reader/writer tasks and closes the connection when a session ends.
struct SessionGuard {
    conn: Connection,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
        self.conn.close(0u32.into(), b"bye");
    }
}

struct Actor {
    cfg: ConnConfig,
    ui: Arc<dyn Ui>,
    store: Arc<Mutex<HostStore>>,
    key: SecretKey,
    endpoint: Option<Endpoint>,
    state: Arc<Mutex<ConnState>>,
    workspace: Arc<Mutex<Option<Workspace>>>,
    target: Option<Target>,
    /// Reconnecting stopped (rejected / failed pairing): wait for the user.
    halted: bool,
    sub: Option<Sub>,
    backoff: Backoff,
    next_req: u64,
    next_nonce: u64,
}

impl Actor {
    fn set_state(&self, st: ConnState) {
        {
            let mut cur = self.state.lock().unwrap();
            if *cur == st {
                return;
            }
            *cur = st.clone();
        }
        self.ui.connection(&st);
    }

    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Cmd>) {
        loop {
            let target = match (&self.target, self.halted) {
                (Some(t), false) => t.clone(),
                _ => {
                    match rx.recv().await {
                        Some(c) => {
                            self.handle_cmd(c, None);
                        }
                        None => return,
                    }
                    continue;
                }
            };
            if self.backoff.attempt() == 0 {
                self.set_state(ConnState::Connecting);
            } else {
                self.set_state(ConnState::Reconnecting { attempt: self.backoff.attempt() as u32, retry_in_ms: 0 });
            }
            match self.session(&target, &mut rx).await {
                End::Closed => return,
                End::Switched => {}
                End::Rejected { reason, detail } => {
                    self.halted = true;
                    self.set_state(ConnState::Rejected { reason, detail });
                }
                End::Lost(err) => {
                    tracing::info!("connection lost: {err}");
                    if matches!(self.target, Some(Target::Pair(_))) {
                        // A pairing secret is one-shot: do not retry it behind the user's back.
                        self.halted = true;
                        self.set_state(ConnState::Rejected { reason: "pair_failed".into(), detail: Some(err) });
                        continue;
                    }
                    let delay = self.backoff.next_delay();
                    self.set_state(ConnState::Reconnecting {
                        attempt: self.backoff.attempt() as u32,
                        retry_in_ms: delay.as_millis() as u64,
                    });
                    if !self.wait_retry(delay, &mut rx).await {
                        return;
                    }
                }
            }
        }
    }

    /// Sleep until the retry time, or until a command asks to reconnect now. False = manager gone.
    async fn wait_retry(&mut self, delay: Duration, rx: &mut mpsc::UnboundedReceiver<Cmd>) -> bool {
        let deadline = Instant::now() + delay;
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => return true,
                cmd = rx.recv() => match cmd {
                    None => return false,
                    Some(c) => if matches!(self.handle_cmd(c, None), Flow::Restart) { return true },
                },
            }
        }
    }

    fn ensure_endpoint_cfg(&self) -> Result<QuicTransportConfig, String> {
        let idle = Duration::from_secs(15).try_into().map_err(|e| format!("idle timeout: {e}"))?;
        Ok(QuicTransportConfig::builder()
            .keep_alive_interval(Duration::from_secs(5))
            .max_idle_timeout(Some(idle))
            .build())
    }

    async fn endpoint(&mut self) -> Result<Endpoint, String> {
        if let Some(ep) = self.endpoint.as_ref().filter(|e| !e.is_closed()) {
            return Ok(ep.clone());
        }
        let tc = self.ensure_endpoint_cfg()?;
        let builder = if self.cfg.relay_disabled {
            Endpoint::builder(presets::Minimal).relay_mode(RelayMode::Disabled)
        } else {
            Endpoint::builder(presets::N0)
        };
        let ep = builder
            .secret_key(self.key.clone())
            .transport_config(tc)
            .bind()
            .await
            .map_err(|e| format!("bind: {e}"))?;
        self.endpoint = Some(ep.clone());
        Ok(ep)
    }

    /// Commands that behave the same with or without a live session. `sess` is `Some` while a
    /// connection exists (messages are only sent once the session is established).
    fn handle_cmd(&mut self, cmd: Cmd, sess: Option<&mut Session>) -> Flow {
        let online = sess.is_some();
        match cmd {
            Cmd::Pair(url) => {
                self.switch_target(Some(Target::Pair(url)));
                return Flow::Restart;
            }
            Cmd::Connect(host) => {
                self.switch_target(Some(Target::Host(host)));
                return Flow::Restart;
            }
            Cmd::Stop => {
                self.switch_target(None);
                self.set_state(ConnState::Idle);
                return Flow::Restart;
            }
            Cmd::RetryNow | Cmd::Resumed => {
                self.halted = self.halted && matches!(self.target, Some(Target::Pair(_)));
                if online {
                    let established = sess.as_deref().is_some_and(|s| s.established);
                    return if established { Flow::Probe } else { Flow::Restart };
                }
                if self.target.is_some() && !self.halted {
                    self.backoff.reset();
                    return Flow::Restart;
                }
            }
            Cmd::Subscribe { term, cols, rows, fit, sink } => {
                if let Some(old) = self.sub.as_ref().filter(|s| s.term != term)
                    && let Some(s) = sess.as_deref()
                {
                    s.send(PhoneMsg::Unsubscribe { term: old.term.clone() });
                }
                self.sub = Some(Sub { term: term.clone(), cols, rows, fit, sink });
                if let Some(s) = sess.as_deref() {
                    s.send(PhoneMsg::Subscribe { term, cols, rows, fit, replay_bytes: REPLAY_BYTES });
                }
            }
            Cmd::Unsubscribe(term) => {
                if self.sub.as_ref().is_some_and(|s| s.term == term) {
                    self.sub = None;
                }
                if let Some(s) = sess.as_deref() {
                    s.send(PhoneMsg::Unsubscribe { term });
                }
            }
            Cmd::Resize { term, cols, rows } => {
                if let Some(sub) = self.sub.as_mut().filter(|s| s.term == term) {
                    sub.cols = cols;
                    sub.rows = rows;
                }
                if let Some(s) = sess.as_deref() {
                    s.send(PhoneMsg::Resize { term, cols, rows });
                }
            }
            Cmd::SetFit { term, fit } => {
                if let Some(sub) = self.sub.as_mut().filter(|s| s.term == term) {
                    sub.fit = fit;
                }
                if let Some(s) = sess.as_deref() {
                    s.send(PhoneMsg::SetFit { term, fit });
                }
            }
            Cmd::Input { term, bytes } => {
                if let Some(s) = sess.as_deref() {
                    s.send(PhoneMsg::Input { term, bytes });
                }
            }
            Cmd::SendText { term, text, images, submit } => {
                if let Some(s) = sess.as_deref() {
                    s.send(PhoneMsg::SendText { term, text, images, submit });
                }
            }
            Cmd::MarkSeen(term) => {
                if let Some(s) = sess.as_deref() {
                    s.send(PhoneMsg::MarkSeen { term });
                }
            }
            Cmd::Upload { file_name, data, reply } => match sess.filter(|s| s.established) {
                Some(s) => {
                    let req_id = self.next_req;
                    self.next_req += 1;
                    s.uploads.insert(req_id, reply);
                    let _ = s.out.send(PhoneMsg::UploadImage { req_id, file_name, data });
                }
                None => {
                    let _ = reply.send(Err("not connected".into()));
                }
            },
        }
        Flow::Continue
    }

    fn switch_target(&mut self, t: Option<Target>) {
        self.target = t;
        self.halted = false;
        self.backoff.reset();
    }

    async fn session(&mut self, target: &Target, rx: &mut mpsc::UnboundedReceiver<Cmd>) -> End {
        let (addr, pair_secret) = match target {
            Target::Host(h) => (to_endpoint_addr(&h.endpoint_id, &h.addrs, h.relay.as_deref()), None),
            Target::Pair(p) => {
                (to_endpoint_addr(&p.endpoint_id, &p.addrs, p.relay.as_deref()), Some(p.secret.clone()))
            }
        };
        let addr = match addr {
            Ok(a) => a,
            Err(e) => return End::Rejected { reason: "bad_address".into(), detail: Some(e) },
        };
        let ep = match self.endpoint().await {
            Ok(ep) => ep,
            Err(e) => return End::Lost(e),
        };

        // Connect, but stay responsive to commands (switch host, retry, stop).
        let connect = tokio::time::timeout(CONNECT_TIMEOUT, ep.connect(addr, ALPN));
        tokio::pin!(connect);
        let conn = loop {
            tokio::select! {
                r = &mut connect => match r {
                    Ok(Ok(c)) => break c,
                    Ok(Err(e)) => return End::Lost(format!("connect: {e}")),
                    Err(_) => return End::Lost("connect timed out".into()),
                },
                cmd = rx.recv() => match cmd {
                    None => return End::Closed,
                    Some(c) => if matches!(self.handle_cmd(c, None), Flow::Restart) { return End::Switched },
                },
            }
        };
        let (send, recv) = match conn.open_bi().await {
            Ok(s) => s,
            Err(e) => return End::Lost(format!("open stream: {e}")),
        };

        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<PhoneMsg>();
        let (in_tx, mut in_rx) = mpsc::unbounded_channel::<Result<GatewayMsg, String>>();
        let writer_err = in_tx.clone();
        let writer = tokio::spawn(async move {
            let mut sink = FramedWrite::new(send, frame_codec());
            while let Some(msg) = out_rx.recv().await {
                let body = match encode_frame(&msg) {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!("dropping unsendable message: {e}");
                        continue;
                    }
                };
                if let Err(e) = sink.send(body).await {
                    let _ = writer_err.send(Err(format!("write: {e}")));
                    return;
                }
            }
        });
        let reader = tokio::spawn(async move {
            let mut stream = FramedRead::new(recv, frame_codec());
            loop {
                match stream.next().await {
                    Some(Ok(frame)) => match decode_frame::<GatewayMsg>(&frame) {
                        Ok(m) => {
                            if in_tx.send(Ok(m)).is_err() {
                                return;
                            }
                        }
                        Err(e) => {
                            let _ = in_tx.send(Err(format!("decode: {e}")));
                            return;
                        }
                    },
                    Some(Err(e)) => {
                        let _ = in_tx.send(Err(format!("read: {e}")));
                        return;
                    }
                    None => {
                        let _ = in_tx.send(Err("closed by peer".into()));
                        return;
                    }
                }
            }
        });
        let _guard = SessionGuard { conn: conn.clone(), tasks: vec![writer, reader] };

        let mut sess = Session { out: out_tx, established: false, uploads: HashMap::new() };
        let _ = sess.out.send(PhoneMsg::Hello {
            proto_version: REMOTE_PROTO_VERSION,
            min_compatible: MIN_COMPATIBLE,
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            device_name: self.cfg.device_name.clone(),
            capabilities: vec![],
            pair_secret,
        });

        let mut ping = tokio::time::interval(PING_INTERVAL);
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        ping.tick().await; // the first tick is immediate
        let mut last_rx = Instant::now();
        let mut hs_deadline = Instant::now() + HANDSHAKE_TIMEOUT;
        let mut pair_pending = false;
        let mut probe: Option<Instant> = None;

        loop {
            tokio::select! {
                msg = in_rx.recv() => {
                    let msg = match msg {
                        None => return End::Lost("reader stopped".into()),
                        Some(Err(e)) => return End::Lost(e),
                        Some(Ok(m)) => m,
                    };
                    last_rx = Instant::now();
                    probe = None;
                    match msg {
                        GatewayMsg::Welcome { proto_version, min_compatible, host_name, .. } => {
                            if !compatible(proto_version, min_compatible) {
                                return End::Rejected {
                                    reason: "incompatible".into(),
                                    detail: Some(format!("gateway {proto_version}/{min_compatible}")),
                                };
                            }
                            sess.established = true;
                            self.backoff.reset();
                            let name = self.on_welcome(target, &conn, &host_name);
                            self.set_state(ConnState::Connected { host_name: name });
                            if let Some(sub) = &self.sub {
                                sess.send(PhoneMsg::Subscribe {
                                    term: sub.term.clone(),
                                    cols: sub.cols,
                                    rows: sub.rows,
                                    fit: sub.fit,
                                    replay_bytes: REPLAY_BYTES,
                                });
                            }
                        }
                        GatewayMsg::Rejected { reason } => {
                            return End::Rejected { reason: reject_code(&reason).into(), detail: None };
                        }
                        GatewayMsg::PairPending => {
                            pair_pending = true;
                            hs_deadline = Instant::now() + PAIR_CONFIRM_TIMEOUT;
                            self.set_state(ConnState::PairPending);
                        }
                        GatewayMsg::Pong { .. } => {}
                        GatewayMsg::Workspace(ws) => {
                            self.ui.workspace(&ws);
                            *self.workspace.lock().unwrap() = Some(ws);
                        }
                        GatewayMsg::Reset { term, cols, rows } => {
                            if let Some(sub) = self.sub.as_ref().filter(|s| s.term == term) {
                                sub.sink.reset(cols, rows);
                            }
                        }
                        GatewayMsg::Output { term, bytes, .. } => {
                            if let Some(sub) = self.sub.as_ref().filter(|s| s.term == term) {
                                sub.sink.output(bytes);
                            }
                        }
                        GatewayMsg::TermSize { term, cols, rows, fit } => self.ui.term_size(&term, cols, rows, fit),
                        GatewayMsg::Uploaded { req_id, result } => {
                            if let Some(reply) = sess.uploads.remove(&req_id) {
                                let _ = reply.send(result);
                            }
                        }
                        GatewayMsg::Exited { term, code } => self.ui.term_exited(&term, code),
                        GatewayMsg::Error { message } => self.ui.gateway_error(&message),
                        // TODO: chat handled in mobile chat module
                        GatewayMsg::ChatSnapshot { .. } | GatewayMsg::ChatUpdate { .. } | GatewayMsg::ChatEarlier { .. } | GatewayMsg::ChatImageData { .. } => {}
                    }
                }
                cmd = rx.recv() => {
                    let Some(cmd) = cmd else { return End::Closed };
                    match self.handle_cmd(cmd, Some(&mut sess)) {
                        Flow::Continue => {}
                        Flow::Restart => return End::Switched,
                        Flow::Probe => {
                            ep.network_change().await;
                            let nonce = self.next_nonce;
                            self.next_nonce += 1;
                            sess.send(PhoneMsg::Ping { nonce });
                            probe = Some(Instant::now() + RESUME_PROBE);
                        }
                    }
                }
                _ = ping.tick() => {
                    if sess.established {
                        if is_dead(last_rx.elapsed()) {
                            return End::Lost("no response from gateway".into());
                        }
                        let nonce = self.next_nonce;
                        self.next_nonce += 1;
                        sess.send(PhoneMsg::Ping { nonce });
                    }
                }
                _ = tokio::time::sleep_until(hs_deadline), if !sess.established => {
                    if pair_pending {
                        return End::Rejected { reason: "pair_timeout".into(), detail: None };
                    }
                    return End::Lost("handshake timed out".into());
                }
                _ = tokio::time::sleep_until(probe.unwrap_or_else(Instant::now)), if probe.is_some() => {
                    return End::Lost("link dead after resume".into());
                }
            }
        }
    }

    /// Store the host after a successful pairing; returns the display name.
    fn on_welcome(&mut self, target: &Target, conn: &Connection, host_name: &str) -> String {
        match target {
            Target::Host(h) => {
                if host_name.is_empty() { h.name.clone() } else { host_name.to_string() }
            }
            Target::Pair(p) => {
                let name = if !host_name.is_empty() {
                    host_name.to_string()
                } else if !p.host.is_empty() {
                    p.host.clone()
                } else {
                    "PC".to_string()
                };
                let endpoint_id = conn.remote_id().to_string();
                let host = Host {
                    id: endpoint_id.clone(),
                    name: name.clone(),
                    endpoint_id,
                    addrs: p.addrs.clone(),
                    relay: p.relay.clone(),
                    paired_unix_ms: now_ms(),
                };
                {
                    let mut store = self.store.lock().unwrap();
                    if let Err(e) = store.upsert(host.clone()).and_then(|_| store.set_active(&host.id).map(|_| ())) {
                        tracing::error!("saving host failed: {e}");
                    }
                }
                self.target = Some(Target::Host(host));
                self.ui.hosts_changed();
                name
            }
        }
    }
}

#[cfg(test)]
mod tests;
