//! A subagent's model read from the agent's own local files (read-only, fail-soft).
//!
//! * Claude Code writes `<projects dir>/<session_id>/subagents/agent-<agent_id>.meta.json` next to the
//!   main transcript. The format is undocumented: we read at most [`MAX_META_BYTES`], take only the
//!   string field `model` and ignore everything else.
//! * Codex child rollouts carry the model in `session_meta` / `turn_context` lines (see
//!   [`crate::codex_rollout::parse_rollout_model`]).
//!
//! Like the Codex identity read, a probe is planned under the tracker lock, executed outside of it and
//! applied afterwards. Any error just means "no model": nothing is logged and nothing panics.

use std::fs::File;
use std::io::Read as _;
use std::path::{Path, PathBuf};

/// The meta file is read from at most this many bytes.
pub const MAX_META_BYTES: u64 = 64 * 1024;

/// Longest `agent_id` we build a path from.
const MAX_ID_LEN: usize = 64;

/// Which agent's file a probe reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelSource {
    ClaudeMeta,
    CodexRollout,
}

/// A model read the tracker wants done for a child hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelPlan {
    pub agent_id: String,
    /// The session the hook belonged to: a result never applies to another one.
    pub session_id: Option<String>,
    pub source: ModelSource,
    pub path: PathBuf,
}

/// The outcome of an executed [`ModelPlan`], fed back to the tracker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelResult {
    pub agent_id: String,
    pub session_id: Option<String>,
    pub source: ModelSource,
    pub model: Option<String>,
}

/// Runs the plan. Never fails: an unreadable or unusable file gives `model: None`.
pub fn execute(plan: &ModelPlan) -> ModelResult {
    let model = match plan.source {
        ModelSource::ClaudeMeta => read_claude_meta_model(&plan.path),
        ModelSource::CodexRollout => crate::codex_rollout::read_rollout_model(&plan.path, &plan.agent_id),
    };
    ModelResult { agent_id: plan.agent_id.clone(), session_id: plan.session_id.clone(), source: plan.source, model }
}

/// `[A-Za-z0-9_-]`, 1..=64 chars: safe to put into a file name.
fn valid_agent_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_ID_LEN && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Path of the subagent meta file for a hook's `transcript_path` + `agent_id`.
///
/// A subagent transcript `<dir>/agent-<id>.jsonl` has its meta file next to it; a main transcript
/// `<dir>/<stem>.jsonl` keeps it in `<dir>/<stem>/subagents/`. Anything else gives `None`.
pub fn claude_meta_path(transcript_path: &str, agent_id: &str) -> Option<PathBuf> {
    if !valid_agent_id(agent_id) || !transcript_path.to_ascii_lowercase().ends_with(".jsonl") {
        return None;
    }
    let path = Path::new(transcript_path);
    if !path.is_absolute() {
        return None;
    }
    let name = path.file_name()?.to_str()?;
    let meta_name = format!("agent-{agent_id}.meta.json");
    if name == format!("agent-{agent_id}.jsonl") {
        return Some(path.with_file_name(meta_name));
    }
    // The suffix is ASCII (checked above), so this cuts on a char boundary.
    let stem = Some(&name[..name.len().checked_sub(".jsonl".len())?]).filter(|s| !s.is_empty())?;
    Some(path.parent()?.join(stem).join("subagents").join(meta_name))
}

/// The non-empty trimmed string field `model` of a meta file; `None` for anything else.
pub fn parse_claude_meta_model(bytes: &[u8]) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let model = v.get("model")?.as_str()?.trim();
    (!model.is_empty()).then(|| model.to_string())
}

/// Reads (at most [`MAX_META_BYTES`]) and parses a meta file.
pub fn read_claude_meta_model(path: &Path) -> Option<String> {
    let mut buf = Vec::new();
    File::open(path).ok()?.take(MAX_META_BYTES).read_to_end(&mut buf).ok()?;
    parse_claude_meta_model(&buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn abs(rel: &str) -> String {
        std::env::temp_dir().join(rel).display().to_string()
    }

    #[test]
    fn meta_path_for_main_transcript() {
        let t = abs("proj/sess-1.jsonl");
        let want = Path::new(&abs("proj")).join("sess-1").join("subagents").join("agent-a_B-9.meta.json");
        assert_eq!(claude_meta_path(&t, "a_B-9"), Some(want));
        // Case of the extension does not matter.
        assert!(claude_meta_path(&abs("proj/sess-1.JSONL"), "a1").is_some());
    }

    #[test]
    fn meta_path_for_subagent_transcript() {
        let t = abs("proj/sess-1/subagents/agent-a1.jsonl");
        let want = Path::new(&abs("proj/sess-1/subagents")).join("agent-a1.meta.json");
        assert_eq!(claude_meta_path(&t, "a1"), Some(want));
        // A subagent transcript of another agent is treated as a main transcript.
        let other = claude_meta_path(&t, "b2").unwrap();
        assert!(other.ends_with(Path::new("agent-a1").join("subagents").join("agent-b2.meta.json")));
    }

    #[test]
    fn meta_path_rejects_bad_ids_and_paths() {
        let t = abs("proj/sess-1.jsonl");
        let long = "a".repeat(65);
        for id in ["", "a/b", "a\\b", "..", "a.b", "a b", "ї", long.as_str()] {
            assert_eq!(claude_meta_path(&t, id), None, "{id:?}");
        }
        assert!(claude_meta_path(&t, &"a".repeat(64)).is_some());
        for p in ["", "rel/x.jsonl", "x.jsonl", &abs("proj/x.json"), &abs("proj/x.txt"), &abs("proj/.jsonl")] {
            assert_eq!(claude_meta_path(p, "a1"), None, "{p:?}");
        }
    }

    #[test]
    fn parses_only_the_model_field() {
        let ok = br#"{"agentType":"scout","model":" claude-haiku-5-5 ","pickedEffortModel":"claude-opus-5-5","finished":true}"#;
        assert_eq!(parse_claude_meta_model(ok).as_deref(), Some("claude-haiku-5-5"));
        assert_eq!(parse_claude_meta_model(br#"{"pickedEffortModel":"x"}"#), None);
        assert_eq!(parse_claude_meta_model(br#"{"model":"  "}"#), None);
        assert_eq!(parse_claude_meta_model(br#"{"model":5}"#), None);
        assert_eq!(parse_claude_meta_model(br#"{"model":{"id":"x"}}"#), None);
        assert_eq!(parse_claude_meta_model(br#"["model"]"#), None);
        assert_eq!(parse_claude_meta_model(b"garbage"), None);
        assert_eq!(parse_claude_meta_model(b"\xff\xfe"), None);
        assert_eq!(parse_claude_meta_model(b""), None);
    }

    fn tmp() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!("sich-probe-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn reads_files_and_fails_soft() {
        let d = tmp();
        let p = d.join("a.meta.json");
        std::fs::write(&p, r#"{"model":"m-1"}"#).unwrap();
        assert_eq!(read_claude_meta_model(&p).as_deref(), Some("m-1"));
        assert_eq!(read_claude_meta_model(&d.join("missing.json")), None);
        assert_eq!(read_claude_meta_model(&d), None, "a directory is not a file");
        // Oversized: only the first 64 KiB are read, so the JSON is cut and rejected.
        let big = format!(r#"{{"model":"m-1","pad":"{}"}}"#, "x".repeat(MAX_META_BYTES as usize + 10));
        std::fs::write(&p, big).unwrap();
        assert_eq!(read_claude_meta_model(&p), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn execute_reads_the_planned_source() {
        let d = tmp();
        let meta = d.join("agent-a1.meta.json");
        std::fs::write(&meta, r#"{"model":"m-1"}"#).unwrap();
        let plan = ModelPlan { agent_id: "a1".into(), session_id: Some("s".into()), source: ModelSource::ClaudeMeta, path: meta };
        let r = execute(&plan);
        assert_eq!((r.model.as_deref(), r.agent_id.as_str(), r.session_id.as_deref()), (Some("m-1"), "a1", Some("s")));
        let rollout = d.join("r.jsonl");
        std::fs::write(&rollout, "{\"type\":\"turn_context\",\"payload\":{\"model\":\"gpt-x\"}}\n").unwrap();
        let plan = ModelPlan { agent_id: "a1".into(), session_id: None, source: ModelSource::CodexRollout, path: rollout };
        assert_eq!(execute(&plan).model.as_deref(), Some("gpt-x"));
        let missing = ModelPlan { path: d.join("nope"), ..plan };
        assert_eq!(execute(&missing).model, None);
        let _ = std::fs::remove_dir_all(&d);
    }
}
