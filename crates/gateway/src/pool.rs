//! Daemon generations aggregated like the GUI's `DaemonPool`: every registered daemon of our base
//! with a matching protocol version gets a connection that mirrors its terminals/projects and
//! forwards watched terminals' output.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use futures::{SinkExt as _, StreamExt as _};
use parking_lot::Mutex;
use sich_daemon::generation;
use sich_proto::{ClientMsg, GroupInfo, PROTO_VERSION, ProjectInfo, ServerMsg, TermId, TerminalInfo, decode, encode, frame_codec};
use sich_remote_proto::TermRef;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::codec::Framed;
use tokio_util::sync::CancellationToken;

use crate::convert::term_ref;

const BACKOFF_MIN: Duration = Duration::from_millis(250);
const BACKOFF_MAX: Duration = Duration::from_secs(2);
const REGISTRY_TICK: Duration = Duration::from_millis(500);
const LIST_POLL: Duration = Duration::from_secs(2);
/// An idle old generation is not probed again for this long.
const IDLE_RETRY: Duration = Duration::from_secs(30);

pub enum PoolEvent {
    Output { term: TermRef, seq: u64, bytes: Vec<u8> },
    Exited { term: TermRef, code: Option<i32> },
    Removed { term: TermRef },
}

enum LinkCmd {
    Watch(TermId),
    Unwatch(TermId),
    Input(TermId, Vec<u8>),
    Resize(TermId, u16, u16),
    MarkSeen(TermId),
}

#[derive(Default)]
struct LinkState {
    terminals: BTreeMap<TermId, TerminalInfo>,
    projects: Option<(Vec<ProjectInfo>, Vec<GroupInfo>)>,
}

struct Link {
    started_ms: u64,
    state: Arc<Mutex<LinkState>>,
    cmd: mpsc::UnboundedSender<LinkCmd>,
    task: JoinHandle<()>,
}

impl Drop for Link {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Inner {
    base: String,
    links: Mutex<HashMap<String, Link>>,
    idle_until: Mutex<HashMap<String, Instant>>,
    newest: Arc<Mutex<Option<String>>>,
    events: mpsc::UnboundedSender<PoolEvent>,
    changed: watch::Sender<u64>,
    cancel: CancellationToken,
}

#[derive(Clone)]
pub struct Pool(Arc<Inner>);

#[derive(Default, Debug)]
pub struct Snapshot {
    /// `(daemon socket name, terminal)`.
    pub terminals: Vec<(String, TerminalInfo)>,
    pub projects: Vec<ProjectInfo>,
    pub groups: Vec<GroupInfo>,
}

fn bump(changed: &watch::Sender<u64>) {
    changed.send_modify(|v| *v += 1);
}

impl Pool {
    /// Starts discovering and connecting to the daemons of `base`. The receiver changes whenever
    /// anything the phone can see may have changed.
    pub fn start(base: String, events: mpsc::UnboundedSender<PoolEvent>) -> (Pool, watch::Receiver<u64>) {
        let (changed, rx) = watch::channel(0);
        let inner = Arc::new(Inner {
            base,
            links: Mutex::default(),
            idle_until: Mutex::default(),
            newest: Arc::default(),
            events,
            changed,
            cancel: CancellationToken::new(),
        });
        let pool = Pool(inner.clone());
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(REGISTRY_TICK);
            loop {
                tokio::select! {
                    _ = inner.cancel.cancelled() => break,
                    _ = tick.tick() => inner.refresh(&generation::entries()),
                }
            }
            inner.links.lock().clear();
        });
        (pool, rx)
    }

    pub fn shutdown(&self) {
        self.0.cancel.cancel();
        self.0.links.lock().clear();
    }

    pub fn snapshot(&self) -> Snapshot {
        let links = self.0.links.lock();
        let mut sockets: Vec<&String> = links.keys().collect();
        sockets.sort();
        let mut snap = Snapshot::default();
        for socket in &sockets {
            for info in links[*socket].state.lock().terminals.values() {
                snap.terminals.push(((*socket).clone(), info.clone()));
            }
        }
        // Projects and groups come from the newest generation that has told us about them.
        let newest = self.0.newest.lock().clone();
        let source = newest
            .as_ref()
            .and_then(|s| links.get(s).filter(|l| l.state.lock().projects.is_some()))
            .or_else(|| links.values().filter(|l| l.state.lock().projects.is_some()).max_by_key(|l| l.started_ms));
        if let Some((projects, groups)) = source.and_then(|l| l.state.lock().projects.clone()) {
            snap.projects = projects;
            snap.groups = groups;
        }
        snap
    }

    pub fn terminal(&self, term: &TermRef) -> Option<TerminalInfo> {
        self.0.links.lock().get(&term.daemon)?.state.lock().terminals.get(&term.id).cloned()
    }

    fn send(&self, term: &TermRef, cmd: impl FnOnce(TermId) -> LinkCmd) {
        if let Some(link) = self.0.links.lock().get(&term.daemon) {
            let _ = link.cmd.send(cmd(term.id));
        }
    }

    pub fn watch(&self, term: &TermRef) {
        self.send(term, LinkCmd::Watch);
    }

    pub fn unwatch(&self, term: &TermRef) {
        self.send(term, LinkCmd::Unwatch);
    }

    pub fn input(&self, term: &TermRef, bytes: Vec<u8>) {
        self.send(term, |id| LinkCmd::Input(id, bytes));
    }

    /// Resizes the PTY. The daemon does not announce resizes, so the mirrored size is updated
    /// right away.
    pub fn resize(&self, term: &TermRef, cols: u16, rows: u16) {
        let (cols, rows) = (cols.max(1), rows.max(1));
        if let Some(link) = self.0.links.lock().get(&term.daemon) {
            if let Some(t) = link.state.lock().terminals.get_mut(&term.id) {
                t.cols = cols;
                t.rows = rows;
            }
            let _ = link.cmd.send(LinkCmd::Resize(term.id, cols, rows));
        }
        bump(&self.0.changed);
    }

    pub fn mark_seen(&self, term: &TermRef) {
        self.send(term, LinkCmd::MarkSeen);
    }
}

impl Inner {
    /// One registry tick: adopt new generations, drop vanished or idle ones.
    fn refresh(self: &Arc<Self>, entries: &[generation::Entry]) {
        let ours: Vec<&generation::Entry> =
            entries.iter().filter(|e| e.base() == Some(self.base.as_str()) && e.proto_version == PROTO_VERSION).collect();
        *self.newest.lock() = generation::newest_compatible(&self.base, entries).map(|e| e.socket.clone());
        let now = Instant::now();
        let mut links = self.links.lock();
        let mut idle = self.idle_until.lock();
        idle.retain(|socket, until| *until > now && ours.iter().any(|e| &e.socket == socket));

        let mut changed = false;
        links.retain(|socket, link| {
            let listed = ours.iter().any(|e| &e.socket == socket);
            let keep = listed && !link.task.is_finished();
            if !keep {
                if listed {
                    idle.insert(socket.clone(), now + IDLE_RETRY);
                }
                for id in link.state.lock().terminals.keys() {
                    let _ = self.events.send(PoolEvent::Removed { term: term_ref(socket, *id) });
                }
                changed = true;
            }
            keep
        });
        for entry in ours {
            if links.contains_key(&entry.socket) || idle.contains_key(&entry.socket) {
                continue;
            }
            links.insert(entry.socket.clone(), self.spawn_link(entry));
        }
        drop((links, idle));
        if changed {
            bump(&self.changed);
        }
    }

    fn spawn_link(self: &Arc<Self>, entry: &generation::Entry) -> Link {
        let (cmd, cmd_rx) = mpsc::unbounded_channel();
        let state = Arc::new(Mutex::new(LinkState::default()));
        let ctx = LinkCtx {
            socket: entry.socket.clone(),
            omp: entry.omp_status,
            state: state.clone(),
            events: self.events.clone(),
            changed: self.changed.clone(),
            newest: self.newest.clone(),
        };
        let task = tokio::spawn(run_link(ctx, cmd_rx));
        Link { started_ms: entry.started_ms, state, cmd, task }
    }
}

struct LinkCtx {
    socket: String,
    omp: bool,
    state: Arc<Mutex<LinkState>>,
    events: mpsc::UnboundedSender<PoolEvent>,
    changed: watch::Sender<u64>,
    newest: Arc<Mutex<Option<String>>>,
}

enum End {
    /// An old generation without terminals: stop talking to it.
    Idle,
}

/// Reconnects forever with backoff; only returns for an idle old generation.
async fn run_link(ctx: LinkCtx, mut cmd_rx: mpsc::UnboundedReceiver<LinkCmd>) {
    let mut backoff = BACKOFF_MIN;
    let mut watched: HashSet<TermId> = HashSet::new();
    loop {
        match sich_daemon::connect(&ctx.socket).await {
            Ok(stream) => {
                let mut welcomed = false;
                let res = session(&ctx, Framed::new(stream, frame_codec()), &mut cmd_rx, &mut watched, &mut welcomed).await;
                if welcomed {
                    backoff = BACKOFF_MIN;
                }
                match res {
                    Ok(End::Idle) => return,
                    Err(e) => tracing::debug!(socket = %ctx.socket, "daemon connection ended: {e:#}"),
                }
                if !ctx.state.lock().terminals.is_empty() {
                    ctx.state.lock().terminals.clear();
                    bump(&ctx.changed);
                }
            }
            Err(e) => tracing::debug!(socket = %ctx.socket, "daemon connect failed: {e}"),
        }
        let sleep = tokio::time::sleep(backoff);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                _ = &mut sleep => break,
                cmd = cmd_rx.recv() => match cmd {
                    Some(LinkCmd::Watch(id)) => { watched.insert(id); }
                    Some(LinkCmd::Unwatch(id)) => { watched.remove(&id); }
                    Some(_) => {}
                    None => return,
                },
            }
        }
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

type Conn = Framed<sich_daemon::ClientStream, tokio_util::codec::LengthDelimitedCodec>;

async fn send(conn: &mut Conn, msg: &ClientMsg) -> anyhow::Result<()> {
    conn.send(encode(msg)?).await.context("send to daemon")
}

async fn session(
    ctx: &LinkCtx,
    mut conn: Conn,
    cmd_rx: &mut mpsc::UnboundedReceiver<LinkCmd>,
    watched: &mut HashSet<TermId>,
    welcomed: &mut bool,
) -> anyhow::Result<End> {
    let name = if ctx.omp { format!("sich-gateway{}", sich_proto::OMP_CLIENT_SUFFIX) } else { "sich-gateway".to_string() };
    send(&mut conn, &ClientMsg::Hello { proto_version: PROTO_VERSION, client_name: name }).await?;
    let mut poll = tokio::time::interval(LIST_POLL);
    poll.reset();
    loop {
        tokio::select! {
            frame = conn.next() => {
                let frame = frame.context("daemon closed the connection")?.context("read frame")?;
                let msg: ServerMsg = decode(&frame).context("decode server message")?;
                if let ServerMsg::Welcome { proto_version, .. } = &msg {
                    anyhow::ensure!(*proto_version == PROTO_VERSION, "daemon speaks protocol {proto_version}");
                    *welcomed = true;
                    send(&mut conn, &ClientMsg::ListTerminals).await?;
                    send(&mut conn, &ClientMsg::ListProjects).await?;
                    for id in watched.iter() {
                        send(&mut conn, &ClientMsg::Subscribe { id: *id, resume_from_seq: None }).await?;
                    }
                    continue;
                }
                anyhow::ensure!(*welcomed, "unexpected message before Welcome: {msg:?}");
                if apply(ctx, msg, watched) {
                    return Ok(End::Idle);
                }
            }
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { anyhow::bail!("pool closed") };
                match cmd {
                    LinkCmd::Watch(id) => {
                        if watched.insert(id) {
                            send(&mut conn, &ClientMsg::Subscribe { id, resume_from_seq: None }).await?;
                        }
                    }
                    LinkCmd::Unwatch(id) => {
                        if watched.remove(&id) {
                            send(&mut conn, &ClientMsg::Unsubscribe { id }).await?;
                        }
                    }
                    LinkCmd::Input(id, bytes) => send(&mut conn, &ClientMsg::Input { id, bytes }).await?,
                    LinkCmd::Resize(id, cols, rows) => send(&mut conn, &ClientMsg::Resize { id, cols, rows }).await?,
                    LinkCmd::MarkSeen(id) => send(&mut conn, &ClientMsg::MarkSeen { id }).await?,
                }
            }
            _ = poll.tick() => {
                // Sizes and other attributes can change without a broadcast (GUI resizes).
                if *welcomed {
                    send(&mut conn, &ClientMsg::ListTerminals).await?;
                }
            }
        }
    }
}

/// Applies a daemon message to the mirrored state. Returns true when the link should stop
/// (an old generation that has no terminals).
fn apply(ctx: &LinkCtx, msg: ServerMsg, watched: &HashSet<TermId>) -> bool {
    let term = |id| term_ref(&ctx.socket, id);
    match msg {
        ServerMsg::Terminals(list) => {
            let new: BTreeMap<TermId, TerminalInfo> = list.into_iter().map(|t| (t.id, t)).collect();
            let (changed, removed) = {
                let mut st = ctx.state.lock();
                let removed: Vec<TermId> = st.terminals.keys().filter(|id| !new.contains_key(id)).copied().collect();
                let changed = st.terminals != new;
                let empty = new.is_empty();
                st.terminals = new;
                if empty && ctx.newest.lock().as_ref().is_some_and(|n| n != &ctx.socket) {
                    return true;
                }
                (changed, removed)
            };
            for id in removed {
                let _ = ctx.events.send(PoolEvent::Removed { term: term(id) });
            }
            if changed {
                bump(&ctx.changed);
            }
        }
        ServerMsg::TerminalCreated(info) => {
            ctx.state.lock().terminals.insert(info.id, info);
            bump(&ctx.changed);
        }
        ServerMsg::TerminalExited { id, code } => {
            if let Some(t) = ctx.state.lock().terminals.get_mut(&id) {
                t.exited = Some(code);
            }
            let _ = ctx.events.send(PoolEvent::Exited { term: term(id), code });
            bump(&ctx.changed);
        }
        ServerMsg::TerminalRemoved { id } => {
            ctx.state.lock().terminals.remove(&id);
            let _ = ctx.events.send(PoolEvent::Removed { term: term(id) });
            bump(&ctx.changed);
        }
        ServerMsg::Output { id, seq, bytes } => {
            if watched.contains(&id) && !bytes.is_empty() {
                let _ = ctx.events.send(PoolEvent::Output { term: term(id), seq, bytes });
            }
        }
        ServerMsg::Projects { projects, groups } => {
            ctx.state.lock().projects = Some((projects, groups));
            bump(&ctx.changed);
        }
        ServerMsg::AgentStatusChanged { id, status } => {
            let mut st = ctx.state.lock();
            if let Some(t) = st.terminals.get_mut(&id) {
                if t.agent != status {
                    t.agent = status;
                    drop(st);
                    bump(&ctx.changed);
                }
            }
        }
        ServerMsg::TerminalTitle { id, title } => {
            let mut st = ctx.state.lock();
            if let Some(t) = st.terminals.get_mut(&id) {
                if t.title != title {
                    t.title = title;
                    drop(st);
                    bump(&ctx.changed);
                }
            }
        }
        ServerMsg::OpFailed { op, message } => tracing::warn!(socket = %ctx.socket, "daemon op {op} failed: {message}"),
        ServerMsg::Error { message } => tracing::warn!(socket = %ctx.socket, "daemon error: {message}"),
        _ => {}
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(base: &str, build: &str, proto: u32, started: u64) -> generation::Entry {
        generation::Entry {
            pid: std::process::id(),
            socket: generation::socket(base, build),
            daemon_build_id: build.into(),
            proto_version: proto,
            started_ms: started,
            omp_status: true,
            subagent_activity: true,
            subagent_models: true,
        }
    }

    fn pool(base: &str) -> (Arc<Inner>, mpsc::UnboundedReceiver<PoolEvent>) {
        let (events, rx) = mpsc::unbounded_channel();
        let (changed, _) = watch::channel(0);
        let inner = Arc::new(Inner {
            base: base.into(),
            links: Mutex::default(),
            idle_until: Mutex::default(),
            newest: Arc::default(),
            events,
            changed,
            cancel: CancellationToken::new(),
        });
        (inner, rx)
    }

    #[tokio::test]
    async fn refresh_adopts_only_matching_base_and_protocol_and_drops_vanished() {
        let (inner, _rx) = pool("unit-base");
        let entries = [
            entry("unit-base", "a", PROTO_VERSION, 1),
            entry("unit-base", "b", PROTO_VERSION, 2),
            entry("unit-base", "old", PROTO_VERSION + 1, 3),
            entry("other-base", "c", PROTO_VERSION, 4),
        ];
        inner.refresh(&entries);
        let mut sockets: Vec<String> = inner.links.lock().keys().cloned().collect();
        sockets.sort();
        assert_eq!(sockets, vec!["unit-base-a".to_string(), "unit-base-b".to_string()]);
        assert_eq!(inner.newest.lock().as_deref(), Some("unit-base-b"));
        inner.refresh(&entries[1..2]);
        assert_eq!(inner.links.lock().keys().cloned().collect::<Vec<_>>(), vec!["unit-base-b".to_string()]);
        inner.links.lock().clear();
    }

    fn ctx(socket: &str, newest: Option<&str>) -> (LinkCtx, mpsc::UnboundedReceiver<PoolEvent>) {
        let (events, rx) = mpsc::unbounded_channel();
        let (changed, _) = watch::channel(0);
        let ctx = LinkCtx {
            socket: socket.into(),
            omp: true,
            state: Arc::default(),
            events,
            changed,
            newest: Arc::new(Mutex::new(newest.map(String::from))),
        };
        (ctx, rx)
    }

    fn info(id: u64) -> TerminalInfo {
        TerminalInfo {
            id,
            title: "t".into(),
            cwd: "/".into(),
            cols: 80,
            rows: 24,
            exited: None,
            created_unix_ms: 1,
            worktree: None,
            agent: None,
        }
    }

    #[test]
    fn apply_tracks_terminal_lifecycle_and_emits_events() {
        let (ctx, mut rx) = ctx("s", Some("s"));
        let watched: HashSet<TermId> = [1].into();
        assert!(!apply(&ctx, ServerMsg::Terminals(vec![info(1), info(2)]), &watched));
        assert_eq!(ctx.state.lock().terminals.len(), 2);
        apply(&ctx, ServerMsg::TerminalExited { id: 1, code: Some(3) }, &watched);
        assert_eq!(ctx.state.lock().terminals[&1].exited, Some(Some(3)));
        assert!(matches!(rx.try_recv(), Ok(PoolEvent::Exited { code: Some(3), .. })));
        apply(&ctx, ServerMsg::TerminalTitle { id: 2, title: "new".into() }, &watched);
        assert_eq!(ctx.state.lock().terminals[&2].title, "new");
        apply(&ctx, ServerMsg::TerminalRemoved { id: 2 }, &watched);
        assert!(matches!(rx.try_recv(), Ok(PoolEvent::Removed { term }) if term.id == 2));
        // A list that no longer contains a terminal removes it too.
        apply(&ctx, ServerMsg::Terminals(vec![]), &watched);
        assert!(matches!(rx.try_recv(), Ok(PoolEvent::Removed { term }) if term.id == 1));
    }

    #[test]
    fn output_is_forwarded_only_for_watched_terminals() {
        let (ctx, mut rx) = ctx("s", Some("s"));
        let watched: HashSet<TermId> = [1].into();
        apply(&ctx, ServerMsg::Output { id: 2, seq: 0, bytes: b"x".to_vec() }, &watched);
        assert!(rx.try_recv().is_err());
        apply(&ctx, ServerMsg::Output { id: 1, seq: 5, bytes: b"hi".to_vec() }, &watched);
        assert!(matches!(rx.try_recv(), Ok(PoolEvent::Output { seq: 5, .. })));
    }

    #[test]
    fn empty_old_generation_stops_but_the_newest_stays() {
        let watched = HashSet::new();
        let (old, _rx) = ctx("old", Some("new"));
        assert!(apply(&old, ServerMsg::Terminals(vec![]), &watched));
        let (newest, _rx2) = ctx("new", Some("new"));
        assert!(!apply(&newest, ServerMsg::Terminals(vec![]), &watched));
        let (unknown, _rx3) = ctx("x", None);
        assert!(!apply(&unknown, ServerMsg::Terminals(vec![]), &watched));
        let (old_busy, _rx4) = ctx("old", Some("new"));
        assert!(!apply(&old_busy, ServerMsg::Terminals(vec![info(1)]), &watched));
    }
}
