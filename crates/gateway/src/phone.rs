//! One phone connection: handshake (compat, pairing, allowlist), then the message loop.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use futures::{SinkExt as _, StreamExt as _};
use iroh::endpoint::{Connection, Incoming, RecvStream, SendStream, VarInt};
use sich_remote_proto::{
    GatewayMsg, MIN_COMPATIBLE, PhoneMsg, REMOTE_PROTO_VERSION, RejectReason, Workspace, compatible, decode_frame, encode_frame,
    frame_codec,
};
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};

use crate::hub::{Hub, PairDecision, Session, sanitize_name};
use crate::pairing::SecretCheck;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a rejected phone gets to read the `Rejected` message before we close.
const CLOSE_GRACE: Duration = Duration::from_secs(3);
const WORKSPACE_DEBOUNCE: Duration = Duration::from_millis(150);

type Reader = FramedRead<RecvStream, LengthDelimitedCodec>;
type Writer = FramedWrite<SendStream, LengthDelimitedCodec>;

async fn send(writer: &mut Writer, msg: &GatewayMsg) -> anyhow::Result<()> {
    writer.send(encode_frame(msg)?).await.context("send to phone")
}

/// Sends `Rejected`, flushes, finishes the stream and waits for the phone to close before
/// closing ourselves, so the message is really delivered.
async fn reject(conn: &Connection, mut writer: Writer, reason: RejectReason) {
    let _ = send(&mut writer, &GatewayMsg::Rejected { reason }).await;
    let _ = writer.get_mut().finish();
    let _ = tokio::time::timeout(CLOSE_GRACE, conn.closed()).await;
    conn.close(VarInt::from_u32(0), b"rejected");
}

pub async fn handle(hub: Arc<Hub>, incoming: Incoming) {
    let remote = match incoming.accept() {
        Ok(accepting) => match accepting.await {
            Ok(conn) => conn,
            Err(e) => {
                tracing::debug!("phone handshake failed: {e:#}");
                return;
            }
        },
        Err(e) => {
            tracing::debug!("incoming connection refused: {e:#}");
            return;
        }
    };
    if let Err(e) = session(hub, remote.clone()).await {
        tracing::debug!("phone connection ended: {e:#}");
    }
    remote.close(VarInt::from_u32(0), b"bye");
}

async fn session(hub: Arc<Hub>, conn: Connection) -> anyhow::Result<()> {
    let endpoint_id = conn.remote_id().to_string();
    let (send_stream, recv_stream) =
        tokio::time::timeout(HANDSHAKE_TIMEOUT, conn.accept_bi()).await.context("no stream opened")??;
    let mut reader: Reader = FramedRead::new(recv_stream, frame_codec());
    let mut writer: Writer = FramedWrite::new(send_stream, frame_codec());

    let first = tokio::time::timeout(HANDSHAKE_TIMEOUT, reader.next())
        .await
        .context("no Hello")?
        .context("closed before Hello")?
        .context("read Hello")?;
    let PhoneMsg::Hello { proto_version, min_compatible, device_name, pair_secret, app_version, .. } =
        decode_frame::<PhoneMsg>(&first).context("first message is not a Hello")?
    else {
        anyhow::bail!("first message is not Hello");
    };
    let name = sanitize_name(&device_name);
    tracing::info!(device = %name, id = %endpoint_id, app = %app_version, "phone connected");

    if !compatible(proto_version, min_compatible) {
        reject(&conn, writer, RejectReason::Incompatible { gateway_version: REMOTE_PROTO_VERSION, gateway_min: MIN_COMPATIBLE }).await;
        return Ok(());
    }

    if let Some(secret) = pair_secret {
        let valid = hub.pairing.lock().check(&secret, Instant::now()) == SecretCheck::Valid;
        if !valid {
            reject(&conn, writer, RejectReason::PairSecretInvalid).await;
            return Ok(());
        }
        send(&mut writer, &GatewayMsg::PairPending).await?;
        let decision = tokio::select! {
            d = hub.confirm_pair(&name, &endpoint_id) => d,
            _ = conn.closed() => return Ok(()),
        };
        match decision {
            PairDecision::Allowed => {
                if let Err(e) = hub.add_device(&endpoint_id, &name) {
                    tracing::error!("could not store paired device: {e:#}");
                    let _ = send(&mut writer, &GatewayMsg::Error { message: "could not save the pairing on the computer".into() }).await;
                    reject(&conn, writer, RejectReason::PairDenied).await;
                    return Ok(());
                }
                tracing::info!(device = %name, id = %endpoint_id, "phone paired");
            }
            PairDecision::Denied => {
                reject(&conn, writer, RejectReason::PairDenied).await;
                return Ok(());
            }
            PairDecision::Timeout => {
                reject(&conn, writer, RejectReason::PairTimeout).await;
                return Ok(());
            }
        }
    } else if !hub.is_paired(&endpoint_id) {
        reject(&conn, writer, RejectReason::Unpaired).await;
        return Ok(());
    }

    let (mut sess, cancel) = hub.register_phone(&endpoint_id);
    let mut changes = hub.changes();
    let outbox = sess.outbox.clone();
    outbox.push(GatewayMsg::Welcome {
        proto_version: REMOTE_PROTO_VERSION,
        min_compatible: MIN_COMPATIBLE,
        host_name: hub.host_name().to_string(),
        gateway_version: env!("CARGO_PKG_VERSION").to_string(),
        capabilities: vec!["fit".into(), "images".into(), "chat".into()],
    });
    let mut last_workspace = hub.workspace();
    outbox.push(GatewayMsg::Workspace(last_workspace.clone()));

    let writer_outbox = outbox.clone();
    let mut writer_task = tokio::spawn(async move {
        while let Some(msg) = writer_outbox.pop().await {
            if send(&mut writer, &msg).await.is_err() {
                return None;
            }
        }
        let _ = writer.get_mut().finish();
        Some(())
    });

    let mut debounce: Option<tokio::time::Instant> = None;
    let mut revoked = false;
    let mut writer_done = false;
    let result: anyhow::Result<()> = loop {
        tokio::select! {
            frame = reader.next() => {
                let Some(frame) = frame else { break Ok(()) };
                let frame = match frame {
                    Ok(f) => f,
                    Err(e) => break Err(e).context("read frame"),
                };
                match decode_frame::<PhoneMsg>(&frame) {
                    Ok(msg) => handle_msg(&hub, &mut sess, msg).await,
                    Err(e) => break Err(e).context("decode phone message"),
                }
            }
            _ = cancel.cancelled() => {
                revoked = true;
                break Ok(());
            }
            _ = hub.shutdown.cancelled() => break Ok(()),
            _ = conn.closed() => break Ok(()),
            _ = &mut writer_task => {
                writer_done = true;
                break Ok(());
            }
            changed = changes.changed() => {
                if changed.is_err() {
                    break Ok(());
                }
                debounce.get_or_insert_with(|| tokio::time::Instant::now() + WORKSPACE_DEBOUNCE);
            }
            _ = async { tokio::time::sleep_until(debounce.unwrap()).await }, if debounce.is_some() => {
                debounce = None;
                let ws: Workspace = hub.workspace();
                if ws != last_workspace {
                    last_workspace = ws.clone();
                    outbox.push(GatewayMsg::Workspace(ws));
                }
            }
        }
    };

    if revoked {
        outbox.push(GatewayMsg::Rejected { reason: RejectReason::Revoked });
    }
    // Closes the outbox: the writer sends what is queued, then finishes the stream.
    hub.unregister_phone(&mut sess);
    if revoked {
        tracing::info!(device = %name, "phone revoked, closing");
        if !writer_done {
            let _ = tokio::time::timeout(CLOSE_GRACE, &mut writer_task).await;
        }
        let _ = tokio::time::timeout(CLOSE_GRACE, conn.closed()).await;
    }
    writer_task.abort();
    result
}

async fn handle_msg(hub: &Arc<Hub>, sess: &mut Session, msg: PhoneMsg) {
    match msg {
        PhoneMsg::Hello { .. } => sess.outbox.push(GatewayMsg::Error { message: "unexpected Hello".into() }),
        PhoneMsg::Ping { nonce } => sess.outbox.push(GatewayMsg::Pong { nonce }),
        PhoneMsg::Subscribe { term, cols, rows, fit, replay_bytes } => hub.subscribe(sess, term, cols, rows, fit, replay_bytes),
        PhoneMsg::Unsubscribe { term } => hub.unsubscribe(sess, &term),
        PhoneMsg::Input { term, bytes } => hub.input(&term, bytes),
        PhoneMsg::SendText { term, text, images, submit } => {
            if let Err(message) = hub.send_text(&term, &text, &images, submit) {
                sess.outbox.push(GatewayMsg::Error { message });
            }
        }
        PhoneMsg::Resize { term, cols, rows } => hub.resize(sess, &term, cols, rows),
        PhoneMsg::SetFit { term, fit } => hub.set_fit(sess, &term, fit),
        PhoneMsg::MarkSeen { term } => hub.mark_seen(&term),
        PhoneMsg::UploadImage { req_id, file_name, data } => {
            tracing::debug!(file = %file_name, bytes = data.len(), "image upload");
            let result = hub.upload(data).await;
            sess.outbox.push(GatewayMsg::Uploaded { req_id, result });
        }
        PhoneMsg::ChatSubscribe { term, limit } => hub.chat_subscribe(sess, term, limit),
        PhoneMsg::ChatUnsubscribe { term } => hub.chat_unsubscribe(sess, &term),
        PhoneMsg::ChatLoadEarlier { term, before_order, limit } => hub.chat_load_earlier(sess, &term, before_order, limit),
        PhoneMsg::ChatImage { req_id, term, image_ref } => hub.chat_image(sess, req_id, &term, image_ref),
    }
}
