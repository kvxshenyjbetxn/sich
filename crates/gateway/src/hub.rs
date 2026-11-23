//! Shared gateway state: the daemon pool, streams, fit state, connected phones, pairing and
//! uploads. Phone handlers and the control server call into it; it never blocks on a phone.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use iroh::Endpoint;
use parking_lot::Mutex;
use sich_remote_proto::{GatewayMsg, PairUrl, TermRef, Workspace};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

use crate::chat::{ChatHandle, Resolver, locate};
use crate::compose;
use crate::control::{PairRequest, Status};
use crate::convert;
use crate::fit::FitTracker;
use crate::outbox::Outbox;
use crate::pairing::{Pairing, generate_secret};
use crate::pool::{Pool, PoolEvent};
use crate::store::{Device, Store};
use crate::stream::TermStream;
use crate::uploads;

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[derive(Debug, PartialEq, Eq)]
pub enum PairDecision {
    Allowed,
    Denied,
    Timeout,
}

struct PhoneEntry {
    endpoint_id: String,
    outbox: Arc<Outbox>,
    cancel: CancellationToken,
}

struct Requester {
    conn_id: u64,
    tx: mpsc::UnboundedSender<PairRequest>,
}

/// What a connected phone has open. Owned by its connection handler.
pub struct Session {
    pub id: u64,
    pub endpoint_id: String,
    pub outbox: Arc<Outbox>,
    /// Watched terminals with the phone's viewport size.
    subs: HashMap<TermRef, (u16, u16)>,
    /// Open chat views (dropping a handle stops its watcher).
    chats: HashMap<TermRef, ChatHandle>,
}

pub struct HubConfig {
    pub host_name: String,
    pub build_id: String,
    pub upload_dir: PathBuf,
    pub relay_disabled: bool,
    pub confirm_timeout: Duration,
}

pub struct Hub {
    cfg: HubConfig,
    pub endpoint: Endpoint,
    pub pool: Pool,
    pub store: Mutex<Store>,
    pub pairing: Mutex<Pairing>,
    streams: Mutex<HashMap<TermRef, TermStream>>,
    fit: Mutex<FitTracker>,
    phones: Mutex<HashMap<u64, PhoneEntry>>,
    next_id: AtomicU64,
    uploads: Mutex<HashMap<String, PathBuf>>,
    requester: Mutex<Option<Requester>>,
    waiters: Mutex<HashMap<u64, oneshot::Sender<bool>>>,
    changed: watch::Receiver<u64>,
    pub shutdown: CancellationToken,
}

/// Direct addresses worth putting into a QR code: no wildcard or IPv6 link-local addresses, and
/// loopback only when there is no relay (tests, LAN-only).
pub fn filter_addrs(addrs: &[SocketAddr], relay_disabled: bool) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for addr in addrs {
        let ip = addr.ip();
        let skip = ip.is_unspecified()
            || (ip.is_loopback() && !relay_disabled)
            || matches!(ip, IpAddr::V6(v6) if (v6.segments()[0] & 0xffc0) == 0xfe80)
            || matches!(ip, IpAddr::V4(v4) if v4.is_link_local());
        let text = addr.to_string();
        if !skip && !out.contains(&text) {
            out.push(text);
        }
    }
    out
}

/// Strips control characters and caps the length of a phone-supplied device name.
pub fn sanitize_name(name: &str) -> String {
    let clean: String = name.chars().filter(|c| !c.is_control()).collect();
    let clean: String = clean.trim().chars().take(64).collect();
    if clean.is_empty() { "Phone".to_string() } else { clean }
}

impl Hub {
    pub fn new(
        cfg: HubConfig,
        endpoint: Endpoint,
        pool: Pool,
        store: Store,
        changed: watch::Receiver<u64>,
        shutdown: CancellationToken,
    ) -> Arc<Hub> {
        Arc::new(Hub {
            cfg,
            endpoint,
            pool,
            store: Mutex::new(store),
            pairing: Mutex::default(),
            streams: Mutex::default(),
            fit: Mutex::default(),
            phones: Mutex::default(),
            next_id: AtomicU64::new(1),
            uploads: Mutex::default(),
            requester: Mutex::default(),
            waiters: Mutex::default(),
            changed,
            shutdown,
        })
    }

    pub fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    pub fn host_name(&self) -> &str {
        &self.cfg.host_name
    }

    pub fn build_id(&self) -> &str {
        &self.cfg.build_id
    }

    pub fn changes(&self) -> watch::Receiver<u64> {
        self.changed.clone()
    }

    // ----- background -----

    /// Pumps daemon events and checks fit conflicts until shutdown.
    pub async fn run_background(self: Arc<Self>, mut events: mpsc::UnboundedReceiver<PoolEvent>) {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                _ = self.shutdown.cancelled() => break,
                ev = events.recv() => match ev {
                    Some(ev) => self.on_event(ev),
                    None => break,
                },
                _ = tick.tick() => self.check_fit_conflicts(),
            }
        }
    }

    fn on_event(&self, ev: PoolEvent) {
        match ev {
            PoolEvent::Output { term, seq, bytes } => {
                let (cols, rows) = self.pool.terminal(&term).map_or((80, 24), |t| (t.cols, t.rows));
                if let Some(stream) = self.streams.lock().get_mut(&term) {
                    stream.feed(&term, seq, &bytes, cols, rows);
                }
            }
            PoolEvent::Exited { term, code } => {
                if let Some(stream) = self.streams.lock().get(&term) {
                    stream.each(|_, out| out.push(GatewayMsg::Exited { term: term.clone(), code }));
                }
            }
            PoolEvent::Removed { term } => {
                if let Some(stream) = self.streams.lock().remove(&term) {
                    stream.each(|_, out| out.push(GatewayMsg::Exited { term: term.clone(), code: None }));
                }
                self.fit.lock().forget(&term);
            }
        }
    }

    fn check_fit_conflicts(&self) {
        let terms = self.fit.lock().terms();
        for term in terms {
            let Some(info) = self.pool.terminal(&term) else { continue };
            let current = (info.cols, info.rows);
            let dropped = self.fit.lock().observe(&term, current);
            if dropped.is_empty() {
                continue;
            }
            tracing::info!(daemon = %term.daemon, id = term.id, "terminal resized by someone else, fit-to-phone off");
            let phones = self.phones.lock();
            for id in dropped {
                if let Some(p) = phones.get(&id) {
                    p.outbox.push(GatewayMsg::TermSize { term: term.clone(), cols: current.0, rows: current.1, fit: false });
                }
            }
        }
    }

    // ----- phones -----

    pub fn register_phone(&self, endpoint_id: &str) -> (Session, CancellationToken) {
        let id = self.next_id();
        let outbox = Arc::new(Outbox::default());
        let cancel = CancellationToken::new();
        self.phones.lock().insert(
            id,
            PhoneEntry { endpoint_id: endpoint_id.to_string(), outbox: outbox.clone(), cancel: cancel.clone() },
        );
        (Session { id, endpoint_id: endpoint_id.to_string(), outbox, subs: HashMap::new(), chats: HashMap::new() }, cancel)
    }

    pub fn unregister_phone(&self, session: &mut Session) {
        session.chats.clear();
        let terms: Vec<TermRef> = session.subs.keys().cloned().collect();
        for term in terms {
            self.unsubscribe(session, &term);
        }
        self.phones.lock().remove(&session.id);
        session.outbox.close();
        self.store.lock().touch(&session.endpoint_id, now_ms());
    }

    pub fn connected_count(&self) -> u32 {
        self.phones.lock().len() as u32
    }

    /// Removes a device and drops its live connections.
    pub fn revoke(&self, endpoint_id: &str) -> anyhow::Result<bool> {
        let removed = self.store.lock().remove(endpoint_id)?;
        for phone in self.phones.lock().values().filter(|p| p.endpoint_id == endpoint_id) {
            phone.cancel.cancel();
        }
        Ok(removed)
    }

    // ----- workspace -----

    pub fn workspace(&self) -> Workspace {
        let snap = self.pool.snapshot();
        convert::workspace(&self.cfg.host_name, &snap.terminals, &snap.projects, &snap.groups)
    }

    // ----- terminals -----

    fn size_of(&self, term: &TermRef) -> Option<(u16, u16)> {
        self.pool.terminal(term).map(|t| (t.cols, t.rows))
    }

    fn broadcast_size(&self, term: &TermRef) {
        let Some((cols, rows)) = self.size_of(term) else { return };
        let fit = self.fit.lock();
        if let Some(stream) = self.streams.lock().get(term) {
            stream.each(|phone, out| {
                out.push(GatewayMsg::TermSize { term: term.clone(), cols, rows, fit: fit.is_fitting(term, phone) });
            });
        }
    }

    pub fn subscribe(&self, s: &mut Session, term: TermRef, cols: u16, rows: u16, fit: bool, replay_bytes: u32) {
        let Some(info) = self.pool.terminal(&term) else {
            s.outbox.push(GatewayMsg::Error { message: "no such terminal".into() });
            return;
        };
        let size = (cols.max(1), rows.max(1));
        if s.subs.contains_key(&term) {
            self.unsubscribe(s, &term);
        }
        s.subs.insert(term.clone(), size);
        if fit {
            let resize = self.fit.lock().enable(&term, s.id, size, (info.cols, info.rows));
            if let Some((c, r)) = resize {
                self.pool.resize(&term, c, r);
            }
        }
        let effective = self.fit.lock().applied(&term).filter(|_| fit).unwrap_or((info.cols, info.rows));
        {
            let mut streams = self.streams.lock();
            let is_new = !streams.contains_key(&term);
            streams.entry(term.clone()).or_default().subscribe(&term, s.id, s.outbox.clone(), effective.0, effective.1, replay_bytes);
            if is_new {
                self.pool.watch(&term);
            }
        }
        if let Some(code) = info.exited {
            s.outbox.push(GatewayMsg::Exited { term: term.clone(), code });
        }
        s.outbox.push(GatewayMsg::TermSize { term: term.clone(), cols: effective.0, rows: effective.1, fit });
        if fit {
            self.broadcast_size(&term);
        }
    }

    pub fn unsubscribe(&self, s: &mut Session, term: &TermRef) {
        if s.subs.remove(term).is_none() {
            return;
        }
        {
            let mut streams = self.streams.lock();
            if let Some(stream) = streams.get_mut(term) {
                if stream.unsubscribe(s.id) {
                    streams.remove(term);
                    self.pool.unwatch(term);
                }
            }
        }
        self.release_fit(s.id, term);
    }

    fn release_fit(&self, phone: u64, term: &TermRef) {
        let current = self.size_of(term).unwrap_or((80, 24));
        let restore = self.fit.lock().disable(term, phone, current);
        if let Some((c, r)) = restore {
            self.pool.resize(term, c, r);
            self.broadcast_size(term);
        }
    }

    pub fn set_fit(&self, s: &mut Session, term: &TermRef, fit: bool) {
        let Some(&size) = s.subs.get(term) else {
            s.outbox.push(GatewayMsg::Error { message: "terminal is not open".into() });
            return;
        };
        if fit {
            let current = self.size_of(term).unwrap_or(size);
            let resize = self.fit.lock().enable(term, s.id, size, current);
            if let Some((c, r)) = resize {
                self.pool.resize(term, c, r);
            }
        } else {
            self.release_fit(s.id, term);
        }
        let (cols, rows) = self.size_of(term).unwrap_or(size);
        s.outbox.push(GatewayMsg::TermSize { term: term.clone(), cols, rows, fit });
        self.broadcast_size(term);
    }

    pub fn resize(&self, s: &mut Session, term: &TermRef, cols: u16, rows: u16) {
        let Some(slot) = s.subs.get_mut(term) else { return };
        *slot = (cols.max(1), rows.max(1));
        let size = *slot;
        let resize = self.fit.lock().resize(term, s.id, size);
        if let Some((c, r)) = resize {
            self.pool.resize(term, c, r);
            self.broadcast_size(term);
        }
    }

    pub fn input(&self, term: &TermRef, bytes: Vec<u8>) {
        self.pool.input(term, bytes);
    }

    pub fn mark_seen(&self, term: &TermRef) {
        self.pool.mark_seen(term);
    }

    /// Composer message. Returns an error text for the phone when nothing was sent.
    pub fn send_text(self: &Arc<Self>, term: &TermRef, text: &str, images: &[String], submit: bool) -> Result<(), String> {
        let Some(info) = self.pool.terminal(term) else { return Err("no such terminal".into()) };
        let paths = {
            let known = self.uploads.lock();
            let mut paths = Vec::new();
            for id in images {
                let path = known.get(id).ok_or_else(|| format!("unknown image {id}"))?;
                paths.push(path.to_string_lossy().into_owned());
            }
            paths
        };
        let kind = info.agent.as_ref().map(|a| a.kind);
        let plan = compose::plan(text, &paths, kind, submit);
        if !plan.payload.is_empty() {
            self.pool.input(term, plan.payload);
        }
        if plan.submit {
            let hub = self.clone();
            let term = term.clone();
            tokio::spawn(async move {
                tokio::time::sleep(compose::SUBMIT_DELAY).await;
                hub.pool.input(&term, b"\r".to_vec());
            });
        }
        Ok(())
    }

    /// Saves an uploaded image; the id is the file name.
    pub async fn upload(&self, data: Vec<u8>) -> Result<String, String> {
        let dir = self.cfg.upload_dir.clone();
        let path = tokio::task::spawn_blocking(move || uploads::save(&dir, &data, now_ms()))
            .await
            .map_err(|e| e.to_string())??;
        let id = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        self.uploads.lock().insert(id.clone(), path);
        Ok(id)
    }

    // ----- chat view -----

    /// Where the transcript of `term`'s agent is, asked again whenever the watcher rechecks.
    fn chat_resolver(&self, term: &TermRef) -> Resolver {
        let pool = self.pool.clone();
        let term = term.clone();
        Arc::new(move || {
            let Some(info) = pool.terminal(&term) else {
                return crate::chat::Target::Unavailable("no such terminal".into());
            };
            let (kind, session) = match &info.agent {
                Some(a) => (Some(a.kind), a.session_id.as_deref()),
                None => (None, None),
            };
            locate::resolve(&locate::Dirs::from_env(), kind, session, Some(info.cwd.as_path()))
        })
    }

    pub fn chat_subscribe(&self, s: &mut Session, term: TermRef, limit: u32) {
        if self.pool.terminal(&term).is_none() {
            s.outbox.push(GatewayMsg::Error { message: "no such terminal".into() });
            return;
        }
        let handle = ChatHandle::spawn(term.clone(), limit, self.chat_resolver(&term), s.outbox.clone());
        // Replacing an old handle stops its watcher.
        s.chats.insert(term, handle);
    }

    pub fn chat_unsubscribe(&self, s: &mut Session, term: &TermRef) {
        s.chats.remove(term);
    }

    pub fn chat_load_earlier(&self, s: &Session, term: &TermRef, before_order: u64, limit: u32) {
        match s.chats.get(term) {
            Some(chat) => chat.load_earlier(before_order, limit),
            None => s.outbox.push(GatewayMsg::ChatEarlier { term: term.clone(), messages: Vec::new(), has_more: false }),
        }
    }

    pub fn chat_image(&self, s: &Session, req_id: u64, term: &TermRef, image_ref: String) {
        match s.chats.get(term) {
            Some(chat) => chat.image(req_id, image_ref),
            None => s.outbox.push(GatewayMsg::ChatImageData { req_id, result: Err("the chat is not open".into()) }),
        }
    }

    // ----- pairing -----

    /// Starts a pairing round for the control connection `conn_id`. Pair requests are delivered
    /// to `tx`. Returns the URL for the QR code and its expiry.
    pub async fn start_pairing(&self, conn_id: u64, tx: mpsc::UnboundedSender<PairRequest>) -> (String, u64) {
        let secret = generate_secret();
        let expires = self.pairing.lock().start(secret.clone(), Instant::now());
        let expires_ms = now_ms() + expires.saturating_duration_since(Instant::now()).as_millis() as u64;
        *self.requester.lock() = Some(Requester { conn_id, tx });
        (self.pair_url(&secret).await, expires_ms)
    }

    /// The control connection that started pairing went away.
    pub fn control_closed(&self, conn_id: u64) {
        let mut requester = self.requester.lock();
        if requester.as_ref().is_some_and(|r| r.conn_id == conn_id) {
            *requester = None;
            self.pairing.lock().cancel();
            self.waiters.lock().clear();
        }
    }

    pub fn answer_pair(&self, request_id: u64, allow: bool) -> bool {
        self.waiters.lock().remove(&request_id).is_some_and(|tx| tx.send(allow).is_ok())
    }

    /// Asks the desktop user (through the control client that started pairing).
    pub async fn confirm_pair(&self, device_name: &str, endpoint_id: &str) -> PairDecision {
        let Some(tx) = self.requester.lock().as_ref().map(|r| r.tx.clone()) else { return PairDecision::Denied };
        let (answer_tx, answer_rx) = oneshot::channel();
        let request_id = self.next_id();
        self.waiters.lock().insert(request_id, answer_tx);
        let request = PairRequest { request_id, device_name: device_name.to_string(), endpoint_id: endpoint_id.to_string() };
        if tx.send(request).is_err() {
            self.waiters.lock().remove(&request_id);
            return PairDecision::Denied;
        }
        match tokio::time::timeout(self.cfg.confirm_timeout, answer_rx).await {
            Ok(Ok(true)) => PairDecision::Allowed,
            Ok(_) => PairDecision::Denied,
            Err(_) => {
                self.waiters.lock().remove(&request_id);
                PairDecision::Timeout
            }
        }
    }

    pub fn add_device(&self, endpoint_id: &str, name: &str) -> anyhow::Result<()> {
        let now = now_ms();
        self.store.lock().add(Device {
            endpoint_id: endpoint_id.to_string(),
            name: name.to_string(),
            paired_unix_ms: now,
            last_seen_unix_ms: now,
        })
    }

    pub fn is_paired(&self, endpoint_id: &str) -> bool {
        self.store.lock().is_paired(endpoint_id)
    }

    // ----- addresses / status -----

    fn direct_addrs(&self) -> Vec<String> {
        let addr = self.endpoint.addr();
        let ips: Vec<SocketAddr> = addr.ip_addrs().copied().collect();
        let mut out = filter_addrs(&ips, self.cfg.relay_disabled);
        if self.cfg.relay_disabled {
            // The bound sockets are wildcards; for LAN-only/tests make loopback reachable too.
            let ports: HashSet<u16> = self.endpoint.bound_sockets().iter().filter(|a| a.is_ipv4()).map(|a| a.port()).collect();
            for port in ports {
                let lo = format!("127.0.0.1:{port}");
                if !out.contains(&lo) {
                    out.push(lo);
                }
            }
        }
        out
    }

    async fn pair_url(&self, secret: &str) -> String {
        if !self.cfg.relay_disabled {
            let _ = tokio::time::timeout(Duration::from_secs(3), self.endpoint.online()).await;
        }
        for _ in 0..20 {
            if !self.direct_addrs().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let relay = self.endpoint.addr().relay_urls().next().map(|u| u.to_string());
        PairUrl {
            endpoint_id: self.endpoint.id().to_string(),
            addrs: self.direct_addrs(),
            relay,
            secret: secret.to_string(),
            host: self.cfg.host_name.clone(),
        }
        .to_url()
    }

    pub fn status(&self) -> Status {
        let (devices, device_list_error) = {
            let store = self.store.lock();
            (store.devices().to_vec(), store.poisoned().map(str::to_string))
        };
        Status {
            endpoint_id: self.endpoint.id().to_string(),
            host_name: self.cfg.host_name.clone(),
            build_id: self.cfg.build_id.clone(),
            gateway_version: env!("CARGO_PKG_VERSION").to_string(),
            devices,
            connected: self.connected_count(),
            addrs: self.direct_addrs(),
            relay: self.endpoint.addr().relay_urls().next().map(|u| u.to_string()),
            port: crate::pick_port(&self.endpoint.bound_sockets()),
            device_list_error,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn addrs_drop_wildcard_linklocal_and_loopback() {
        let all = [
            a("0.0.0.0:5"),
            a("127.0.0.1:5"),
            a("192.168.1.5:5"),
            a("169.254.1.1:5"),
            a("[fe80::1]:5"),
            a("[::1]:5"),
            a("[2001:db8::1]:5"),
            a("192.168.1.5:5"),
        ];
        assert_eq!(filter_addrs(&all, false), vec!["192.168.1.5:5", "[2001:db8::1]:5"]);
        assert_eq!(filter_addrs(&all, true), vec!["127.0.0.1:5", "192.168.1.5:5", "[::1]:5", "[2001:db8::1]:5"]);
    }

    #[test]
    fn device_names_are_cleaned() {
        assert_eq!(sanitize_name("  Pixel 8\n\u{7}"), "Pixel 8");
        assert_eq!(sanitize_name(""), "Phone");
        assert_eq!(sanitize_name("\n\t"), "Phone");
        assert_eq!(sanitize_name(&"x".repeat(200)).chars().count(), 64);
    }
}
