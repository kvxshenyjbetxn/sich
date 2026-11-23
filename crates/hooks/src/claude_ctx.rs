//! Per-session Claude Code context data shared with the GUI through `<sich config>/claude-ctx/`.
//!
//! Two producers write there: the statusLine relay (`<session>.status.json`, this module's
//! [`CtxStatus`]) and the bundled Claude Code mod (`<session>.mod.json`, see `claude_mod`).
//! Everything is read fail-soft: any field of Claude's statusLine payload may be missing.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::usage::config_file;

/// Directory name under the Sich config base.
pub const DIR_NAME: &str = "claude-ctx";
/// A status file is rewritten at least this often even when nothing changed.
pub const REFRESH_MS: u64 = 60_000;
const MAX_ID_LEN: usize = 128;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct CtxCurrent {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct CtxCache {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warm: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hit_ratio: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CtxStatus {
    pub v: u32,
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub term_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub socket: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub updated_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub used_percentage: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window_size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<CtxCurrent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exceeds_200k: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines_added: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines_removed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache: Option<CtxCache>,
}

impl Default for CtxStatus {
    fn default() -> Self {
        Self {
            v: 1,
            session_id: String::new(),
            term_id: None,
            socket: None,
            cwd: None,
            updated_ms: 0,
            version: None,
            model_id: None,
            model_name: None,
            effort: None,
            used_percentage: None,
            context_window_size: None,
            total_input_tokens: None,
            total_output_tokens: None,
            current: None,
            exceeds_200k: None,
            cost_usd: None,
            duration_ms: None,
            api_duration_ms: None,
            lines_added: None,
            lines_removed: None,
            cache: None,
        }
    }
}

/// The directory both producers write to (`<sich config>/claude-ctx`).
pub fn ctx_dir() -> Option<PathBuf> {
    config_file(DIR_NAME)
}

/// The Sich config directory (`<base>/sich`) that holds `claude-ctx/` and `claude-mod/`.
pub fn sich_config_dir() -> Option<PathBuf> {
    ctx_dir()?.parent().map(Path::to_path_buf)
}

/// Keeps only `[A-Za-z0-9_-]`; `None` when nothing is left (or the id is absurdly long).
pub fn sanitize_session_id(id: &str) -> Option<String> {
    let clean: String = id.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-')).collect();
    (!clean.is_empty() && clean.len() <= MAX_ID_LEN).then_some(clean)
}

pub fn status_path(dir: &Path, session_id: &str) -> Option<PathBuf> {
    Some(dir.join(format!("{}.status.json", sanitize_session_id(session_id)?)))
}

pub fn mod_path(dir: &Path, session_id: &str) -> Option<PathBuf> {
    Some(dir.join(format!("{}.mod.json", sanitize_session_id(session_id)?)))
}

fn text(v: &Value) -> Option<String> {
    v.as_str().map(str::to_owned)
}

fn count(v: &Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_f64().filter(|f| f.is_finite() && *f >= 0.0).map(|f| f as u64))
}

fn float(v: &Value) -> Option<f64> {
    v.as_f64().filter(|f| f.is_finite())
}

/// Builds the status from Claude's statusLine JSON. `None` without a `session_id`.
pub fn status_from_payload(
    payload: &Value,
    term_id: Option<String>,
    socket: Option<String>,
    now_ms: u64,
) -> Option<CtxStatus> {
    let session_id = payload["session_id"].as_str().filter(|id| !id.is_empty())?.to_owned();
    let window = &payload["context_window"];
    let usage = &window["current_usage"];
    let current = CtxCurrent {
        input: count(&usage["input_tokens"]),
        output: count(&usage["output_tokens"]),
        cache_creation: count(&usage["cache_creation_input_tokens"]),
        cache_read: count(&usage["cache_read_input_tokens"]),
    };
    let current = (current != CtxCurrent::default()).then_some(current);
    let cache_json = &payload["prompt_cache"];
    let cache = CtxCache {
        warm: cache_json["warm"].as_bool(),
        // Tolerate a numeric ttl as well.
        ttl: text(&cache_json["ttl"]).or_else(|| cache_json["ttl"].as_i64().map(|n| n.to_string())),
        hit_ratio: float(&cache_json["hit_ratio"]),
        expires_at: cache_json["expires_at"].as_i64().or_else(|| float(&cache_json["expires_at"]).map(|f| f as i64)),
    };
    let cache = (cache != CtxCache::default()).then_some(cache);
    let cost = &payload["cost"];
    Some(CtxStatus {
        v: 1,
        session_id,
        term_id,
        socket,
        cwd: text(&payload["cwd"]),
        updated_ms: now_ms,
        version: text(&payload["version"]),
        model_id: text(&payload["model"]["id"]),
        model_name: text(&payload["model"]["display_name"]),
        effort: text(&payload["effort"]["level"]),
        used_percentage: float(&window["used_percentage"]),
        context_window_size: count(&window["context_window_size"]),
        total_input_tokens: count(&window["total_input_tokens"]),
        total_output_tokens: count(&window["total_output_tokens"]),
        current,
        exceeds_200k: payload["exceeds_200k_tokens"].as_bool(),
        cost_usd: float(&cost["total_cost_usd"]),
        duration_ms: count(&cost["total_duration_ms"]),
        api_duration_ms: count(&cost["total_api_duration_ms"]),
        lines_added: count(&cost["total_lines_added"]),
        lines_removed: count(&cost["total_lines_removed"]),
        cache,
    })
}

/// Write only when something besides `updated_ms` changed, or the file is older than a minute.
pub fn should_write(existing: Option<&CtxStatus>, next: &CtxStatus) -> bool {
    let Some(old) = existing else { return true };
    let same = CtxStatus { updated_ms: next.updated_ms, ..old.clone() } == *next;
    !same || next.updated_ms.saturating_sub(old.updated_ms) >= REFRESH_MS
}

fn read_status(path: &Path) -> Option<CtxStatus> {
    use std::io::Read as _;
    let mut bytes = Vec::new();
    fs::File::open(path).ok()?.take(64 * 1024).read_to_end(&mut bytes).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Writes `<dir>/<session>.status.json` for the payload when [`should_write`] says so.
/// Returns whether a file was written.
pub fn record_status(
    dir: &Path,
    payload: &[u8],
    term_id: Option<String>,
    socket: Option<String>,
    now_ms: u64,
) -> anyhow::Result<bool> {
    let value: Value = serde_json::from_slice(payload)?;
    let Some(next) = status_from_payload(&value, term_id, socket, now_ms) else { return Ok(false) };
    let Some(path) = status_path(dir, &next.session_id) else { return Ok(false) };
    if !should_write(read_status(&path).as_ref(), &next) {
        return Ok(false);
    }
    crate::statusline::write_json(&path, &next)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> Value {
        json!({
            "session_id": "abc-123",
            "cwd": "C:/work/app",
            "model": {"id": "claude-opus-5-5", "display_name": "Opus 5.5"},
            "effort": {"level": "high"},
            "version": "2.1.330",
            "cost": {"total_cost_usd": 0.338, "total_duration_ms": 209036, "total_api_duration_ms": 110845,
                     "total_lines_added": 0, "total_lines_removed": 0},
            "context_window": {
                "total_input_tokens": 58420, "total_output_tokens": 902, "context_window_size": 1000000,
                "current_usage": {"input_tokens": 4, "output_tokens": 902,
                                  "cache_creation_input_tokens": 2803, "cache_read_input_tokens": 55613},
                "used_percentage": 6, "remaining_percentage": 94
            },
            "exceeds_200k_tokens": false,
            "prompt_cache": {"warm": true, "ttl": "1h", "hit_ratio": 0.876, "expires_at": 1795400300, "extra": 1}
        })
    }

    #[test]
    fn extracts_the_full_sample() {
        let s = status_from_payload(&sample(), Some("7".into()), Some("sock".into()), 1234).unwrap();
        assert_eq!(s.v, 1);
        assert_eq!(s.session_id, "abc-123");
        assert_eq!(s.term_id.as_deref(), Some("7"));
        assert_eq!(s.socket.as_deref(), Some("sock"));
        assert_eq!(s.cwd.as_deref(), Some("C:/work/app"));
        assert_eq!(s.updated_ms, 1234);
        assert_eq!(s.version.as_deref(), Some("2.1.330"));
        assert_eq!(s.model_id.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(s.model_name.as_deref(), Some("Opus 5.5"));
        assert_eq!(s.effort.as_deref(), Some("high"));
        assert_eq!(s.used_percentage, Some(6.0));
        assert_eq!(s.context_window_size, Some(1_000_000));
        assert_eq!(s.total_input_tokens, Some(58420));
        assert_eq!(s.total_output_tokens, Some(902));
        assert_eq!(
            s.current,
            Some(CtxCurrent { input: Some(4), output: Some(902), cache_creation: Some(2803), cache_read: Some(55613) })
        );
        assert_eq!(s.exceeds_200k, Some(false));
        assert_eq!(s.cost_usd, Some(0.338));
        assert_eq!(s.duration_ms, Some(209036));
        assert_eq!(s.api_duration_ms, Some(110845));
        assert_eq!((s.lines_added, s.lines_removed), (Some(0), Some(0)));
        assert_eq!(
            s.cache,
            Some(CtxCache { warm: Some(true), ttl: Some("1h".into()), hit_ratio: Some(0.876), expires_at: Some(1795400300) })
        );
        let out = serde_json::to_value(&s).unwrap();
        assert_eq!(out["current"]["cache_read"], 55613);
        assert_eq!(out["cache"]["expires_at"], 1795400300);
        assert!(out.get("remaining_percentage").is_none());
    }

    #[test]
    fn missing_null_and_wrong_typed_fields_are_omitted() {
        assert!(status_from_payload(&json!({}), None, None, 1).is_none());
        assert!(status_from_payload(&json!({"session_id": ""}), None, None, 1).is_none());
        assert!(status_from_payload(&json!({"session_id": 5}), None, None, 1).is_none());
        let s = status_from_payload(
            &json!({"session_id": "s", "model": null, "context_window": {"used_percentage": null,
                "current_usage": null, "context_window_size": "big"}, "cost": [], "prompt_cache": 3}),
            None,
            None,
            9,
        )
        .unwrap();
        assert_eq!(s, CtxStatus { session_id: "s".into(), updated_ms: 9, ..Default::default() });
        let out = serde_json::to_value(&s).unwrap();
        assert_eq!(out.as_object().unwrap().len(), 3, "{out}");
    }

    #[test]
    fn serialized_status_round_trips() {
        let s = status_from_payload(&sample(), None, None, 5).unwrap();
        let back: CtxStatus = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
        let partial: CtxStatus = serde_json::from_str(r#"{"session_id":"x"}"#).unwrap();
        assert_eq!(partial.session_id, "x");
    }

    #[test]
    fn sanitizes_session_ids_for_file_names() {
        assert_eq!(sanitize_session_id("ab-C_9"), Some("ab-C_9".into()));
        assert_eq!(sanitize_session_id("../..\\x/y.json"), Some("xyjson".into()));
        assert_eq!(sanitize_session_id("..."), None);
        assert_eq!(sanitize_session_id(""), None);
        assert_eq!(sanitize_session_id(&"a".repeat(129)), None);
        let dir = Path::new("d");
        assert_eq!(status_path(dir, "a/b"), Some(dir.join("ab.status.json")));
        assert_eq!(mod_path(dir, "a/b"), Some(dir.join("ab.mod.json")));
        assert_eq!(status_path(dir, "/"), None);
        assert_eq!(mod_path(dir, "."), None);
    }

    #[test]
    fn write_rule_ignores_updated_ms_until_a_minute_passed() {
        let a = status_from_payload(&sample(), None, None, 1_000).unwrap();
        assert!(should_write(None, &a));
        let same = CtxStatus { updated_ms: 1_000 + REFRESH_MS - 1, ..a.clone() };
        assert!(!should_write(Some(&a), &same));
        let old = CtxStatus { updated_ms: 1_000 + REFRESH_MS, ..a.clone() };
        assert!(should_write(Some(&a), &old));
        let changed = CtxStatus { used_percentage: Some(7.0), updated_ms: 2_000, ..a.clone() };
        assert!(should_write(Some(&a), &changed));
        // A clock that went backwards must not suppress changes or panic.
        let earlier = CtxStatus { updated_ms: 10, ..a.clone() };
        assert!(!should_write(Some(&a), &earlier));
    }

    #[test]
    fn record_status_writes_atomically_and_only_on_change() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("claude-ctx");
        let bytes = sample().to_string().into_bytes();
        assert!(record_status(&dir, &bytes, Some("3".into()), None, 1_000).unwrap());
        let path = dir.join("abc-123.status.json");
        assert_eq!(read_status(&path).unwrap().updated_ms, 1_000);
        // Same content soon after: untouched.
        assert!(!record_status(&dir, &bytes, Some("3".into()), None, 5_000).unwrap());
        assert_eq!(read_status(&path).unwrap().updated_ms, 1_000);
        // Changed content: rewritten.
        let mut changed = sample();
        changed["context_window"]["used_percentage"] = json!(7);
        assert!(record_status(&dir, changed.to_string().as_bytes(), Some("3".into()), None, 6_000).unwrap());
        assert_eq!(read_status(&path).unwrap().used_percentage, Some(7.0));
        // Stale file: refreshed even when identical.
        assert!(record_status(&dir, changed.to_string().as_bytes(), Some("3".into()), None, 70_000).unwrap());
        assert_eq!(read_status(&path).unwrap().updated_ms, 70_000);
        // Garbage, no session id and an unusable id write nothing and do not panic.
        assert!(record_status(&dir, b"garbage", None, None, 1).is_err());
        assert!(!record_status(&dir, b"{}", None, None, 1).unwrap());
        assert!(!record_status(&dir, br#"{"session_id":"//"}"#, None, None, 1).unwrap());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
    }
}
