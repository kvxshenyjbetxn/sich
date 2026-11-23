//! Tauri commands: thin wrappers over [`ConnectionManager`] and the hosts store.

use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde_json::Value;
use sich_remote_proto::{PairUrl, TermRef};
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{AppHandle, Emitter, State};

use crate::chat::{self, ChatSink};
use crate::conn::ConnectionManager;
use crate::hosts::{Host, HostStore};
use crate::ui::{
    ConnState, OutputSink, Ui, encode_output_frame, encode_reset_frame, workspace_to_json,
};

pub struct AppState {
    pub mgr: ConnectionManager,
    pub store: Arc<Mutex<HostStore>>,
}

/// Forwards manager events to the WebView as Tauri events.
pub struct TauriUi {
    pub app: AppHandle,
}

impl Ui for TauriUi {
    fn connection(&self, state: &ConnState) {
        let _ = self.app.emit("connection", state);
    }
    fn workspace(&self, ws: &sich_remote_proto::Workspace) {
        let _ = self.app.emit("workspace", workspace_to_json(ws));
    }
    fn term_size(&self, term: &TermRef, cols: u16, rows: u16, fit: bool) {
        let _ = self.app.emit("term_size", serde_json::json!({ "term": term, "cols": cols, "rows": rows, "fit": fit }));
    }
    fn term_exited(&self, term: &TermRef, code: Option<i32>) {
        let _ = self.app.emit("term_exited", serde_json::json!({ "term": term, "code": code }));
    }
    fn hosts_changed(&self) {
        let _ = self.app.emit("hosts", ());
    }
    fn gateway_error(&self, message: &str) {
        let _ = self.app.emit("gateway_error", message);
    }
}

/// Terminal data goes through a per-subscription channel as raw binary frames:
/// `[0, cols_be16, rows_be16]` = Reset, `[1, ...bytes]` = Output (see `ui.rs`).
struct ChannelSink(Channel<InvokeResponseBody>);

impl OutputSink for ChannelSink {
    fn reset(&self, cols: u16, rows: u16) {
        let _ = self.0.send(InvokeResponseBody::Raw(encode_reset_frame(cols, rows)));
    }
    fn output(&self, bytes: Vec<u8>) {
        let _ = self.0.send(InvokeResponseBody::Raw(encode_output_frame(&bytes)));
    }
}

/// Chat events go through a per-subscription channel as JSON objects (see `chat.rs`).
struct ChatChannelSink(Channel<Value>);

impl ChatSink for ChatChannelSink {
    fn event(&self, ev: Value) {
        let _ = self.0.send(ev);
    }
}

#[derive(Serialize)]
pub struct HostsList {
    hosts: Vec<Host>,
    active: Option<String>,
}

#[tauri::command]
pub fn hosts_list(state: State<'_, AppState>) -> HostsList {
    let store = state.store.lock().unwrap();
    HostsList { hosts: store.hosts().to_vec(), active: store.active_id().map(str::to_string) }
}

#[tauri::command]
pub fn host_forget(state: State<'_, AppState>, id: String) -> Result<(), String> {
    let was_active = {
        let mut store = state.store.lock().unwrap();
        let was_active = store.active_id() == Some(id.as_str());
        store.remove(&id).map_err(|e| e.to_string())?;
        was_active
    };
    if was_active {
        state.mgr.stop();
    }
    Ok(())
}

#[tauri::command]
pub fn pair_start(state: State<'_, AppState>, url: String) -> Result<(), String> {
    let parsed = PairUrl::parse(url.trim()).map_err(|e| e.to_string())?;
    state.mgr.pair(parsed);
    Ok(())
}

#[tauri::command]
pub fn host_connect(state: State<'_, AppState>, id: String) -> Result<(), String> {
    let host = {
        let mut store = state.store.lock().unwrap();
        let host = store.get(&id).cloned().ok_or_else(|| "unknown host".to_string())?;
        store.set_active(&id).map_err(|e| e.to_string())?;
        host
    };
    state.mgr.connect(host);
    Ok(())
}

#[tauri::command]
pub fn retry_now(state: State<'_, AppState>) {
    state.mgr.retry_now();
}

#[tauri::command]
pub fn subscribe(
    state: State<'_, AppState>,
    term: TermRef,
    cols: u16,
    rows: u16,
    fit: bool,
    on_output: Channel<InvokeResponseBody>,
) {
    state.mgr.subscribe(term, cols, rows, fit, Arc::new(ChannelSink(on_output)));
}

#[tauri::command]
pub fn unsubscribe(state: State<'_, AppState>, term: TermRef) {
    state.mgr.unsubscribe(term);
}

#[tauri::command]
pub fn send_input(state: State<'_, AppState>, term: TermRef, bytes: Vec<u8>) {
    state.mgr.input(term, bytes);
}

#[tauri::command]
pub fn send_text(state: State<'_, AppState>, term: TermRef, text: String, image_ids: Vec<String>, submit: bool) {
    state.mgr.send_text(term, text, image_ids, submit);
}

/// Image upload: JSON args (Android's IPC does not deliver raw binary bodies). Resolves to the upload id.
#[tauri::command]
pub async fn upload_image(
    state: State<'_, AppState>,
    file_name: String,
    data_b64: String,
) -> Result<String, String> {
    let (name, data) = decode_upload(&file_name, &data_b64)?;
    state.mgr.upload(name, data).await
}

#[tauri::command]
pub fn resize(state: State<'_, AppState>, term: TermRef, cols: u16, rows: u16) {
    state.mgr.resize(term, cols, rows);
}

#[tauri::command]
pub fn set_fit(state: State<'_, AppState>, term: TermRef, fit: bool) {
    state.mgr.set_fit(term, fit);
}

#[tauri::command]
pub fn mark_seen(state: State<'_, AppState>, term: TermRef) {
    state.mgr.mark_seen(term);
}

#[tauri::command]
pub fn connection_state(state: State<'_, AppState>) -> ConnState {
    state.mgr.state()
}

/// Latest workspace snapshot (so a freshly mounted UI does not wait for the next event).
#[tauri::command]
pub fn workspace_snapshot(state: State<'_, AppState>) -> Option<Value> {
    state.mgr.workspace().map(|w| workspace_to_json(&w))
}

/// Capabilities the gateway advertised (the UI shows the chat toggle only with `chat`).
#[tauri::command]
pub fn gateway_capabilities(state: State<'_, AppState>) -> Vec<String> {
    state.mgr.capabilities()
}

#[tauri::command]
pub fn chat_subscribe(state: State<'_, AppState>, term: TermRef, limit: u32, on_event: Channel<Value>) {
    state.mgr.chat_subscribe(term, limit, Arc::new(ChatChannelSink(on_event)));
}

#[tauri::command]
pub fn chat_unsubscribe(state: State<'_, AppState>, term: TermRef) {
    state.mgr.chat_unsubscribe(term);
}

#[tauri::command]
pub fn chat_load_earlier(state: State<'_, AppState>, term: TermRef, before_order: u64, limit: u32) {
    state.mgr.chat_load_earlier(term, before_order, limit);
}

/// Resolves to a `data:` URL of the transcript image.
#[tauri::command]
pub async fn chat_image(state: State<'_, AppState>, term: TermRef, image_ref: String) -> Result<String, String> {
    let img = state.mgr.chat_image(term, image_ref).await?;
    Ok(chat::data_url(&img.mime, &img.data))
}

/// Max decoded image size (same as the gateway).
const MAX_DECODED_BYTES: usize = 15 * 1024 * 1024;

/// Validate and decode an upload request: non-empty valid base64, at most 15 MiB decoded.
/// An empty file name falls back to `image.jpg`.
pub fn decode_upload(file_name: &str, data_b64: &str) -> Result<(String, Vec<u8>), String> {
    use base64::Engine;
    let b64 = data_b64.trim();
    if b64.is_empty() {
        return Err("empty image data".into());
    }
    if b64.len() > MAX_DECODED_BYTES / 3 * 4 + 4 {
        return Err("image too large".into());
    }
    let data = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| format!("invalid base64: {e}"))?;
    if data.is_empty() {
        return Err("empty image data".into());
    }
    if data.len() > MAX_DECODED_BYTES {
        return Err("image too large".into());
    }
    let name = file_name.trim();
    let name = if name.is_empty() { "image.jpg" } else { name };
    Ok((name.to_string(), data))
}

#[cfg(test)]
mod tests {
    use super::{MAX_DECODED_BYTES, decode_upload};
    use base64::Engine;

    fn enc(b: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(b)
    }

    #[test]
    fn decodes_valid() {
        let (n, d) = decode_upload("a.png", &enc(&[1, 2, 3, 4])).unwrap();
        assert_eq!(n, "a.png");
        assert_eq!(d, vec![1, 2, 3, 4]);
    }

    #[test]
    fn empty_name_falls_back() {
        assert_eq!(decode_upload("  ", &enc(&[1])).unwrap().0, "image.jpg");
    }

    #[test]
    fn rejects_invalid_base64() {
        let e = decode_upload("a.png", "not*base64!").unwrap_err();
        assert!(e.contains("invalid base64"), "{e}");
    }

    #[test]
    fn rejects_empty() {
        assert!(decode_upload("a.png", "").is_err());
        assert!(decode_upload("a.png", "   ").is_err());
    }

    #[test]
    fn rejects_too_large() {
        let big = vec![0u8; MAX_DECODED_BYTES + 1];
        assert_eq!(decode_upload("a.png", &enc(&big)).unwrap_err(), "image too large");
        let ok = vec![0u8; MAX_DECODED_BYTES];
        assert!(decode_upload("a.png", &enc(&ok)).is_ok());
    }
}
