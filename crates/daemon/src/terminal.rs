//! A single PTY-backed terminal: reader thread, ring buffer, subscribers.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use parking_lot::Mutex;
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use sich_proto::{AgentKind, MAX_OUTPUT_CHUNK, ServerMsg, TermId, TerminalInfo};

use crate::agent::{self, AgentTracker};
use crate::codex_rollout;
use crate::hub::{ConnHandle, ConnId, Hub};
use crate::osc::{OscEvent, OscScanner};
use crate::ring::{DEFAULT_CAPACITY, RingBuffer};
use crate::shell::{default_shell, shell_title};

pub struct SpawnRequest {
    pub cwd: Option<PathBuf>,
    pub shell: Option<String>,
    pub args: Vec<String>,
    pub cols: u16,
    pub rows: u16,
    pub worktree: Option<PathBuf>,
}

struct State {
    ring: RingBuffer,
    subs: Vec<ConnHandle>,
    cols: u16,
    rows: u16,
    exited: Option<Option<i32>>,
    scanner: OscScanner,
    /// Live OSC title; `None` until the shell/program sets one.
    live_title: Option<String>,
    agent: AgentTracker,
}

/// State shared with the reader / waiter threads (deliberately not owning the PTY master, so
/// dropping the `Terminal` closes the PTY and ends the reader).
struct Shared {
    id: TermId,
    title: String,
    cwd: PathBuf,
    worktree: Option<PathBuf>,
    created_unix_ms: u64,
    hub: Arc<Hub>,
    state: Mutex<State>,
}

pub struct Terminal {
    shared: Arc<Shared>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    input_tx: std_mpsc::Sender<Vec<u8>>,
}

impl Terminal {
    pub fn spawn(
        id: TermId,
        req: SpawnRequest,
        hub: Arc<Hub>,
        socket_name: &str,
    ) -> anyhow::Result<Arc<Terminal>> {
        let (cols, rows) = (req.cols.max(1), req.rows.max(1));
        let shell = req.shell.filter(|s| !s.is_empty()).unwrap_or_else(default_shell);
        let cwd = req
            .cwd
            .filter(|p| p.is_dir())
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("."));

        let pair = native_pty_system()
            .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .context("open pty")?;
        let mut cmd = CommandBuilder::new(&shell);
        cmd.args(&req.args);
        cmd.cwd(&cwd);
        cmd.env_clear();
        for (k, v) in crate::env::terminal_env(id, socket_name) {
            cmd.env(k, v);
        }
        let mut child =
            pair.slave.spawn_command(cmd).with_context(|| format!("spawn `{shell}`"))?;
        drop(pair.slave);

        let killer = child.clone_killer();
        let mut reader = pair.master.try_clone_reader().context("clone pty reader")?;
        let mut writer = pair.master.take_writer().context("take pty writer")?;

        let shared = Arc::new(Shared {
            id,
            title: shell_title(&shell),
            cwd,
            worktree: req.worktree,
            created_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as u64),
            hub,
            state: Mutex::new(State {
                ring: RingBuffer::new(DEFAULT_CAPACITY),
                subs: Vec::new(),
                cols,
                rows,
                exited: None,
                scanner: OscScanner::new(),
                live_title: None,
                agent: AgentTracker::default(),
            }),
        });

        // PTY reader.
        let sh = shared.clone();
        std::thread::Builder::new()
            .name(format!("pty-read-{id}"))
            .spawn(move || {
                let mut buf = vec![0u8; MAX_OUTPUT_CHUNK];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => sh.on_output(&buf[..n]),
                    }
                }
            })
            .context("spawn reader thread")?;

        // PTY writer: a thread so the async side never blocks on a full pipe.
        let (input_tx, input_rx) = std_mpsc::channel::<Vec<u8>>();
        std::thread::Builder::new()
            .name(format!("pty-write-{id}"))
            .spawn(move || {
                while let Ok(bytes) = input_rx.recv() {
                    if writer.write_all(&bytes).and_then(|()| writer.flush()).is_err() {
                        break;
                    }
                }
            })
            .context("spawn writer thread")?;

        // Exit watcher.
        let sh = shared.clone();
        std::thread::Builder::new()
            .name(format!("pty-wait-{id}"))
            .spawn(move || {
                let code = child.wait().ok().map(|s| s.exit_code() as i32);
                // Give the reader a moment to drain the last output.
                std::thread::sleep(Duration::from_millis(150));
                let agent_cleared = {
                    let mut st = sh.state.lock();
                    st.exited = Some(code);
                    st.agent.on_exit()
                };
                sh.hub.broadcast(ServerMsg::TerminalExited { id: sh.id, code });
                if agent_cleared {
                    sh.hub.broadcast(ServerMsg::AgentStatusChanged { id: sh.id, status: None });
                }
            })
            .context("spawn wait thread")?;

        Ok(Arc::new(Terminal {
            shared,
            master: Mutex::new(pair.master),
            killer: Mutex::new(killer),
            input_tx,
        }))
    }

    pub fn id(&self) -> TermId {
        self.shared.id
    }

    pub fn info(&self) -> TerminalInfo {
        let st = self.shared.state.lock();
        TerminalInfo {
            id: self.shared.id,
            title: self.shared.effective_title(&st),
            cwd: self.shared.cwd.clone(),
            cols: st.cols,
            rows: st.rows,
            exited: st.exited,
            created_unix_ms: self.shared.created_unix_ms,
            worktree: self.shared.worktree.clone(),
            agent: st.agent.status().cloned(),
        }
    }

    /// Applies a hook payload sent by `sich hook` and broadcasts the status if it changed.
    pub fn agent_hook(&self, kind: AgentKind, payload: &str) {
        let now = now_ms();
        match kind {
            AgentKind::Claude => {
                let Some(hook) = agent::parse_claude_payload(payload) else {
                    tracing::debug!(id = self.shared.id, "ignoring unparsable hook payload");
                    return;
                };
                self.shared.update_agent(|a| a.on_claude_hook(&hook, now));
            }
            AgentKind::Codex => {
                let Some(hook) = agent::parse_codex_payload(payload) else {
                    tracing::debug!(id = self.shared.id, "ignoring unparsable codex hook payload");
                    return;
                };
                // A child hook may need its rollout's first line (identity). Decide under the lock,
                // read the small file outside of it, then apply both together.
                let plan = self.shared.state.lock().agent.identity_plan(&hook, now);
                let ident = plan.as_ref().and_then(codex_rollout::execute);
                self.shared.update_agent(|a| a.on_codex_hook_with(&hook, now, ident));
            }
            // Other agents have no hook integration yet.
            _ => {}
        }
    }

    /// The user looked at the terminal.
    pub fn mark_seen(&self) {
        let now = now_ms();
        self.shared.update_agent(|a| a.mark_seen(now));
    }

    /// Drops the agent status (terminal removed).
    pub fn clear_agent(&self) {
        self.shared.update_agent(AgentTracker::on_exit);
    }

    /// Atomically replays the buffered output (from `resume_from_seq`, clamped to the retained
    /// start) and registers the connection for live output.
    pub fn subscribe(&self, conn: &ConnHandle, resume_from_seq: Option<u64>) {
        let mut st = self.shared.state.lock();
        st.subs.retain(|s| s.id != conn.id);
        for (seq, bytes) in st.ring.chunks_from(resume_from_seq, MAX_OUTPUT_CHUNK) {
            if !conn.send(ServerMsg::Output { id: self.shared.id, seq, bytes }) {
                return;
            }
        }
        st.subs.push(conn.clone());
    }

    pub fn unsubscribe(&self, conn_id: ConnId) {
        self.shared.state.lock().subs.retain(|s| s.id != conn_id);
    }

    pub fn input(&self, bytes: Vec<u8>) {
        let _ = self.input_tx.send(bytes);
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        let (cols, rows) = (cols.max(1), rows.max(1));
        let res = self.master.lock().resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
        match res {
            Ok(()) => {
                let mut st = self.shared.state.lock();
                st.cols = cols;
                st.rows = rows;
            }
            Err(e) => tracing::warn!(id = self.shared.id, "pty resize failed: {e:#}"),
        }
    }

    /// Kills the process if it is still running.
    pub fn kill(&self) {
        if self.shared.state.lock().exited.is_none() {
            if let Err(e) = self.killer.lock().kill() {
                tracing::debug!(id = self.shared.id, "kill failed: {e}");
            }
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

impl Shared {
    fn effective_title(&self, st: &State) -> String {
        match &st.live_title {
            Some(t) if !t.trim().is_empty() => t.clone(),
            _ => self.title.clone(),
        }
    }

    /// Runs `f` on the agent tracker; broadcasts the new status when `f` reports a change.
    fn update_agent(&self, f: impl FnOnce(&mut AgentTracker) -> bool) {
        let status = {
            let mut st = self.state.lock();
            if !f(&mut st.agent) {
                return;
            }
            st.agent.status().cloned()
        };
        self.hub.broadcast(ServerMsg::AgentStatusChanged { id: self.id, status });
    }

    /// Scans a PTY chunk for title changes and bells.
    fn scan(&self, bytes: &[u8]) {
        let mut titles: Vec<String> = Vec::new();
        {
            let mut st = self.state.lock();
            st.scanner.feed(bytes, |e| match e {
                OscEvent::Title(t) => titles.push(t),
                OscEvent::Bell => {}
            });
        }
        for t in titles {
            self.set_title(t);
        }
    }

    fn set_title(&self, title: String) {
        let now = now_ms();
        let (effective, agent_status, agent_changed) = {
            let mut st = self.state.lock();
            if st.live_title.as_deref() == Some(title.as_str()) {
                return;
            }
            let before = self.effective_title(&st);
            st.live_title = Some(title.clone());
            let agent_changed = st.agent.on_title(&title, now);
            let after = self.effective_title(&st);
            let changed = (after != before).then_some(after);
            (changed, st.agent.status().cloned(), agent_changed)
        };
        if let Some(title) = effective {
            self.hub.broadcast(ServerMsg::TerminalTitle { id: self.id, title });
        }
        if agent_changed {
            self.hub.broadcast(ServerMsg::AgentStatusChanged { id: self.id, status: agent_status });
        }
    }

    fn on_output(&self, bytes: &[u8]) {
        self.scan(bytes);
        let mut st = self.state.lock();
        let seq = st.ring.push(bytes);
        let id = self.id;
        // Fan out; a subscriber that cannot keep up gets disconnected (it resumes on reconnect).
        let mut offset = 0u64;
        for piece in bytes.chunks(MAX_OUTPUT_CHUNK) {
            let msg = ServerMsg::Output { id, seq: seq + offset, bytes: piece.to_vec() };
            st.subs.retain(|s| s.send(msg.clone()));
            offset += piece.len() as u64;
        }
    }
}
