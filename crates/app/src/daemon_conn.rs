//! Connection task: owns the daemon connection, routes server messages to sessions and
//! reconnects forever with backoff.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use futures::{SinkExt as _, StreamExt as _};
use parking_lot::Mutex;
use sich_proto::{
    ClientMsg, PROTO_VERSION, ServerMsg, TermId, TerminalInfo, decode, encode, frame_codec,
};
use sich_term::TermSession;
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use crate::daemon_client::{
    ClientState, Cmd, ConnStatus, DEFAULT_COLS, DEFAULT_ROWS, TermEntry, Wake,
};

const BACKOFF_MIN: Duration = Duration::from_millis(250);
const BACKOFF_MAX: Duration = Duration::from_secs(2);

pub(crate) struct Conn {
    pub socket: String,
    pub rt: Handle,
    pub state: Arc<Mutex<ClientState>>,
    pub cmd_tx: mpsc::UnboundedSender<Cmd>,
    pub wake: Wake,
}

type Stream = Framed<sich_daemon::ClientStream, LengthDelimitedCodec>;

pub(crate) async fn run(conn: Conn, mut cmd_rx: mpsc::UnboundedReceiver<Cmd>) {
    let mut backoff = BACKOFF_MIN;
    loop {
        match sich_daemon::connect(&conn.socket).await {
            Ok(stream) => {
                let mut welcomed = false;
                let framed = Framed::new(stream, frame_codec());
                if let Err(e) = session(&conn, framed, &mut cmd_rx, &mut welcomed).await {
                    tracing::warn!("daemon connection lost: {e:#}");
                }
                if welcomed {
                    backoff = BACKOFF_MIN;
                }
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
                cmd = cmd_rx.recv() => if cmd.is_none() { return },
            }
        }
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

async fn send(stream: &mut Stream, msg: &ClientMsg) -> anyhow::Result<()> {
    stream.send(encode(msg)?).await.context("send to daemon")
}

async fn session(
    conn: &Conn,
    mut stream: Stream,
    cmd_rx: &mut mpsc::UnboundedReceiver<Cmd>,
    welcomed: &mut bool,
) -> anyhow::Result<()> {
    send(
        &mut stream,
        &ClientMsg::Hello { proto_version: PROTO_VERSION, client_name: "sich-app".into() },
    )
    .await?;

    loop {
        tokio::select! {
            frame = stream.next() => {
                let frame = frame.context("daemon closed the connection")?.context("read frame")?;
                let msg: ServerMsg = decode(&frame).context("decode server message")?;
                if matches!(msg, ServerMsg::Welcome { .. }) {
                    *welcomed = true;
                }
                for out in conn.on_server(msg)? {
                    send(&mut stream, &out).await?;
                }
            }
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { return Ok(()) };
                let msg = match cmd {
                    Cmd::Create { cols, rows, worktree } => ClientMsg::CreateTerminal {
                        cwd: None,
                        shell: None,
                        args: Vec::new(),
                        cols,
                        rows,
                        worktree,
                    },
                    Cmd::RefreshProjects => ClientMsg::RefreshProjects,
                    Cmd::AddProject(path) => ClientMsg::AddProject { path },
                    Cmd::RemoveProject(id) => ClientMsg::RemoveProject { id },
                    Cmd::CreateWorktree { project_id, name, base } => {
                        ClientMsg::CreateWorktree { project_id, name, base }
                    }
                    Cmd::RemoveWorktree { project_id, path, force, delete_branch } => {
                        ClientMsg::RemoveWorktree { project_id, path, force, delete_branch }
                    }
                    Cmd::Kill(id) => ClientMsg::KillTerminal { id },
                    Cmd::Input(id, bytes) => ClientMsg::Input { id, bytes },
                    Cmd::Resize(id, cols, rows) => ClientMsg::Resize { id, cols, rows },
                };
                send(&mut stream, &msg).await?;
            }
        }
    }
}

impl Conn {
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
                    st.status = ConnStatus::Connected { pid: daemon_pid };
                }
                out.push(ClientMsg::ListTerminals);
                out.push(ClientMsg::ListProjects);
            }
            ServerMsg::Terminals(list) => self.reconcile(list, &mut out),
            ServerMsg::TerminalCreated(info) => {
                let mut st = self.state.lock();
                if !st.terminals.iter().any(|t| t.info.id == info.id) {
                    out.push(ClientMsg::Subscribe { id: info.id, resume_from_seq: None });
                    let session = self.new_session(&info);
                    st.terminals.push(TermEntry { info, session });
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
            ServerMsg::Output { id, seq, bytes } => {
                if let Some(s) = self.session_of(id) {
                    s.feed(seq, &bytes);
                }
                return Ok(out);
            }
            ServerMsg::Projects(list) => self.state.lock().projects = list,
            ServerMsg::OpFailed { op, message } => {
                tracing::warn!("daemon op `{op}` failed: {message}");
                self.state.lock().last_error = Some((op, message, Instant::now()));
            }
            ServerMsg::Error { message } => tracing::warn!("daemon error: {message}"),
        }
        (self.wake)();
        Ok(out)
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
            match old.iter().position(|t| t.info.id == info.id) {
                Some(i) => {
                    let mut entry = old.swap_remove(i);
                    let (cols, rows) = entry.session.size();
                    if (cols, rows) != (info.cols, info.rows) {
                        out.push(ClientMsg::Resize { id: info.id, cols, rows });
                    }
                    entry.session.set_exited(info.exited);
                    entry.info = info;
                    out.push(ClientMsg::Subscribe {
                        id: entry.info.id,
                        resume_from_seq: Some(entry.session.last_seq()),
                    });
                    terminals.push(entry);
                }
                None => {
                    out.push(ClientMsg::Subscribe { id: info.id, resume_from_seq: None });
                    let session = self.new_session(&info);
                    terminals.push(TermEntry { info, session });
                }
            }
        }
        // Terminals still in `old` no longer exist on the daemon and are dropped.
        if terminals.is_empty() && !st.synced {
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
