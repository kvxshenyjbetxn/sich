//! Chat view support: converts gateway chat messages to the JSON the UI consumes and defines the
//! per-subscription event sink. Self-contained so the chat experiment can be removed cleanly.

use serde_json::{Value, json};
use sich_remote_proto::{ChatBlock, ChatMessage, ChatRole, ChatStatus};

/// Capability string the gateway advertises when it supports the chat view.
pub const CAP_CHAT: &str = "chat";

/// Receives chat events of the active chat subscription (a Tauri `Channel` in the app).
/// Events are JSON objects tagged by `kind`: `snapshot`, `update`, `earlier`.
pub trait ChatSink: Send + Sync + 'static {
    fn event(&self, ev: Value);
}

pub fn role_str(r: &ChatRole) -> &'static str {
    match r {
        ChatRole::User => "user",
        ChatRole::Assistant => "assistant",
        ChatRole::Tool => "tool",
        ChatRole::Reasoning => "reasoning",
        ChatRole::System => "system",
    }
}

pub fn block_json(b: &ChatBlock) -> Value {
    match b {
        ChatBlock::Text { markdown } => json!({ "type": "text", "markdown": markdown }),
        ChatBlock::ToolCall { call_id, name, summary } => {
            json!({ "type": "tool_call", "call_id": call_id, "name": name, "summary": summary })
        }
        ChatBlock::ToolResult { call_id, output, is_error } => {
            json!({ "type": "tool_result", "call_id": call_id, "output": output, "is_error": is_error })
        }
        ChatBlock::Image { image_ref, alt } => json!({ "type": "image", "image_ref": image_ref, "alt": alt }),
    }
}

pub fn message_json(m: &ChatMessage) -> Value {
    json!({
        "id": m.id,
        "order": m.order,
        "role": role_str(&m.role),
        "timestamp_ms": m.timestamp_ms,
        "blocks": m.blocks.iter().map(block_json).collect::<Vec<_>>(),
    })
}

pub fn status_json(s: &ChatStatus) -> Value {
    match s {
        ChatStatus::Ready => json!({ "state": "ready" }),
        ChatStatus::Pending => json!({ "state": "pending" }),
        ChatStatus::Unavailable { reason } => json!({ "state": "unavailable", "reason": reason }),
    }
}

fn messages_json(ms: &[ChatMessage]) -> Vec<Value> {
    ms.iter().map(message_json).collect()
}

pub fn snapshot_event(status: &ChatStatus, messages: &[ChatMessage], has_more: bool) -> Value {
    json!({ "kind": "snapshot", "status": status_json(status), "messages": messages_json(messages), "has_more": has_more })
}

pub fn update_event(messages: &[ChatMessage]) -> Value {
    json!({ "kind": "update", "messages": messages_json(messages) })
}

pub fn earlier_event(messages: &[ChatMessage], has_more: bool) -> Value {
    json!({ "kind": "earlier", "messages": messages_json(messages), "has_more": has_more })
}

/// `data:<mime>;base64,<data>`; a non-image or empty mime falls back to `image/jpeg`.
pub fn data_url(mime: &str, data: &[u8]) -> String {
    use base64::Engine;
    let mime = mime.trim();
    let mime = if mime.starts_with("image/") && !mime.contains([';', ',', ' ']) { mime } else { "image/jpeg" };
    format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(data))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg() -> ChatMessage {
        ChatMessage {
            id: "m1".into(),
            order: 42,
            role: ChatRole::Assistant,
            timestamp_ms: Some(1000),
            blocks: vec![
                ChatBlock::Text { markdown: "hi".into() },
                ChatBlock::ToolCall { call_id: "c".into(), name: "Bash".into(), summary: "ls".into() },
                ChatBlock::ToolResult { call_id: "c".into(), output: "ok".into(), is_error: true },
                ChatBlock::Image { image_ref: "r".into(), alt: "a".into() },
            ],
        }
    }

    #[test]
    fn message_shape() {
        let v = message_json(&msg());
        assert_eq!(v["id"], "m1");
        assert_eq!(v["order"], 42);
        assert_eq!(v["role"], "assistant");
        assert_eq!(v["timestamp_ms"], 1000);
        let b = v["blocks"].as_array().unwrap();
        assert_eq!(b[0], json!({ "type": "text", "markdown": "hi" }));
        assert_eq!(b[1]["type"], "tool_call");
        assert_eq!(b[1]["summary"], "ls");
        assert_eq!(b[2]["is_error"], true);
        assert_eq!(b[3]["image_ref"], "r");
    }

    #[test]
    fn missing_timestamp_is_null() {
        let mut m = msg();
        m.timestamp_ms = None;
        assert_eq!(message_json(&m)["timestamp_ms"], Value::Null);
    }

    #[test]
    fn roles_are_lowercase() {
        assert_eq!(role_str(&ChatRole::User), "user");
        assert_eq!(role_str(&ChatRole::Tool), "tool");
        assert_eq!(role_str(&ChatRole::Reasoning), "reasoning");
        assert_eq!(role_str(&ChatRole::System), "system");
    }

    #[test]
    fn status_shapes() {
        assert_eq!(status_json(&ChatStatus::Ready), json!({ "state": "ready" }));
        assert_eq!(status_json(&ChatStatus::Pending), json!({ "state": "pending" }));
        assert_eq!(
            status_json(&ChatStatus::Unavailable { reason: "x".into() }),
            json!({ "state": "unavailable", "reason": "x" })
        );
    }

    #[test]
    fn events_are_tagged() {
        let s = snapshot_event(&ChatStatus::Ready, &[msg()], true);
        assert_eq!(s["kind"], "snapshot");
        assert_eq!(s["has_more"], true);
        assert_eq!(s["messages"].as_array().unwrap().len(), 1);
        let u = update_event(&[]);
        assert_eq!(u["kind"], "update");
        assert!(u["messages"].as_array().unwrap().is_empty());
        let e = earlier_event(&[msg()], false);
        assert_eq!(e["kind"], "earlier");
        assert_eq!(e["has_more"], false);
    }

    #[test]
    fn data_url_encodes_and_sanitizes_mime() {
        assert_eq!(data_url("image/png", &[1, 2, 3]), "data:image/png;base64,AQID");
        assert_eq!(data_url("", &[1]), "data:image/jpeg;base64,AQ==");
        assert_eq!(data_url("text/html", &[1]), "data:image/jpeg;base64,AQ==");
        assert_eq!(data_url("image/png;evil,x", &[1]), "data:image/jpeg;base64,AQ==");
    }
}
