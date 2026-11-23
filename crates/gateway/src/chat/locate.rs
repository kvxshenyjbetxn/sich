//! Finds the transcript file of an agent session. Pure file-system lookups, no state.
//!
//! - Claude Code: `<config>/projects/<cwd bucket>/<session id>.jsonl`, where the bucket is the
//!   cwd with every non-alphanumeric char replaced by `-`; fallback scan of all buckets.
//!   `<config>` is `CLAUDE_CONFIG_DIR` or `~/.claude`.
//! - Codex: `<codex home>/sessions/YYYY/MM/DD/rollout-<time>-<session id>.jsonl`;
//!   `<codex home>` is `CODEX_HOME` or `~/.codex`.

use std::path::{Path, PathBuf};

use sich_proto::AgentKind;

use super::{Format, Target};

/// How deep under `sessions/` a rollout may sit (`YYYY/MM/DD` = 3 levels).
const CODEX_DEPTH: usize = 4;

/// Where each agent keeps its session files.
#[derive(Debug, Clone, Default)]
pub struct Dirs {
    pub claude_config: Option<PathBuf>,
    pub codex_home: Option<PathBuf>,
}

impl Dirs {
    pub fn from_env() -> Self {
        Self {
            claude_config: dir_from(std::env::var_os("CLAUDE_CONFIG_DIR"), ".claude"),
            codex_home: dir_from(std::env::var_os("CODEX_HOME"), ".codex"),
        }
    }
}

fn dir_from(env: Option<std::ffi::OsString>, default: &str) -> Option<PathBuf> {
    match env.filter(|v| !v.is_empty()) {
        Some(dir) => Some(PathBuf::from(dir)),
        None => dirs::home_dir().map(|h| h.join(default)),
    }
}

/// Claude's project folder name for a working directory.
pub fn bucket_name(cwd: &str) -> String {
    cwd.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

/// Session ids are file-name parts: letters, digits, `-` and `_` only (never a path or `..`).
pub fn valid_session_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

pub fn claude_transcript(config: &Path, session_id: &str, cwd: Option<&Path>) -> Option<PathBuf> {
    if !valid_session_id(session_id) {
        return None;
    }
    let projects = config.join("projects");
    let file = format!("{session_id}.jsonl");
    if let Some(cwd) = cwd {
        let direct = projects.join(bucket_name(&cwd.to_string_lossy())).join(&file);
        if direct.is_file() {
            return Some(direct);
        }
    }
    std::fs::read_dir(&projects).ok()?.flatten().map(|e| e.path().join(&file)).find(|p| p.is_file())
}

fn is_rollout_of(name: &str, id: &str) -> bool {
    let Some(stem) = name.strip_suffix(".jsonl") else { return false };
    stem == id || stem.strip_suffix(id).is_some_and(|rest| rest.ends_with('-'))
}

pub fn codex_rollout(home: &Path, session_id: &str) -> Option<PathBuf> {
    if !valid_session_id(session_id) {
        return None;
    }
    fn walk(dir: &Path, depth: usize, id: &str) -> Option<PathBuf> {
        let mut dirs = Vec::new();
        for entry in std::fs::read_dir(dir).ok()?.flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                dirs.push(entry.path());
            } else if kind.is_file() && is_rollout_of(&entry.file_name().to_string_lossy(), id) {
                return Some(entry.path());
            }
        }
        if depth >= CODEX_DEPTH {
            return None;
        }
        // Newest day first: the session being watched is almost always a recent one.
        dirs.sort_by(|a, b| b.cmp(a));
        dirs.into_iter().find_map(|sub| walk(&sub, depth + 1, id))
    }
    walk(&home.join("sessions"), 0, session_id)
}

/// The chat target of a terminal whose agent is `kind` with `session_id`, running in `cwd`.
pub fn resolve(dirs: &Dirs, kind: Option<AgentKind>, session_id: Option<&str>, cwd: Option<&Path>) -> Target {
    let format = match kind {
        Some(AgentKind::Claude) => Format::Claude,
        Some(AgentKind::Codex) => Format::Codex,
        Some(_) => return Target::Unavailable("chat is available for Claude Code and Codex only".into()),
        None => return Target::Unavailable("no agent runs in this terminal".into()),
    };
    let Some(id) = session_id.filter(|s| !s.is_empty()) else { return Target::Pending };
    if !valid_session_id(id) {
        return Target::Unavailable("the agent reported an unusable session id".into());
    }
    let found = match format {
        Format::Claude => dirs.claude_config.as_deref().and_then(|c| claude_transcript(c, id, cwd)),
        Format::Codex => dirs.codex_home.as_deref().and_then(|h| codex_rollout(h, id)),
    };
    match found {
        Some(path) => Target::Ready { path, format },
        None => Target::Pending,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "{}\n").unwrap();
    }

    #[test]
    fn bucket_replaces_every_non_alphanumeric_char() {
        assert_eq!(bucket_name(r"E:\dev\program\sich"), "E--dev-program-sich");
        assert_eq!(bucket_name("/home/a b/p.q_r"), "-home-a-b-p-q-r");
        assert_eq!(bucket_name("ab1"), "ab1");
    }

    #[test]
    fn session_ids_are_file_name_safe() {
        assert!(valid_session_id("37916b56-2e9e-47ee-91ff-c57a988fca06"));
        assert!(valid_session_id("abc_DEF-1"));
        for bad in ["", "..", "a/b", "a\\b", "a b", "a.jsonl", &"x".repeat(129)] {
            assert!(!valid_session_id(bad), "{bad}");
        }
    }

    #[test]
    fn claude_uses_the_cwd_bucket_then_scans_other_buckets() {
        let cfg = tempfile::tempdir().unwrap();
        let direct = cfg.path().join("projects").join("E--dev-x").join("s1.jsonl");
        touch(&direct);
        touch(&cfg.path().join("projects").join("other").join("s2.jsonl"));
        let cwd = Path::new(r"E:\dev\x");
        assert_eq!(claude_transcript(cfg.path(), "s1", Some(cwd)), Some(direct));
        // Wrong or missing cwd: fallback scan.
        assert_eq!(claude_transcript(cfg.path(), "s2", Some(cwd)), Some(cfg.path().join("projects/other/s2.jsonl")));
        assert_eq!(claude_transcript(cfg.path(), "s2", None), Some(cfg.path().join("projects/other/s2.jsonl")));
        assert_eq!(claude_transcript(cfg.path(), "missing", None), None);
        assert_eq!(claude_transcript(&cfg.path().join("nope"), "s1", None), None);
    }

    #[test]
    fn claude_rejects_path_like_ids() {
        let cfg = tempfile::tempdir().unwrap();
        touch(&cfg.path().join("secret.jsonl"));
        assert_eq!(claude_transcript(cfg.path(), "../secret", None), None);
    }

    #[test]
    fn codex_finds_the_rollout_by_session_id_suffix() {
        let home = tempfile::tempdir().unwrap();
        let id = "01a1f602-4a3b-7ab0-8ed1-9bde6814a439";
        let want = home.path().join(format!("sessions/2026/11/19/rollout-2026-11-19T22-51-47-{id}.jsonl"));
        touch(&want);
        touch(&home.path().join("sessions/2026/11/20/rollout-2026-11-20T00-00-00-other-id.jsonl"));
        assert_eq!(codex_rollout(home.path(), id), Some(want));
        assert_eq!(codex_rollout(home.path(), "other"), None, "a suffix must follow a dash");
        assert_eq!(codex_rollout(home.path(), "nothing"), None);
        assert_eq!(codex_rollout(&home.path().join("none"), id), None);
    }

    #[test]
    fn resolve_covers_every_outcome() {
        let cfg = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let dirs = Dirs { claude_config: Some(cfg.path().into()), codex_home: Some(home.path().into()) };
        let file = cfg.path().join("projects/p/sid.jsonl");
        touch(&file);
        assert_eq!(
            resolve(&dirs, Some(AgentKind::Claude), Some("sid"), None),
            Target::Ready { path: file, format: Format::Claude }
        );
        assert_eq!(resolve(&dirs, Some(AgentKind::Claude), None, None), Target::Pending);
        assert_eq!(resolve(&dirs, Some(AgentKind::Claude), Some(""), None), Target::Pending);
        assert_eq!(resolve(&dirs, Some(AgentKind::Claude), Some("later"), None), Target::Pending);
        assert_eq!(resolve(&dirs, Some(AgentKind::Codex), Some("later"), None), Target::Pending);
        assert!(matches!(resolve(&dirs, Some(AgentKind::Claude), Some("../x"), None), Target::Unavailable(_)));
        assert!(matches!(resolve(&dirs, Some(AgentKind::Gemini), Some("sid"), None), Target::Unavailable(_)));
        assert!(matches!(resolve(&dirs, None, None, None), Target::Unavailable(_)));
        let rollout = home.path().join("sessions/2026/01/01/rollout-t-cid.jsonl");
        touch(&rollout);
        assert_eq!(
            resolve(&dirs, Some(AgentKind::Codex), Some("cid"), None),
            Target::Ready { path: rollout, format: Format::Codex }
        );
        let none = Dirs::default();
        assert_eq!(resolve(&none, Some(AgentKind::Claude), Some("sid"), None), Target::Pending);
    }

    #[test]
    fn dirs_honor_the_env_overrides() {
        assert_eq!(dir_from(Some("/x/cfg".into()), ".claude"), Some(PathBuf::from("/x/cfg")));
        let default = dir_from(Some("".into()), ".claude");
        assert_eq!(default, dirs::home_dir().map(|h| h.join(".claude")));
        assert_eq!(dir_from(None, ".codex"), dirs::home_dir().map(|h| h.join(".codex")));
    }
}
