//! What the connection manager reports to the UI: connection state, events, and the binary
//! frames pushed through per-subscription channels.

use serde::Serialize;
use serde_json::{Value, json};
use sich_remote_proto::{RejectReason, TermRef, Workspace};

/// Connection state shown in the UI. Serialized as `{"state": "...", ...}`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ConnState {
    Idle,
    Connecting,
    Connected { host_name: String },
    Reconnecting { attempt: u32, retry_in_ms: u64 },
    PairPending,
    /// Terminal state: reconnecting stopped. `reason` is a stable code, see `reject_code`.
    Rejected { reason: String, detail: Option<String> },
}

/// Stable string codes for the UI.
pub fn reject_code(r: &RejectReason) -> &'static str {
    match r {
        RejectReason::Unpaired => "unpaired",
        RejectReason::PairSecretInvalid => "pair_secret_invalid",
        RejectReason::PairDenied => "pair_denied",
        RejectReason::PairTimeout => "pair_timeout",
        RejectReason::Revoked => "revoked",
        RejectReason::Incompatible { .. } => "incompatible",
    }
}

/// Events for the UI. Implemented by the Tauri app and by test collectors.
pub trait Ui: Send + Sync + 'static {
    fn connection(&self, state: &ConnState);
    fn workspace(&self, ws: &Workspace);
    fn term_size(&self, term: &TermRef, cols: u16, rows: u16, fit: bool);
    fn term_exited(&self, term: &TermRef, code: Option<i32>);
    fn hosts_changed(&self);
    fn gateway_error(&self, message: &str);
}

/// Terminal data sink of one subscription (a Tauri `Channel` in the app).
pub trait OutputSink: Send + Sync + 'static {
    fn reset(&self, cols: u16, rows: u16);
    fn output(&self, bytes: Vec<u8>);
}

/// Binary channel frame tags (first byte).
pub const FRAME_RESET: u8 = 0;
pub const FRAME_OUTPUT: u8 = 1;

/// `[0, cols_hi, cols_lo, rows_hi, rows_lo]`
pub fn encode_reset_frame(cols: u16, rows: u16) -> Vec<u8> {
    let mut v = Vec::with_capacity(5);
    v.push(FRAME_RESET);
    v.extend_from_slice(&cols.to_be_bytes());
    v.extend_from_slice(&rows.to_be_bytes());
    v
}

/// `[1, ...bytes]`
pub fn encode_output_frame(bytes: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(1 + bytes.len());
    v.push(FRAME_OUTPUT);
    v.extend_from_slice(bytes);
    v
}

/// Workspace as JSON for the UI. Differs from plain serde only for terminals: the ambiguous
/// `exited: Option<Option<i32>>` becomes `exited: bool` + `exit_code: number | null`.
pub fn workspace_to_json(ws: &Workspace) -> Value {
    let mut v = serde_json::to_value(ws).unwrap_or(Value::Null);
    if let Some(terms) = v.get_mut("terminals").and_then(Value::as_array_mut) {
        for (t, src) in terms.iter_mut().zip(&ws.terminals) {
            if let Some(obj) = t.as_object_mut() {
                obj.insert("exited".into(), json!(src.exited.is_some()));
                obj.insert("exit_code".into(), json!(src.exited.flatten()));
            }
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use sich_remote_proto::{Terminal, TermRef};

    fn term(exited: Option<Option<i32>>) -> Terminal {
        Terminal {
            term: TermRef { daemon: "d".into(), id: 7 },
            title: "t".into(),
            cwd: "C:\\x".into(),
            worktree: None,
            cols: 80,
            rows: 24,
            exited,
            created_unix_ms: 1,
            agent: None,
        }
    }

    #[test]
    fn reset_frame_layout() {
        assert_eq!(encode_reset_frame(0x0102, 0x0304), vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn output_frame_layout() {
        assert_eq!(encode_output_frame(b"hi"), vec![1, b'h', b'i']);
        assert_eq!(encode_output_frame(b""), vec![1]);
    }

    #[test]
    fn workspace_json_flattens_exited() {
        let ws = Workspace {
            host_name: "PC".into(),
            groups: vec![],
            projects: vec![],
            terminals: vec![term(None), term(Some(None)), term(Some(Some(3)))],
        };
        let v = workspace_to_json(&ws);
        let t = v["terminals"].as_array().unwrap();
        assert_eq!(t[0]["exited"], json!(false));
        assert_eq!(t[0]["exit_code"], Value::Null);
        assert_eq!(t[1]["exited"], json!(true));
        assert_eq!(t[1]["exit_code"], Value::Null);
        assert_eq!(t[2]["exited"], json!(true));
        assert_eq!(t[2]["exit_code"], json!(3));
        assert_eq!(t[2]["term"]["id"], json!(7));
        assert_eq!(v["host_name"], json!("PC"));
    }

    #[test]
    fn conn_state_serializes_tagged() {
        assert_eq!(serde_json::to_value(ConnState::Idle).unwrap(), json!({"state": "idle"}));
        assert_eq!(
            serde_json::to_value(ConnState::Reconnecting { attempt: 2, retry_in_ms: 1000 }).unwrap(),
            json!({"state": "reconnecting", "attempt": 2, "retry_in_ms": 1000})
        );
        assert_eq!(
            serde_json::to_value(ConnState::Connected { host_name: "PC".into() }).unwrap(),
            json!({"state": "connected", "host_name": "PC"})
        );
        assert_eq!(serde_json::to_value(ConnState::PairPending).unwrap(), json!({"state": "pair_pending"}));
    }

    #[test]
    fn reject_codes_are_stable() {
        assert_eq!(reject_code(&RejectReason::Unpaired), "unpaired");
        assert_eq!(reject_code(&RejectReason::PairSecretInvalid), "pair_secret_invalid");
        assert_eq!(reject_code(&RejectReason::PairDenied), "pair_denied");
        assert_eq!(reject_code(&RejectReason::PairTimeout), "pair_timeout");
        assert_eq!(reject_code(&RejectReason::Revoked), "revoked");
        assert_eq!(reject_code(&RejectReason::Incompatible { gateway_version: 2, gateway_min: 2 }), "incompatible");
    }
}
