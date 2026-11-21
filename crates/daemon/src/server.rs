//! Per-connection protocol handling.

use std::sync::Arc;

use anyhow::Context as _;
use futures::{SinkExt as _, StreamExt as _};
use sich_proto::{
    AgentKind, AgentStatus, ClientMsg, OMP_CLIENT_SUFFIX, PROTO_VERSION, SUBAGENT_ACTIVITY_CLIENT,
    SUBAGENT_MODELS_CLIENT_SUFFIX, ServerMsg, SubagentModel, decode, encode, frame_codec,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use tokio_util::codec::Framed;
use tokio_util::sync::CancellationToken;

use crate::hub::ConnHandle;
use crate::manager::Manager;
use crate::projects;
use crate::terminal::SpawnRequest;

const WRITE_QUEUE: usize = 1024;

pub async fn handle_connection<S>(stream: S, mgr: Arc<Manager>)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let conn_id = mgr.next_conn_id();
    if let Err(e) = serve(stream, &mgr, conn_id).await {
        tracing::debug!(conn = conn_id, "connection ended: {e:#}");
    }
    mgr.drop_connection(conn_id);
}

async fn serve<S>(stream: S, mgr: &Arc<Manager>, conn_id: u64) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let framed = Framed::new(stream, frame_codec());
    let (mut sink, mut source) = framed.split();

    // Handshake: the first message must be Hello with a matching version.
    // `Shutdown` is accepted before Hello so any client can stop any daemon version.
    let first = source.next().await.context("closed before Hello")?.context("read Hello")?;
    let client_name = match decode::<ClientMsg>(&first).context("decode Hello")? {
        ClientMsg::Hello { proto_version, client_name } if proto_version == PROTO_VERSION => {
            tracing::info!(conn = conn_id, %client_name, "client connected");
            client_name
        }
        ClientMsg::Hello { proto_version, .. } => {
            // Welcome first, so the client can learn our version and pid.
            let welcome =
                ServerMsg::Welcome { proto_version: PROTO_VERSION, daemon_pid: std::process::id() };
            sink.send(encode(&welcome)?).await.ok();
            let message = format!(
                "protocol version mismatch: client {proto_version}, daemon {PROTO_VERSION}"
            );
            sink.send(encode(&ServerMsg::Error { message: message.clone() })?).await.ok();
            anyhow::bail!(message);
        }
        ClientMsg::Shutdown => {
            tracing::info!(conn = conn_id, "shutdown requested before Hello");
            mgr.shutdown.cancel();
            return Ok(());
        }
        _ => {
            let message = "expected Hello as the first message".to_string();
            sink.send(encode(&ServerMsg::Error { message: message.clone() })?).await.ok();
            anyhow::bail!(message);
        }
    };

    if client_name == SUBAGENT_ACTIVITY_CLIENT {
        sink.send(encode(&ServerMsg::Welcome { proto_version: PROTO_VERSION, daemon_pid: std::process::id() })?).await?;
        loop {
            let Some(frame) = (tokio::select! {
                _ = mgr.shutdown.cancelled() => return Ok(()),
                frame = source.next() => frame,
            }) else { return Ok(()) };
            let msg = match decode(&frame.context("read activity request")?).context("decode activity request")? {
                ClientMsg::GetSubagentActivity { id, created_unix_ms, key, after_seq } => ServerMsg::SubagentActivity {
                    activity: mgr.get(id).and_then(|t| t.subagent_activity(created_unix_ms, &key, after_seq)),
                },
                _ => ServerMsg::Error { message: "expected an activity request".into() },
            };
            sink.send(encode(&msg)?).await?;
        }
    }

    let (tx, mut rx) = mpsc::channel::<ServerMsg>(WRITE_QUEUE);
    let cancel = CancellationToken::new();
    let mut conn = ConnHandle::new(conn_id, tx, cancel.clone());
    conn.worktree_owner = crate::worker_worktree::client_owner(&client_name).map(str::to_owned);
    conn.worker_worktrees = client_name == crate::worker_worktree::CLIENT_NAME || conn.worktree_owner.is_some();

    // Welcome goes first in the queue, then the connection starts receiving broadcasts.
    conn.send(ServerMsg::Welcome { proto_version: PROTO_VERSION, daemon_pid: std::process::id() });
    mgr.hub.register(conn.clone());

    let (client_name, omp_status) = match client_name.strip_suffix(OMP_CLIENT_SUFFIX) {
        Some(name) => (name, true),
        None => (client_name.as_str(), false),
    };
    let (client_name, subagent_models) = match client_name.strip_suffix(SUBAGENT_MODELS_CLIENT_SUFFIX) {
        Some(name) => (name, true),
        None => (client_name, false),
    };
    let writer_cancel = cancel.clone();
    let writer = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = writer_cancel.cancelled() => break,
                msg = rx.recv() => {
                    let Some(mut msg) = msg else { break };
                    if !omp_status {
                        downgrade_omp(&mut msg);
                    }
                    if subagent_models {
                        msg = with_subagent_models(msg);
                    }
                    let Ok(frame) = encode(&msg) else { continue };
                    if sink.send(frame).await.is_err() {
                        break;
                    }
                }
            }
        }
        writer_cancel.cancel();
    });

    let retire = client_name == crate::generation::RETIRE_CLIENT;
    let result = read_loop(&mut source, mgr, &conn, &cancel, retire).await;
    cancel.cancel();
    let _ = writer.await;
    result
}

/// Keep protocol v7 readable for clients compiled before the new enum variant.
fn downgrade_omp(msg: &mut ServerMsg) {
    fn downgrade(status: Option<&mut AgentStatus>) {
        if let Some(status) = status.filter(|s| s.kind == AgentKind::Omp) {
            status.kind = AgentKind::Other;
        }
    }
    match msg {
        ServerMsg::AgentStatusChanged { status, .. } => downgrade(status.as_mut()),
        ServerMsg::TerminalCreated(info) => downgrade(info.agent.as_mut()),
        ServerMsg::TerminalRestored { terminal: Some(info), .. } => downgrade(info.agent.as_mut()),
        ServerMsg::Terminals(list) => {
            for info in list {
                downgrade(info.agent.as_mut());
            }
        }
        _ => {}
    }
}

/// Add sidecars only for opted-in clients without changing the legacy v7 status layout.
fn with_subagent_models(msg: ServerMsg) -> ServerMsg {
    fn models(status: &AgentStatus) -> Vec<SubagentModel> {
        status.subagents.iter().filter_map(|sub| Some(SubagentModel {
            id: sub.id.clone(),
            model: sub.model.clone()?,
        })).collect()
    }
    match msg {
        ServerMsg::Terminals(terminals) => {
            let models: Vec<_> = terminals.iter().filter_map(|info| {
                let models = models(info.agent.as_ref()?);
                (!models.is_empty()).then_some((info.id, models))
            }).collect();
            if models.is_empty() { ServerMsg::Terminals(terminals) }
            else { ServerMsg::TerminalsWithSubagentModels { terminals, models } }
        }
        ServerMsg::AgentStatusChanged { id, status: Some(status) } => {
            let models = models(&status);
            if models.is_empty() { ServerMsg::AgentStatusChanged { id, status: Some(status) } }
            else { ServerMsg::AgentStatusWithSubagentModels { id, status, models } }
        }
        other => other,
    }
}

async fn read_loop<R>(
    source: &mut R,
    mgr: &Arc<Manager>,
    conn: &ConnHandle,
    cancel: &CancellationToken,
    retire: bool,
) -> anyhow::Result<()>
where
    R: futures::Stream<Item = std::io::Result<bytes::BytesMut>> + Unpin,
{
    loop {
        let frame = tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            f = source.next() => f,
        };
        let Some(frame) = frame else { return Ok(()) };
        let msg: ClientMsg = decode(&frame.context("read frame")?).context("decode message")?;
        if retire && msg == ClientMsg::Shutdown {
            mgr.retire_if_empty(std::time::Duration::ZERO).await;
            return Ok(());
        }
        handle_msg(msg, mgr, conn).await;
    }
}

async fn handle_msg(msg: ClientMsg, mgr: &Arc<Manager>, conn: &ConnHandle) {
    match msg {
        ClientMsg::Hello { .. } => {
            conn.send(ServerMsg::Error { message: "unexpected Hello".into() });
        }
        ClientMsg::ListTerminals => {
            conn.send(ServerMsg::Terminals(mgr.list()));
        }
        ClientMsg::CreateTerminal { cwd, shell, args, cols, rows, worktree } => {
            let cwd = cwd.or_else(|| worktree.clone());
            let req = SpawnRequest { cwd, shell, args, cols, rows, worktree };
            if let Err(e) = mgr.create(req, |_| {}).await {
                tracing::warn!("create terminal failed: {e:#}");
                conn.send(ServerMsg::Error { message: format!("create terminal failed: {e:#}") });
            }
        }
        ClientMsg::Subscribe { id, resume_from_seq } => match mgr.get(id) {
            Some(t) => t.subscribe(conn, resume_from_seq),
            None => {
                conn.send(ServerMsg::Error { message: format!("no such terminal: {id}") });
            }
        },
        ClientMsg::Unsubscribe { id } => {
            if let Some(t) = mgr.get(id) {
                t.unsubscribe(conn.id);
            }
        }
        ClientMsg::Input { id, bytes } => {
            if let Some(t) = mgr.get(id) {
                t.input(bytes);
            }
        }
        ClientMsg::Resize { id, cols, rows } => {
            if let Some(t) = mgr.get(id) {
                t.resize(cols, rows);
            }
        }
        ClientMsg::KillTerminal { id } => {
            mgr.kill(id);
        }
        ClientMsg::ListProjects => {
            conn.send(projects::list(mgr).await);
        }
        ClientMsg::RefreshProjects => projects::broadcast(mgr).await,
        ClientMsg::AddProject { path, group } => projects::add_project(mgr, conn, path, group).await,
        ClientMsg::CreateGroup { name } => projects::create_group(mgr, conn, name).await,
        ClientMsg::RenameGroup { id, name } => projects::rename_group(mgr, conn, id, name).await,
        ClientMsg::RemoveGroup { id } => projects::remove_group(mgr, conn, id).await,
        ClientMsg::SetProjectGroup { project_id, group } => {
            projects::set_project_group(mgr, conn, project_id, group).await
        }
        ClientMsg::RemoveProject { id } => projects::remove_project(mgr, id).await,
        ClientMsg::CreateWorktree { project_id, name, base } => {
            projects::create_worktree(mgr, conn, project_id, name, base).await;
        }
        ClientMsg::RemoveWorktree { project_id, path, force, delete_branch } => {
            projects::remove_worktree(mgr, conn, project_id, path, force, delete_branch).await;
        }
        ClientMsg::AgentHook { term_id, agent, payload } => {
            mgr.agent_hook(term_id, agent, &payload);
        }
        ClientMsg::MarkSeen { id } => mgr.mark_seen(id),
        ClientMsg::GetSubagentActivity { .. } => {
            conn.send(ServerMsg::Error { message: "activity requests require a dedicated connection".into() });
        }
        ClientMsg::GetDaemonInfo => {
            conn.send(ServerMsg::DaemonInfo {
                build_id: mgr.build_id.clone(),
                proto_version: PROTO_VERSION,
                started_unix_ms: mgr.started_unix_ms,
            });
        }
        ClientMsg::SaveScrollback => {
            let files = mgr.save_scrollback().await;
            conn.send(ServerMsg::ScrollbackSaved { files });
        }
        ClientMsg::RestoreTerminal { request_id, cwd, cols, rows, worktree } => {
            let cwd = cwd.or_else(|| worktree.clone());
            let req = SpawnRequest { cwd, shell: None, args: Vec::new(), cols, rows, worktree };
            let result = mgr.create(req, |info| {
                conn.send(ServerMsg::TerminalRestored { request_id, terminal: Some(info.clone()) });
            }).await;
            if let Err(err) = result {
                tracing::warn!("restore terminal failed: {err:#}");
                conn.send(ServerMsg::TerminalRestored { request_id, terminal: None });
                conn.send(ServerMsg::Error { message: format!("restore terminal failed: {err:#}") });
            }
        }
        ClientMsg::Shutdown => {
            tracing::info!(conn = conn.id, "shutdown requested");
            mgr.shutdown.cancel();
        }
    }
}
