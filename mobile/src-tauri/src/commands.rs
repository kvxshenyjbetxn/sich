//! Tauri commands: thin wrappers over [`ConnectionManager`] and the hosts store.

use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde_json::Value;
use sich_remote_proto::{PairUrl, TermRef};
use tauri::ipc::{Channel, InvokeBody, InvokeResponseBody, Request};
use tauri::{AppHandle, Emitter, State};

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

/// Raw request body = image bytes; the file name travels percent-encoded in `x-file-name`.
#[tauri::command]
pub async fn upload_image(request: Request<'_>, state: State<'_, AppState>) -> Result<String, String> {
    let InvokeBody::Raw(data) = request.body() else {
        return Err("expected a binary body".into());
    };
    let name = request
        .headers()
        .get("x-file-name")
        .and_then(|v| v.to_str().ok())
        .map(percent_decode)
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "image.jpg".to_string());
    state.mgr.upload(name, data.clone()).await
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

/// Lenient percent-decoding for header values; invalid escapes are kept literally.
pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(h) = s.get(i + 1..i + 3)
            && let Ok(v) = u8::from_str_radix(h, 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::percent_decode;

    #[test]
    fn decodes_utf8_and_spaces() {
        assert_eq!(percent_decode("photo%20%D1%84%D0%BE%D1%82%D0%BE.jpg"), "photo фото.jpg");
    }

    #[test]
    fn keeps_invalid_escapes() {
        assert_eq!(percent_decode("a%zzb%"), "a%zzb%");
        assert_eq!(percent_decode("100%"), "100%");
    }

    #[test]
    fn plain_passes_through() {
        assert_eq!(percent_decode("a.png"), "a.png");
        assert_eq!(percent_decode(""), "");
    }
}
