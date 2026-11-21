//! Connection task: owns the daemon connection, routes server messages to sessions and
//! reconnects forever with backoff.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use futures::{SinkExt as _, StreamExt as _};
use parking_lot::Mutex;
use sich_proto::{
    AgentStatus, ClientMsg, PROTO_VERSION, ServerMsg, SubagentModel, TermId, TerminalInfo, decode, encode, frame_codec,
};
use sich_term::TermSession;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use crate::daemon_client::{
    ClientState, Cmd, ConnStatus, DEFAULT_COLS, DEFAULT_ROWS, RestartResult, RestoreData, TermEntry, Wake,
};
use crate::workspace::{PendingWorktree, match_pending_worktree};

fn attach_subagent_models(status: &mut AgentStatus, models: Vec<SubagentModel>) {
    for SubagentModel { id, model } in models {
        if let Some(sub) = status.subagents.iter_mut().find(|sub| sub.id == id) {
            sub.model = Some(model);
        }
    }
}

const BACKOFF_MIN: Duration = Duration::from_millis(250);
const BACKOFF_MAX: Duration = Duration::from_secs(2);
const RESUME_DELAY: Duration = Duration::from_millis(1500);

pub(crate) struct Conn {
    pub socket: String,
    pub client_name: String,
    pub rt: Handle,
    pub state: Arc<Mutex<ClientState>>,
    pub cmd_tx: mpsc::UnboundedSender<Cmd>,
    pub wake: Wake,
}

type Stream<S> = Framed<S, LengthDelimitedCodec>;

/// How a connection ended (when it did not fail with an error).
enum End {
    /// The UI went away.
    Closed,
    /// The daemon speaks another protocol version.
    Incompatible,
    Restart(crate::daemon_restart::Request),
}

pub(crate) fn should_retire(legacy: bool, listed: bool, terminal_count: usize) -> bool {
    legacy && listed && terminal_count == 0
}

/// Finds the end of a daemon history replay without a protocol change. The daemon answers a
/// connection's messages in order and replays history synchronously while handling
/// `Subscribe`, so the reply to a `GetDaemonInfo` sent right after a `Subscribe` arrives after
/// the last replayed byte. Every `GetDaemonInfo` gets exactly one `DaemonInfo` reply.
#[derive(Default)]
pub(crate) struct ReplayMarkers {
    /// The terminal of the latest `Subscribe` from the start, not yet followed by a marker.
    pending: Option<TermId>,
    /// One entry per outstanding `GetDaemonInfo`: the terminal whose replay it ends, if any.
    queue: std::collections::VecDeque<Option<TermId>>,
}

impl ReplayMarkers {
    pub(crate) fn on_sent(&mut self, msg: &ClientMsg) {
        match msg {
            ClientMsg::Subscribe { id, resume_from_seq } => {
                self.pending = resume_from_seq.is_none().then_some(*id);
            }
            ClientMsg::GetDaemonInfo => self.queue.push_back(self.pending.take()),
            _ => {}
        }
    }

    /// A `DaemonInfo` arrived: the terminal whose replay just ended, if any.
    pub(crate) fn on_daemon_info(&mut self) -> Option<TermId> {
        self.queue.pop_front().flatten()
    }
}

/// Detects a daemon we cannot talk to: a `Welcome` with another version, or an `Error` before
/// any `Welcome` (an old v1 daemon only answers a version mismatch with `Error`).
pub(crate) fn classify_incompatible(msg: &ServerMsg, welcomed: bool) -> Option<ConnStatus> {
    match msg {
        ServerMsg::Welcome { proto_version, daemon_pid } if *proto_version != PROTO_VERSION => {
            Some(ConnStatus::Incompatible { pid: Some(*daemon_pid), version: Some(*proto_version) })
        }
        ServerMsg::Error { .. } if !welcomed => {
            Some(ConnStatus::Incompatible { pid: None, version: None })
        }
        _ => None,
    }
}

pub(crate) async fn run(conn: Conn, mut cmd_rx: mpsc::UnboundedReceiver<Cmd>, legacy: bool) {
    let mut backoff = BACKOFF_MIN;
    'connect: loop {
        match sich_daemon::connect(&conn.socket).await {
            Ok(stream) => {
                let mut welcomed = false;
                let framed = Framed::new(stream, frame_codec());
                match session_on(&conn, framed, &mut cmd_rx, &mut welcomed, legacy).await {
                    Ok(End::Closed) => return,
                    Ok(End::Incompatible) => {
                        // Stay put until the user confirms a restart.
                        while let Some(cmd) = cmd_rx.recv().await {
                            if let Cmd::Restart(request) = cmd {
                                if conn.restart(request).await {
                                    if legacy {
                                        return;
                                    }
                                    backoff = BACKOFF_MIN;
                                    while cmd_rx.try_recv().is_ok() {}
                                    continue 'connect;
                                }
                            }
                        }
                        return;
                    }
                    Ok(End::Restart(request)) => {
                        conn.restart(request).await;
                        while cmd_rx.try_recv().is_ok() {}
                        backoff = BACKOFF_MIN;
                        continue 'connect;
                    }
                    Err(e) => tracing::warn!("daemon connection lost: {e:#}"),
                }
                if welcomed {
                    backoff = BACKOFF_MIN;
                }
            }
            Err(e) if legacy && matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused) => {
                conn.state.lock().terminals.clear();
                (conn.wake)();
                return;
            }
            Err(e) => tracing::debug!("daemon connect failed: {e}"),
        }
        conn.state.lock().status = ConnStatus::Reconnecting;
        (conn.wake)();

        // Wait before retrying; user commands issued meanwhile are dropped (stale).
        let sleep = tokio::time::sleep(backoff);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                _ = &mut sleep => break,
                cmd = cmd_rx.recv() => match cmd {
                    Some(Cmd::Restart(_)) => conn.restart_failed(anyhow::anyhow!("daemon connection changed before restart")),
                    Some(_) => {}
                    None => return,
                },
            }
        }
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

async fn send<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut Stream<S>, msg: &ClientMsg) -> anyhow::Result<()> {
    stream.send(encode(msg)?).await.context("send to daemon")
}

async fn session_on<S: AsyncRead + AsyncWrite + Unpin>(
    conn: &Conn,
    mut stream: Stream<S>,
    cmd_rx: &mut mpsc::UnboundedReceiver<Cmd>,
    welcomed: &mut bool,
    legacy: bool,
) -> anyhow::Result<End> {
    conn.state.lock().pending_worktrees.clear();
    send(
        &mut stream,
        &ClientMsg::Hello {
            proto_version: PROTO_VERSION,
            client_name: conn.client_name.clone(),
        },
    )
    .await?;

    let incarnation = Arc::new(());
    let mut restores: HashMap<u64, RestoreData> = HashMap::new();
    let mut resumes = HashMap::new();
    let mut next_restore = 0_u64;
    let mut markers = ReplayMarkers::default();
    loop {
        let resume_at = resumes.values().map(|(when, _, _)| *when).min();
        tokio::select! {
            frame = stream.next() => {
                let frame = frame.context("daemon closed the connection")?.context("read frame")?;
                let msg: ServerMsg = decode(&frame).context("decode server message")?;
                if let Some(status) = classify_incompatible(&msg, *welcomed) {
                    conn.state.lock().status = status;
                    (conn.wake)();
                    return Ok(End::Incompatible);
                }
                if matches!(msg, ServerMsg::Welcome { .. }) {
                    *welcomed = true;
                }
                match msg {
                    ServerMsg::TerminalRestored { request_id, terminal } => {
                        if let Some(mut data) = restores.remove(&request_id) {
                            let mut migration = data.migration.take();
                            if let Some(info) = terminal {
                                let id = info.id;
                                let exited = info.exited.is_some();
                                let protect_resume = data.protect_resume;
                                let history = migration.as_mut().map(|m| &*m.history.make_contiguous());
                                let command = conn.restore_terminal(info, data, history);
                                // The replacement is visible now. Don't resume an agent while
                                // its old shell is still open.
                                let can_resume = match migration {
                                    Some(migration) if !migration.done.is_closed() => migration.close_source().await,
                                    Some(_) => false,
                                    None => true,
                                };
                                if let Some(command) = command.filter(|_| !exited && can_resume) {
                                    resumes.insert(id, (tokio::time::Instant::now() + RESUME_DELAY, command, protect_resume));
                                }
                                let sub = ClientMsg::Subscribe { id, resume_from_seq: None };
                                markers.on_sent(&sub);
                                send(&mut stream, &sub).await?;
                            } else if let Some(migration) = migration {
                                let _ = migration.done.send(Err(anyhow::anyhow!("current daemon could not create the replacement terminal")));
                            } else if data.split {
                                conn.state.lock().failed_splits.push(data.token);
                                (conn.wake)();
                            }
                        }
                    }
                    msg => {
                        let listed = matches!(msg, ServerMsg::Terminals(_) | ServerMsg::TerminalsWithSubagentModels { .. });
                        let removed = matches!(msg, ServerMsg::TerminalRemoved { .. });
                        let resume_id = match &msg {
                            ServerMsg::Output { id, bytes, .. } if !bytes.is_empty() => Some(*id),
                            ServerMsg::TerminalExited { id, .. } | ServerMsg::TerminalRemoved { id } => {
                                resumes.remove(id);
                                None
                            }
                            _ => None,
                        };
                        if matches!(msg, ServerMsg::DaemonInfo { .. }) {
                            if let Some(session) = markers.on_daemon_info().and_then(|id| conn.session_of(id)) {
                                session.set_quiet(false);
                            }
                        }
                        for out in conn.on_server(msg)? {
                            markers.on_sent(&out);
                            send(&mut stream, &out).await?;
                        }
                        let terminal_count = conn.state.lock().terminals.len();
                        if should_retire(legacy, listed, terminal_count) {
                            send(&mut stream, &ClientMsg::Shutdown).await?;
                        } else if legacy && removed && terminal_count == 0 {
                            send(&mut stream, &ClientMsg::ListTerminals).await?;
                        }
                        if let Some(id) = resume_id {
                            if let Some((_, bytes, _)) = resumes.remove(&id) {
                                // Terminal replies generated by feed, including ConPTY's initial
                                // cursor-position reply, are already ahead of this command.
                                let _ = conn.cmd_tx.send(Cmd::Resume(incarnation.clone(), id, bytes));
                            }
                        }
                    }
                }
            }
            _ = wait_for_resume(resume_at) => {
                let now = tokio::time::Instant::now();
                let ready: Vec<_> = resumes.iter().filter_map(|(&id, (when, _, _))| (*when <= now).then_some(id)).collect();
                for id in ready {
                    if let Some((_, bytes, _)) = resumes.remove(&id) {
                        let _ = conn.cmd_tx.send(Cmd::Resume(incarnation.clone(), id, bytes));
                    }
                }
            }
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { return Ok(End::Closed) };
                let msg = match cmd {
                    Cmd::Restart(request) => return Ok(End::Restart(request)),
                    Cmd::Restore { worktree, cwd, cols, rows, data } => {
                        if data.migration.as_ref().is_some_and(|migration| migration.done.is_closed()) {
                            continue;
                        }
                        let request_id = next_restore;
                        next_restore += 1;
                        restores.insert(request_id, data);
                        ClientMsg::RestoreTerminal { request_id, worktree, cwd, cols, rows }
                    }
                    Cmd::Create { cols, rows, worktree, cwd } => ClientMsg::CreateTerminal {
                        cwd,
                        shell: None,
                        args: Vec::new(),
                        cols,
                        rows,
                        worktree,
                    },
                    Cmd::RefreshProjects => ClientMsg::RefreshProjects,
                    Cmd::AddProject { path, group } => ClientMsg::AddProject { path, group },
                    Cmd::CreateGroup(name) => ClientMsg::CreateGroup { name },
                    Cmd::RenameGroup(id, name) => ClientMsg::RenameGroup { id, name },
                    Cmd::RemoveGroup(id) => ClientMsg::RemoveGroup { id },
                    Cmd::SetProjectGroup { project_id, group } => {
                        ClientMsg::SetProjectGroup { project_id, group }
                    }
                    Cmd::RemoveProject(id) => ClientMsg::RemoveProject { id },
                    Cmd::CreateWorktree { project_id, name, base } => {
                        conn.state.lock().pending_worktrees.push_back(PendingWorktree { project_id, name: name.clone() });
                        ClientMsg::CreateWorktree { project_id, name, base }
                    }
                    Cmd::RemoveWorktree { project_id, path, force, delete_branch } => {
                        ClientMsg::RemoveWorktree { project_id, path, force, delete_branch }
                    }
                    Cmd::Kill(id) => ClientMsg::KillTerminal { id },
                    Cmd::MarkSeen(id) => ClientMsg::MarkSeen { id },
                    Cmd::Input(id, bytes) => {
                        if resumes.get(&id).is_some_and(|(_, _, protected)| !protected) {
                            resumes.remove(&id);
                        }
                        ClientMsg::Input { id, bytes }
                    }
                    Cmd::Resume(connection, id, bytes) if Arc::ptr_eq(&connection, &incarnation) => {
                        ClientMsg::Input { id, bytes }
                    }
                    Cmd::Resume(..) => continue,
                    Cmd::Resize(id, cols, rows) => ClientMsg::Resize { id, cols, rows },
                };
                markers.on_sent(&msg);
                send(&mut stream, &msg).await?;
            }
        }
    }
}

async fn wait_for_resume(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(when) => tokio::time::sleep_until(when).await,
        None => std::future::pending().await,
    }
}

impl Conn {
    fn restart_failed(&self, err: anyhow::Error) {
        tracing::warn!("restart daemon failed: {err:#}");
        let mut st = self.state.lock();
        st.restart_result = Some(RestartResult::Failed);
        st.last_error = Some(("restart daemon".into(), format!("{err:#}"), Instant::now()));
        drop(st);
        (self.wake)();
    }

    async fn restart(&self, request: crate::daemon_restart::Request) -> bool {
        let session = match request.save_and_shutdown(&self.socket).await {
            Ok(session) => session,
            Err(err) => {
                self.restart_failed(err);
                return false;
            }
        };
        if let Err(err) = crate::daemon_restart::wait_for_exit(&self.socket).await {
            self.restart_failed(err);
            return false;
        }
        {
            let mut st = self.state.lock();
            st.terminals.clear();
            st.status = ConnStatus::Reconnecting;
            st.daemon_pid = None;
            st.daemon_build = None;
            st.synced = false;
            st.first_list_count = None;
            st.restart_result = Some(RestartResult::Stopped(session));
        }
        (self.wake)();
        true
    }

    fn restore_terminal(&self, info: TerminalInfo, data: RestoreData, captured: Option<&[u8]>) -> Option<Vec<u8>> {
        let session = self.state.lock().terminals.iter()
            .find(|entry| entry.info.id == info.id && entry.info.created_unix_ms == info.created_unix_ms)
            .map(|entry| entry.session.clone())
            .unwrap_or_else(|| self.new_session(&info));
        let history = data.scrollback.and_then(|name| {
            let dir = sich_daemon::scrollback::directory()?;
            let bytes = match sich_daemon::scrollback::load(&dir, &name) {
                Ok(bytes) => bytes,
                Err(err) => {
                    tracing::warn!("could not read terminal scrollback: {err}");
                    return None;
                }
            };
            Some((dir, name, bytes))
        });
        let mut lines = Vec::new();
        let bytes = captured.or_else(|| history.as_ref().map(|(_, _, bytes)| bytes.as_slice()));
        if bytes.is_some() {
            lines.push(data.separator);
        }
        lines.extend(data.interruption);
        if !lines.is_empty() {
            session.replay_history(bytes.unwrap_or_default(), &lines);
        }
        if let Some((dir, name, _)) = history {
            if let Err(err) = sich_daemon::scrollback::remove(&dir, &name) {
                tracing::warn!("could not remove restored scrollback: {err}");
            }
        }
        let mut state = self.state.lock();
        state.terminals.retain(|entry| entry.info.id != info.id);
        state.terminals.push(TermEntry { info, session, restore_token: Some(data.token), legacy: false });
        drop(state);
        (self.wake)();
        data.resume.map(|cmd| format!("{cmd}\r").into_bytes())
    }

    fn new_session(&self, info: &TerminalInfo) -> TermSession {
        let id = info.id;
        let input_tx = self.cmd_tx.clone();
        let resize_tx = self.cmd_tx.clone();
        let wake = self.wake.clone();
        let session = TermSession::new(
            &self.rt,
            info.cols,
            info.rows,
            Arc::new(move |bytes| {
                let _ = input_tx.send(Cmd::Input(id, bytes));
            }),
            Arc::new(move |cols, rows| {
                let _ = resize_tx.send(Cmd::Resize(id, cols, rows));
            }),
            move || wake(),
        );
        session.set_exited(info.exited);
        session
    }

    /// Applies a server message to the shared state; returns messages to send back.
    fn on_server(&self, msg: ServerMsg) -> anyhow::Result<Vec<ClientMsg>> {
        let mut out = Vec::new();
        match msg {
            ServerMsg::Welcome { proto_version, daemon_pid } => {
                anyhow::ensure!(
                    proto_version == PROTO_VERSION,
                    "daemon speaks protocol {proto_version}, expected {PROTO_VERSION}"
                );
                {
                    let mut st = self.state.lock();
                    // A restarted daemon reuses ids; our sessions would be stale.
                    if st.daemon_pid.is_some_and(|p| p != daemon_pid) {
                        st.terminals.clear();
                    }
                    st.daemon_pid = Some(daemon_pid);
                    st.daemon_build = None;
                    st.status = ConnStatus::Connected { pid: daemon_pid };
                }
                out.push(ClientMsg::GetDaemonInfo);
                out.push(ClientMsg::ListTerminals);
                out.push(ClientMsg::ListProjects);
            }
            ServerMsg::DaemonInfo { build_id, proto_version, .. } => {
                if proto_version == PROTO_VERSION {
                    self.state.lock().daemon_build = Some(build_id);
                }
            }
            ServerMsg::ScrollbackSaved { .. } | ServerMsg::TerminalRestored { .. } => {}
            // Requested only on a dedicated activity connection.
            ServerMsg::SubagentActivity { .. } => return Ok(out),
            ServerMsg::Terminals(list) => self.reconcile(list, &mut out),
            ServerMsg::TerminalsWithSubagentModels { mut terminals, models } => {
                for (id, models) in models {
                    if let Some(status) = terminals.iter_mut().find(|t| t.id == id).and_then(|t| t.agent.as_mut()) {
                        attach_subagent_models(status, models);
                    }
                }
                self.reconcile(terminals, &mut out);
            }
            ServerMsg::TerminalCreated(info) => {
                let mut st = self.state.lock();
                if !st.terminals.iter().any(|t| t.info.id == info.id) {
                    out.push(ClientMsg::Subscribe { id: info.id, resume_from_seq: None });
                    let session = self.new_session(&info);
                    st.terminals.push(TermEntry { info, session, restore_token: None, legacy: false });
                }
            }
            ServerMsg::TerminalExited { id, code } => {
                let mut st = self.state.lock();
                if let Some(t) = st.terminals.iter_mut().find(|t| t.info.id == id) {
                    t.info.exited = Some(code);
                    t.session.set_exited(Some(code));
                }
            }
            ServerMsg::TerminalRemoved { id } => {
                self.state.lock().terminals.retain(|t| t.info.id != id);
            }
            ServerMsg::AgentStatusChanged { id, status } => self.set_agent_status(id, status),
            ServerMsg::AgentStatusWithSubagentModels { id, mut status, models } => {
                attach_subagent_models(&mut status, models);
                self.set_agent_status(id, Some(status));
            }
            ServerMsg::TerminalTitle { id, title } => {
                let mut st = self.state.lock();
                if let Some(t) = st.terminals.iter_mut().find(|t| t.info.id == id) {
                    t.info.title = title;
                }
            }
            ServerMsg::Output { id, seq, bytes } => {
                if let Some(s) = self.session_of(id) {
                    s.feed(seq, &bytes);
                }
                return Ok(out);
            }
            ServerMsg::Projects { projects, groups } => {
                let mut st = self.state.lock();
                // CreateWorktree replies arrive in request order on this connection. Only a
                // newly listed match can confirm success, including when other clients broadcast
                // Projects while we are waiting for the daemon's response.
                if let Some(request) = st.pending_worktrees.front()
                    && match_pending_worktree(request, &st.projects).is_none()
                    && let Some((root, path)) = match_pending_worktree(request, &projects)
                {
                    st.pending_worktrees.pop_front();
                    st.created_worktrees.push((root.to_path_buf(), path.to_path_buf()));
                }
                st.projects = projects;
                st.groups = groups;
                st.projects_rev += 1;
            }
            ServerMsg::OpFailed { op, message } => {
                tracing::warn!("daemon op `{op}` failed: {message}");
                let mut st = self.state.lock();
                if op == "create_worktree" {
                    st.pending_worktrees.pop_front();
                }
                st.last_error = Some((op, message, Instant::now()));
            }
            ServerMsg::Error { message } => tracing::warn!("daemon error: {message}"),
        }
        (self.wake)();
        Ok(out)
    }

    fn set_agent_status(&self, id: TermId, status: Option<AgentStatus>) {
        if let Some(t) = self.state.lock().terminals.iter_mut().find(|t| t.info.id == id) {
            t.info.agent = status;
        }
    }

    fn session_of(&self, id: TermId) -> Option<TermSession> {
        self.state.lock().terminals.iter().find(|t| t.info.id == id).map(|t| t.session.clone())
    }

    /// Syncs local terminals with the daemon list (add unknown, drop missing, resume known).
    fn reconcile(&self, list: Vec<TerminalInfo>, out: &mut Vec<ClientMsg>) {
        let mut st = self.state.lock();
        let mut old = std::mem::take(&mut st.terminals);
        let mut terminals = Vec::with_capacity(list.len());
        for info in list {
            match old.iter().position(|t| t.info.id == info.id && t.info.created_unix_ms == info.created_unix_ms) {
                Some(i) => {
                    let mut entry = old.swap_remove(i);
                    let (cols, rows) = entry.session.size();
                    if (cols, rows) != (info.cols, info.rows) {
                        out.push(ClientMsg::Resize { id: info.id, cols, rows });
                    }
                    entry.session.set_exited(info.exited);
                    // A marker lost with the old connection must not leave it quiet.
                    entry.session.set_quiet(false);
                    entry.info = info;
                    out.push(ClientMsg::Subscribe {
                        id: entry.info.id,
                        resume_from_seq: Some(entry.session.last_seq()),
                    });
                    terminals.push(entry);
                }
                None => {
                    // An existing terminal: the daemon replays its buffered history, which
                    // may hold old queries nobody is waiting for. Quiet until the marker.
                    let session = self.new_session(&info);
                    session.set_quiet(true);
                    out.push(ClientMsg::Subscribe { id: info.id, resume_from_seq: None });
                    out.push(ClientMsg::GetDaemonInfo);
                    terminals.push(TermEntry { info, session, restore_token: None, legacy: false });
                }
            }
        }
        // Terminals still in `old` no longer exist on the daemon and are dropped.
        if !st.synced {
            st.first_list_count = Some(terminals.len());
        }
        if terminals.is_empty() && !st.synced && st.auto_create {
            out.push(ClientMsg::CreateTerminal {
                cwd: None,
                shell: None,
                args: Vec::new(),
                cols: DEFAULT_COLS,
                rows: DEFAULT_ROWS,
                worktree: None,
            });
        }
        st.synced = true;
        st.terminals = terminals;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn welcome(v: u32) -> ServerMsg {
        ServerMsg::Welcome { proto_version: v, daemon_pid: 42 }
    }

    #[test]
    fn replay_marker_ends_the_replay_of_the_latest_fresh_subscribe() {
        let mut m = ReplayMarkers::default();
        m.on_sent(&ClientMsg::GetDaemonInfo); // from Welcome
        m.on_sent(&ClientMsg::Subscribe { id: 3, resume_from_seq: None });
        m.on_sent(&ClientMsg::GetDaemonInfo);
        m.on_sent(&ClientMsg::Subscribe { id: 4, resume_from_seq: Some(10) });
        m.on_sent(&ClientMsg::GetDaemonInfo); // unpaired: a resume is live output
        assert_eq!(m.on_daemon_info(), None);
        assert_eq!(m.on_daemon_info(), Some(3));
        assert_eq!(m.on_daemon_info(), None);
        assert_eq!(m.on_daemon_info(), None, "extra replies are harmless");
    }

    #[test]
    fn reconcile_keeps_known_terminals_live_and_quiets_new_ones_until_the_marker() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let conn = test_conn(&rt, false);
        let info = |id| TerminalInfo {
            id,
            title: String::new(),
            cwd: Default::default(),
            cols: 80,
            rows: 24,
            exited: None,
            created_unix_ms: 0,
            worktree: None,
            agent: None,
        };
        let mut out = Vec::new();
        conn.reconcile(vec![info(1)], &mut out);
        assert_eq!(
            out,
            vec![
                ClientMsg::Subscribe { id: 1, resume_from_seq: None },
                ClientMsg::GetDaemonInfo
            ]
        );
        let session = conn.session_of(1).unwrap();
        assert!(session.is_quiet());
        // Reconnect before the marker arrived: the resume must not stay quiet.
        conn.reconcile(vec![info(1)], &mut Vec::new());
        assert!(!session.is_quiet());
    }

    #[test]
    fn matching_welcome_is_compatible() {
        assert_eq!(classify_incompatible(&welcome(PROTO_VERSION), false), None);
    }

    #[test]
    fn welcome_with_other_version_is_incompatible() {
        assert_eq!(
            classify_incompatible(&welcome(PROTO_VERSION + 1), false),
            Some(ConnStatus::Incompatible { pid: Some(42), version: Some(PROTO_VERSION + 1) })
        );
    }

    #[test]
    fn error_before_welcome_is_an_old_daemon() {
        let err = ServerMsg::Error { message: "protocol version mismatch".into() };
        assert_eq!(
            classify_incompatible(&err, false),
            Some(ConnStatus::Incompatible { pid: None, version: None })
        );
    }

    #[test]
    fn error_after_welcome_is_not_incompatibility() {
        let err = ServerMsg::Error { message: "no such terminal: 3".into() };
        assert_eq!(classify_incompatible(&err, true), None);
    }

    #[test]
    fn other_messages_are_ignored() {
        assert_eq!(classify_incompatible(&ServerMsg::Terminals(Vec::new()), false), None);
    }

    #[test]
    fn extended_inventory_feeds_model_to_sidebar_and_plain_updates_clear_it() {
        use sich_proto::{AgentKind, AgentState, SubagentStatus};

        let rt = tokio::runtime::Runtime::new().unwrap();
        let conn = test_conn(&rt, false);
        let status = AgentStatus {
            kind: AgentKind::Omp, state: AgentState::Working, unread: false, since_unix_ms: 10,
            detail: None, model: Some("parent-model".into()), title: None, prompt: None, message: None,
            session_id: Some("parent".into()), subagents: vec![SubagentStatus {
                id: "child".into(), agent_type: None, description: None, state: AgentState::Working,
                detail: None, since_unix_ms: 20, model: None,
            }],
        };
        let info = TerminalInfo {
            id: 1, title: "shell".into(), cwd: "/test".into(), cols: 80, rows: 24, exited: None,
            created_unix_ms: 1, worktree: None, agent: Some(status.clone()),
        };
        conn.on_server(ServerMsg::TerminalsWithSubagentModels {
            terminals: vec![info],
            models: vec![(1, vec![
                SubagentModel { id: "unrelated".into(), model: "other-model".into() },
                SubagentModel { id: "child".into(), model: "child-model".into() },
            ])],
        }).unwrap();
        let child_model = || conn.state.lock().terminals[0].info.agent.as_ref().unwrap().subagents[0].model.clone();
        assert_eq!(child_model().as_deref(), Some("child-model"));
        assert_eq!(conn.state.lock().first_list_count, Some(1));
        conn.on_server(ServerMsg::AgentStatusChanged { id: 1, status: Some(status) }).unwrap();
        assert_eq!(child_model(), None);
    }

    fn test_conn(rt: &tokio::runtime::Runtime, auto_create: bool) -> Conn {
        test_conn_with_rx(rt.handle(), auto_create).0
    }

    fn test_conn_with_rx(handle: &Handle, auto_create: bool) -> (Conn, mpsc::UnboundedReceiver<Cmd>) {
        let (cmd_tx, rx) = mpsc::unbounded_channel();
        (Conn {
            socket: "unused".into(),
            client_name: "test".into(),
            rt: handle.clone(),
            state: Arc::new(Mutex::new(ClientState {
                status: ConnStatus::Reconnecting,
                terminals: Vec::new(),
                synced: false,
                first_list_count: None,
                auto_create,
                daemon_pid: None,
                daemon_build: None,
                restart_result: None,
                projects: Vec::new(),
                groups: Vec::new(),
                projects_rev: 0,
                pending_worktrees: Default::default(),
                created_worktrees: Vec::new(),
                last_error: None,
                failed_splits: Vec::new(),
            })),
            cmd_tx,
            wake: Arc::new(|| {}),
        }, rx)
    }

    #[test]
    fn worktree_confirmations_survive_coalesced_projects_updates() {
        use sich_proto::{ProjectInfo, WorktreeInfo};

        let rt = tokio::runtime::Runtime::new().unwrap();
        let conn = test_conn(&rt, false);
        let report = |names: &[&str]| ServerMsg::Projects {
            projects: vec![ProjectInfo {
                id: 7,
                name: "repo".into(),
                path: "/repo".into(),
                is_git: true,
                worktrees: names.iter().map(|name| WorktreeInfo {
                    path: format!("/worktrees/{name}").into(),
                    branch: Some((*name).into()),
                    head: String::new(),
                    is_main: false,
                    dirty: false,
                }).collect(),
                error: None,
                group: None,
            }],
            groups: Vec::new(),
        };

        conn.on_server(report(&["external"])).unwrap();
        conn.state.lock().pending_worktrees.extend(
            ["first", "external", "second"].map(|name| PendingWorktree {
                project_id: 7, name: name.into(),
            }),
        );
        conn.on_server(report(&["external", "first"])).unwrap();
        conn.on_server(report(&["external", "first", "unrelated"])).unwrap();
        conn.on_server(ServerMsg::OpFailed {
            op: "create_worktree".into(), message: "already exists".into(),
        }).unwrap();
        conn.on_server(report(&["external", "first", "unrelated", "second"])).unwrap();

        let state = conn.state.lock();
        assert!(state.pending_worktrees.is_empty());
        assert_eq!(state.created_worktrees, [
            ("/repo".into(), "/worktrees/first".into()),
            ("/repo".into(), "/worktrees/second".into()),
        ]);
    }

    #[test]
    fn empty_first_list_auto_creates_a_terminal_only_when_enabled() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        for (auto, expect) in [(true, true), (false, false)] {
            let conn = test_conn(&rt, auto);
            let mut out = Vec::new();
            conn.reconcile(Vec::new(), &mut out);
            let created = out.iter().any(|m| matches!(m, ClientMsg::CreateTerminal { .. }));
            assert_eq!(created, expect, "auto_create={auto}");
            assert_eq!(conn.state.lock().first_list_count, Some(0));
            // Only the very first list counts.
            let mut out = Vec::new();
            conn.reconcile(Vec::new(), &mut out);
            assert!(out.is_empty());
            assert_eq!(conn.state.lock().first_list_count, Some(0));
        }
    }

    async fn recv_client(peer: &mut Stream<tokio::io::DuplexStream>) -> ClientMsg {
        let frame = tokio::time::timeout(Duration::from_secs(3), peer.next()).await.unwrap().unwrap().unwrap();
        decode(&frame).unwrap()
    }

    async fn send_server(peer: &mut Stream<tokio::io::DuplexStream>, msg: ServerMsg) {
        peer.send(encode(&msg).unwrap()).await.unwrap();
    }

    async fn exercise_resume(first_output: bool, protected: bool) {
        let (conn, mut commands) = test_conn_with_rx(&Handle::current(), false);
        let cmd_tx = conn.cmd_tx.clone();
        let (local, peer) = tokio::io::duplex(8192);
        let mut peer = Framed::new(peer, frame_codec());
        let task = tokio::spawn(async move {
            session_on(&conn, Framed::new(local, frame_codec()), &mut commands, &mut false, false).await
        });

        assert!(matches!(recv_client(&mut peer).await, ClientMsg::Hello { .. }));
        send_server(&mut peer, welcome(PROTO_VERSION)).await;
        for expected in [ClientMsg::GetDaemonInfo, ClientMsg::ListTerminals, ClientMsg::ListProjects] {
            assert_eq!(recv_client(&mut peer).await, expected);
        }
        cmd_tx.send(Cmd::Restore {
            worktree: None, cwd: None, cols: DEFAULT_COLS, rows: DEFAULT_ROWS,
            data: RestoreData { token: 42, scrollback: None, separator: "restored", interruption: None, resume: Some("codex resume abcdef12".into()), protect_resume: protected, split: false, migration: None },
        }).unwrap();
        let ClientMsg::RestoreTerminal { request_id, .. } = recv_client(&mut peer).await else { panic!("expected restore request") };
        let terminal = TerminalInfo {
            id: 15, title: String::new(), cwd: Default::default(), cols: 80, rows: 24,
            exited: None, created_unix_ms: 1, worktree: None, agent: None,
        };
        send_server(&mut peer, ServerMsg::TerminalRestored {
            request_id: request_id + 1, terminal: Some(TerminalInfo { id: 16, ..terminal.clone() }),
        }).await;
        send_server(&mut peer, ServerMsg::TerminalRestored { request_id, terminal: Some(terminal) }).await;
        assert_eq!(recv_client(&mut peer).await, ClientMsg::Subscribe { id: 15, resume_from_seq: None });
        if protected {
            cmd_tx.send(Cmd::Input(15, b"early".to_vec())).unwrap();
            assert_eq!(recv_client(&mut peer).await, ClientMsg::Input { id: 15, bytes: b"early".to_vec() });
        }
        if first_output {
            send_server(&mut peer, ServerMsg::Output { id: 15, seq: 0, bytes: b"\x1b[6n".to_vec() }).await;
            assert_eq!(recv_client(&mut peer).await, ClientMsg::Input { id: 15, bytes: b"\x1b[1;1R".to_vec() });
        }
        assert_eq!(recv_client(&mut peer).await, ClientMsg::Input { id: 15, bytes: b"codex resume abcdef12\r".to_vec() });
        task.abort();
    }

    #[tokio::test]
    async fn fresh_split_requests_are_correlated_and_fail_without_replay() {
        let (conn, mut commands) = test_conn_with_rx(&Handle::current(), false);
        let state = conn.state.clone();
        let tx = conn.cmd_tx.clone();
        let (local, peer) = tokio::io::duplex(8192);
        let mut peer = Framed::new(peer, frame_codec());
        let task = tokio::spawn(async move {
            session_on(&conn, Framed::new(local, frame_codec()), &mut commands, &mut false, false).await
        });
        assert!(matches!(recv_client(&mut peer).await, ClientMsg::Hello { .. }));
        send_server(&mut peer, welcome(PROTO_VERSION)).await;
        for _ in 0..3 { recv_client(&mut peer).await; }
        for token in [101, 202] {
            tx.send(Cmd::Restore {
                worktree: Some("/worktree".into()), cwd: Some("/worktree/sub".into()), cols: 91, rows: 27,
                data: RestoreData::fresh_split(token),
            }).unwrap();
        }
        let first = recv_client(&mut peer).await;
        let ClientMsg::RestoreTerminal { request_id: first_id, ref worktree, ref cwd, cols, rows } = first else { panic!("restore") };
        assert_eq!((worktree.as_ref().unwrap(), cwd.as_ref().unwrap(), cols, rows),
            (&std::path::PathBuf::from("/worktree"), &std::path::PathBuf::from("/worktree/sub"), 91, 27));
        let ClientMsg::RestoreTerminal { request_id: second_id, .. } = recv_client(&mut peer).await else { panic!("restore") };
        let info = |id| TerminalInfo {
            id, title: String::new(), cwd: "/worktree/sub".into(), cols: 91, rows: 27,
            exited: None, created_unix_ms: id, worktree: Some("/worktree".into()), agent: None,
        };
        send_server(&mut peer, ServerMsg::TerminalCreated(info(9))).await;
        assert!(matches!(recv_client(&mut peer).await, ClientMsg::Subscribe { id: 9, .. }));
        send_server(&mut peer, ServerMsg::TerminalRestored { request_id: first_id, terminal: None }).await;
        send_server(&mut peer, ServerMsg::TerminalRestored { request_id: second_id, terminal: Some(info(2)) }).await;
        assert!(matches!(recv_client(&mut peer).await, ClientMsg::Subscribe { id: 2, .. }));
        {
            let state = state.lock();
            assert_eq!(state.failed_splits, vec![101]);
            assert_eq!(state.terminals.iter().find(|t| t.info.id == 9).unwrap().restore_token, None);
            assert_eq!(state.terminals.iter().find(|t| t.info.id == 2).unwrap().restore_token, Some(202));
        }
        assert!(tokio::time::timeout(Duration::from_millis(100), recv_client(&mut peer)).await.is_err());
        task.abort();
    }

    #[tokio::test]
    async fn restore_tokens_follow_only_matching_responses_and_survive_inventory() {
        let (conn, mut commands) = test_conn_with_rx(&Handle::current(), false);
        let state = conn.state.clone();
        let tx = conn.cmd_tx.clone();
        let (local, peer) = tokio::io::duplex(8192);
        let mut peer = Framed::new(peer, frame_codec());
        let task = tokio::spawn(async move {
            session_on(&conn, Framed::new(local, frame_codec()), &mut commands, &mut false, false).await
        });
        assert!(matches!(recv_client(&mut peer).await, ClientMsg::Hello { .. }));
        send_server(&mut peer, welcome(PROTO_VERSION)).await;
        for _ in 0..3 { recv_client(&mut peer).await; }
        for token in [101, 202] {
            tx.send(Cmd::Restore { worktree: None, cwd: None, cols: 80, rows: 24, data: RestoreData {
                token, scrollback: None, separator: "", interruption: None, resume: None, protect_resume: false, split: false,
                migration: None,
            }}).unwrap();
        }
        let ClientMsg::RestoreTerminal { request_id: first, .. } = recv_client(&mut peer).await else { panic!("restore") };
        let ClientMsg::RestoreTerminal { request_id: second, .. } = recv_client(&mut peer).await else { panic!("restore") };
        let info = |id, created_unix_ms| TerminalInfo {
            id, title: String::new(), cwd: Default::default(), cols: 80, rows: 24,
            exited: None, created_unix_ms, worktree: None, agent: None,
        };
        send_server(&mut peer, ServerMsg::TerminalCreated(info(9, 9))).await;
        assert!(matches!(recv_client(&mut peer).await, ClientMsg::Subscribe { id: 9, .. }));
        send_server(&mut peer, ServerMsg::TerminalRestored { request_id: second + 1, terminal: Some(info(99, 99)) }).await;
        send_server(&mut peer, ServerMsg::TerminalRestored { request_id: second, terminal: Some(info(2, 2)) }).await;
        assert!(matches!(recv_client(&mut peer).await, ClientMsg::Subscribe { id: 2, .. }));
        send_server(&mut peer, ServerMsg::TerminalRestored { request_id: first, terminal: Some(info(1, 1)) }).await;
        assert!(matches!(recv_client(&mut peer).await, ClientMsg::Subscribe { id: 1, .. }));
        {
            let st = state.lock();
            let token = |id| st.terminals.iter().find(|t| t.info.id == id).and_then(|t| t.restore_token);
            assert_eq!((token(1), token(2), token(9), token(99)), (Some(101), Some(202), None, None));
        }
        send_server(&mut peer, ServerMsg::Terminals(vec![info(1, 1), info(2, 22), info(9, 9)])).await;
        // The new incarnation of id 2 must not inherit its predecessor's token.
        for _ in 0..4 { recv_client(&mut peer).await; }
        {
            let st = state.lock();
            let token = |id| st.terminals.iter().find(|t| t.info.id == id).and_then(|t| t.restore_token);
            assert_eq!((token(1), token(2), token(9)), (Some(101), None, None));
        }
        task.abort();
    }

    #[tokio::test]
    async fn agent_resume_waits_for_initial_terminal_reply() {
        exercise_resume(true, false).await;
    }

    #[tokio::test]
    async fn agent_resume_falls_back_to_timer_when_the_shell_is_quiet() {
        exercise_resume(false, false).await;
    }

    #[tokio::test]
    async fn quick_command_survives_early_user_input() {
        exercise_resume(true, true).await;
    }

    #[test]
    fn first_list_count_records_existing_terminals() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let conn = test_conn(&rt, true);
        let info = TerminalInfo {
            id: 1,
            title: String::new(),
            cwd: std::path::PathBuf::new(),
            cols: 80,
            rows: 24,
            exited: None,
            created_unix_ms: 1,
            worktree: None,
            agent: None,
        };
        let mut out = Vec::new();
        conn.reconcile(vec![info], &mut out);
        assert_eq!(conn.state.lock().first_list_count, Some(1));
        assert!(!out.iter().any(|m| matches!(m, ClientMsg::CreateTerminal { .. })));
    }
}

#[cfg(test)]
mod retire_tests {
    use super::should_retire;

    #[test]
    fn only_a_listed_empty_legacy_daemon_is_retired() {
        assert!(should_retire(true, true, 0));
        assert!(!should_retire(true, true, 1), "never stop a daemon that still has terminals");
        assert!(!should_retire(true, false, 0), "wait for the first terminal list");
        assert!(!should_retire(false, true, 0), "the primary daemon stays up");
    }
}
