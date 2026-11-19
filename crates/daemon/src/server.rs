//! Per-connection protocol handling.

use std::sync::Arc;

use anyhow::Context as _;
use futures::{SinkExt as _, StreamExt as _};
use sich_proto::{ClientMsg, PROTO_VERSION, ServerMsg, decode, encode, frame_codec};
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
    match decode::<ClientMsg>(&first).context("decode Hello")? {
        ClientMsg::Hello { proto_version, client_name } if proto_version == PROTO_VERSION => {
            tracing::info!(conn = conn_id, %client_name, "client connected");
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
    }

    let (tx, mut rx) = mpsc::channel::<ServerMsg>(WRITE_QUEUE);
    let cancel = CancellationToken::new();
    let conn = ConnHandle::new(conn_id, tx, cancel.clone());

    // Welcome goes first in the queue, then the connection starts receiving broadcasts.
    conn.send(ServerMsg::Welcome { proto_version: PROTO_VERSION, daemon_pid: std::process::id() });
    mgr.hub.register(conn.clone());

    let writer_cancel = cancel.clone();
    let writer = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = writer_cancel.cancelled() => break,
                msg = rx.recv() => {
                    let Some(msg) = msg else { break };
                    let Ok(frame) = encode(&msg) else { continue };
                    if sink.send(frame).await.is_err() {
                        break;
                    }
                }
            }
        }
        writer_cancel.cancel();
    });

    let result = read_loop(&mut source, mgr, &conn, &cancel).await;
    cancel.cancel();
    let _ = writer.await;
    result
}

async fn read_loop<R>(
    source: &mut R,
    mgr: &Arc<Manager>,
    conn: &ConnHandle,
    cancel: &CancellationToken,
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
            if let Err(e) = mgr.create(req).await {
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
            conn.send(ServerMsg::Projects(projects::list(mgr).await));
        }
        ClientMsg::RefreshProjects => projects::broadcast(mgr).await,
        ClientMsg::AddProject { path } => projects::add_project(mgr, conn, path).await,
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
        ClientMsg::Shutdown => {
            tracing::info!(conn = conn.id, "shutdown requested");
            mgr.shutdown.cancel();
        }
    }
}
