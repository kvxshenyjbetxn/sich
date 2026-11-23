//! Claude context usage: parses the files written by the statusLine hook (`*.status.json`) and the
//! Claude Code mod (`*.mod.json`) into one merged [`CtxView`] per session, and matches terminals to
//! sessions. Pure logic + fail-soft loading; the UI lives in `context_panel`.
//!
//! The format is produced by Sich itself and may evolve: every field is optional, unknown fields
//! are ignored and a file that does not parse is skipped.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::Deserialize;

pub(crate) const DIR_NAME: &str = "claude-ctx";
const STATUS_SUFFIX: &str = ".status.json";
const MOD_SUFFIX: &str = ".mod.json";
/// Files older than this are deleted on startup.
pub(crate) const PRUNE_AGE: Duration = Duration::from_secs(7 * 24 * 3600);
/// Refuse to parse absurdly large files.
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;

// ---------------------------------------------------------------------------------------------
// File formats
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub(crate) struct StatusFile {
    pub session_id: Option<String>,
    pub term_id: Option<String>,
    pub socket: Option<String>,
    pub updated_ms: Option<u64>,
    pub model_name: Option<String>,
    pub model_id: Option<String>,
    pub effort: Option<String>,
    pub used_percentage: Option<f64>,
    pub context_window_size: Option<u64>,
    pub current: Option<CurrentUsage>,
    pub cost_usd: Option<f64>,
    pub duration_ms: Option<u64>,
    pub api_duration_ms: Option<u64>,
    pub lines_added: Option<u64>,
    pub lines_removed: Option<u64>,
    pub cache: Option<CacheInfo>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub(crate) struct CurrentUsage {
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub cache_creation: Option<u64>,
    pub cache_read: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub(crate) struct CacheInfo {
    pub warm: Option<bool>,
    pub ttl: Option<String>,
    pub hit_ratio: Option<f64>,
    pub expires_at: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub(crate) struct ModFile {
    #[serde(rename = "sessionId")]
    pub session_id: Option<String>,
    #[serde(rename = "termId")]
    pub term_id: Option<String>,
    pub socket: Option<String>,
    #[serde(rename = "updatedAt")]
    pub updated_at: Option<u64>,
    pub snapshot: Option<ModSnapshot>,
    #[serde(rename = "agentRuns")]
    pub agent_runs: Vec<AgentRun>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub(crate) struct ModSnapshot {
    pub model: Option<String>,
    pub total: Option<u64>,
    pub max: Option<u64>,
    pub percent: Option<f64>,
    pub rows: Vec<ModRow>,
    pub mcp: Vec<McpItem>,
    pub memory: Vec<MemoryItem>,
    pub skills: Option<SkillsInfo>,
    pub at: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub(crate) struct ModRow {
    pub name: String,
    pub tokens: u64,
    pub color: Option<String>,
    pub kind: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub(crate) struct McpItem {
    pub name: String,
    pub detail: Option<String>,
    pub tokens: u64,
    pub loaded: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub(crate) struct MemoryItem {
    pub name: String,
    pub detail: Option<String>,
    pub tokens: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub(crate) struct SkillsInfo {
    pub count: Option<u64>,
    pub tokens: Option<u64>,
    pub items: Vec<SkillItem>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub(crate) struct SkillItem {
    pub name: String,
    pub tokens: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub(crate) struct AgentRun {
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub description: Option<String>,
    pub status: Option<String>,
    pub steps: Option<u64>,
    #[serde(rename = "lastCtx")]
    pub last_ctx: Option<u64>,
    pub total: Option<u64>,
    #[serde(rename = "startedAt")]
    pub started_at: Option<u64>,
    pub model: Option<String>,
}

pub(crate) fn parse_status(text: &str) -> Option<StatusFile> {
    serde_json::from_str(text).ok()
}

pub(crate) fn parse_mod(text: &str) -> Option<ModFile> {
    serde_json::from_str(text).ok()
}

// ---------------------------------------------------------------------------------------------
// Merged view
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CatKind {
    Used,
    Free,
    Buffer,
    Deferred,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Category {
    pub name: String,
    pub tokens: u64,
    /// Claude Code theme color name (see `context_panel::palette_slot`).
    pub color: Option<String>,
    pub kind: CatKind,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct CtxView {
    pub session_id: String,
    pub percent: Option<f64>,
    pub used: Option<u64>,
    pub max: Option<u64>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub updated_ms: u64,
    pub categories: Vec<Category>,
    /// True when the categories come from the mod (full breakdown), not from the status fallback.
    pub detailed: bool,
    pub mcp: Vec<McpItem>,
    pub memory: Vec<MemoryItem>,
    pub skills: Option<SkillsInfo>,
    pub runs: Vec<AgentRun>,
    pub cost_usd: Option<f64>,
    pub duration_ms: Option<u64>,
    pub api_duration_ms: Option<u64>,
    pub lines_added: Option<u64>,
    pub lines_removed: Option<u64>,
    pub cache: Option<CacheInfo>,
}

fn non_blank(s: &Option<String>) -> Option<String> {
    s.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(String::from)
}

fn kind_of(kind: Option<&str>) -> CatKind {
    match kind {
        Some("free") => CatKind::Free,
        Some("buffer") => CatKind::Buffer,
        Some("deferred") => CatKind::Deferred,
        _ => CatKind::Used,
    }
}

/// Used tokens by the status line: what is currently in the context window.
fn status_used(current: &CurrentUsage) -> Option<u64> {
    let parts = [current.input, current.cache_creation, current.cache_read];
    parts.iter().any(Option::is_some).then(|| parts.iter().flatten().sum())
}

/// Categories when the mod gave no rows: a coarse split from the status line's counters.
pub(crate) fn fallback_categories(current: Option<&CurrentUsage>, max: Option<u64>) -> Vec<Category> {
    let Some(current) = current else { return Vec::new() };
    let mut out = Vec::new();
    for (name, tokens, color) in [
        ("Cache read", current.cache_read, "suggestion"),
        ("Cache write", current.cache_creation, "permission"),
        ("Input", current.input, "claude"),
    ] {
        out.push(Category { name: name.into(), tokens: tokens.unwrap_or(0), color: Some(color.into()), kind: CatKind::Used });
    }
    let used = status_used(current).unwrap_or(0);
    if let Some(max) = max.filter(|&max| max > used) {
        out.push(Category { name: "Free".into(), tokens: max - used, color: None, kind: CatKind::Free });
    }
    out
}

/// Merges the two files of one session. `None` when there is nothing to show.
pub(crate) fn merge(session_id: &str, status: Option<&StatusFile>, modf: Option<&ModFile>) -> Option<CtxView> {
    if status.is_none() && modf.is_none() {
        return None;
    }
    let snap = modf.and_then(|m| m.snapshot.as_ref());
    let used = status.and_then(|s| s.current.as_ref()).and_then(status_used).or(snap.and_then(|s| s.total));
    let max = status.and_then(|s| s.context_window_size).filter(|&m| m > 0).or(snap.and_then(|s| s.max));
    let percent = status.and_then(|s| s.used_percentage).or(snap.and_then(|s| s.percent));
    let model = status
        .and_then(|s| non_blank(&s.model_name).or_else(|| non_blank(&s.model_id)))
        .or_else(|| snap.and_then(|s| non_blank(&s.model)));
    let updated_ms = [status.and_then(|s| s.updated_ms), modf.and_then(|m| m.updated_at), snap.and_then(|s| s.at)]
        .into_iter()
        .flatten()
        .max()
        .unwrap_or(0);

    let rows: Vec<Category> = snap
        .map(|s| {
            s.rows
                .iter()
                .map(|r| Category { name: r.name.clone(), tokens: r.tokens, color: r.color.clone(), kind: kind_of(r.kind.as_deref()) })
                .collect()
        })
        .unwrap_or_default();
    let detailed = !rows.is_empty();
    let categories = if detailed {
        rows
    } else {
        fallback_categories(status.and_then(|s| s.current.as_ref()), max)
    };
    Some(CtxView {
        session_id: session_id.to_string(),
        percent,
        used,
        max,
        model,
        effort: status.and_then(|s| non_blank(&s.effort)),
        updated_ms,
        categories,
        detailed,
        mcp: snap.map(|s| s.mcp.clone()).unwrap_or_default(),
        memory: snap.map(|s| s.memory.clone()).unwrap_or_default(),
        skills: snap.and_then(|s| s.skills.clone()),
        runs: modf.map(|m| m.agent_runs.clone()).unwrap_or_default(),
        cost_usd: status.and_then(|s| s.cost_usd),
        duration_ms: status.and_then(|s| s.duration_ms),
        api_duration_ms: status.and_then(|s| s.api_duration_ms),
        lines_added: status.and_then(|s| s.lines_added),
        lines_removed: status.and_then(|s| s.lines_removed),
        cache: status.and_then(|s| s.cache.clone()),
    })
}

// ---------------------------------------------------------------------------------------------
// Small pure helpers
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tone {
    Muted,
    Good,
    Warning,
    Danger,
}

pub(crate) fn tone(percent: Option<f64>) -> Tone {
    match percent {
        Some(p) if p >= 80. => Tone::Danger,
        Some(p) if p >= 60. => Tone::Warning,
        Some(_) => Tone::Good,
        None => Tone::Muted,
    }
}

fn trim_zeros(mut s: String) -> String {
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    s
}

/// 950 -> "950", 55674 -> "55.7k", 1000000 -> "1M", 1250000 -> "1.25M".
pub(crate) fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        return format!("{}M", trim_zeros(format!("{:.2}", n as f64 / 1e6)));
    }
    if n >= 1000 {
        let tenths = (n as f64 / 100.).round() as u64; // tokens / 100
        if tenths >= 10_000 {
            return "1M".into();
        }
        return format!("{}k", trim_zeros(format!("{:.1}", tenths as f64 / 10.)));
    }
    n.to_string()
}

/// Percent of `max` with one decimal ("6.4%"), "0%" without a max.
pub(crate) fn percent_of(tokens: u64, max: Option<u64>) -> String {
    match max.filter(|&m| m > 0) {
        Some(max) => format!("{}%", trim_zeros(format!("{:.1}", tokens as f64 * 100. / max as f64))),
        None => "0%".into(),
    }
}

/// Whole percent for the bar/header ("6%").
pub(crate) fn percent_label(percent: f64) -> String {
    format!("{}%", percent.round().clamp(0., 999.) as u32)
}

pub(crate) fn age_secs(updated_ms: u64, now_ms: u64) -> u64 {
    now_ms.saturating_sub(updated_ms) / 1000
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// Session ids become file names the same way the writers name them (outside `[A-Za-z0-9_-]` is dropped).
pub(crate) fn sanitize_id(id: &str) -> String {
    sich_hooks::claude_ctx::sanitize_session_id(id).unwrap_or_default()
}

// ---------------------------------------------------------------------------------------------
// Terminal -> session matching
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Candidate {
    /// Sanitized session id (the file name stem).
    pub session: String,
    pub term_id: Option<String>,
    pub socket: Option<String>,
    pub updated_ms: u64,
}

/// A Claude terminal as the GUI knows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Target {
    /// GUI terminal key.
    pub key: u64,
    pub session_id: Option<String>,
    /// Raw terminal id on its daemon (`SICH_TERM_ID`).
    pub raw_id: String,
    pub socket: String,
    pub title: String,
    pub worktree: Option<PathBuf>,
}

/// The session of a terminal: its own session id when files exist, else the newest candidate
/// whose terminal id and socket both match.
pub(crate) fn match_session(
    session_id: Option<&str>,
    raw_id: &str,
    socket: &str,
    candidates: &[Candidate],
) -> Option<String> {
    if let Some(id) = session_id.map(sanitize_id).filter(|id| !id.is_empty()) {
        if candidates.iter().any(|c| c.session == id) {
            return Some(id);
        }
    }
    candidates
        .iter()
        .filter(|c| c.term_id.as_deref() == Some(raw_id) && c.socket.as_deref() == Some(socket))
        .max_by_key(|c| c.updated_ms)
        .map(|c| c.session.clone())
}

/// The terminal to show for "Auto": the focused one when it is a Claude terminal of the
/// workspace, else the workspace's active one, else the first.
pub(crate) fn auto_target(ids: &[u64], focused: Option<u64>, active: Option<u64>) -> Option<u64> {
    let has = |id: &u64| ids.contains(id);
    focused.filter(has).or(active.filter(has)).or(ids.first().copied())
}

// ---------------------------------------------------------------------------------------------
// Prune
// ---------------------------------------------------------------------------------------------

/// Files (name, modified) that are older than `max_age` at `now`.
pub(crate) fn select_prune(files: &[(PathBuf, SystemTime)], now: SystemTime, max_age: Duration) -> Vec<PathBuf> {
    files
        .iter()
        .filter(|(_, modified)| now.duration_since(*modified).is_ok_and(|age| age > max_age))
        .map(|(path, _)| path.clone())
        .collect()
}

pub(crate) fn prune_dir(dir: &Path, now: SystemTime, max_age: Duration) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    let files: Vec<(PathBuf, SystemTime)> = entries
        .flatten()
        .filter(|e| split_name(&e.file_name().to_string_lossy()).is_some())
        .filter_map(|e| Some((e.path(), e.metadata().ok()?.modified().ok()?)))
        .collect();
    select_prune(&files, now, max_age).into_iter().filter(|p| std::fs::remove_file(p).is_ok()).count()
}

pub(crate) fn data_dir() -> Option<PathBuf> {
    sich_hooks::usage::config_file(DIR_NAME)
}

// ---------------------------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Part {
    Status,
    Mod,
}

/// `abc.status.json` -> ("abc", Status).
fn split_name(name: &str) -> Option<(&str, Part)> {
    if let Some(stem) = name.strip_suffix(STATUS_SUFFIX) {
        return (!stem.is_empty()).then_some((stem, Part::Status));
    }
    name.strip_suffix(MOD_SUFFIX).filter(|s| !s.is_empty()).map(|stem| (stem, Part::Mod))
}

#[derive(Clone)]
enum Parsed {
    Status(Box<StatusFile>),
    Mod(Box<ModFile>),
    Bad,
}

/// Incremental reader: remembers parsed files by modification time and re-reads only changes.
#[derive(Default)]
pub(crate) struct Loader {
    cache: HashMap<PathBuf, (SystemTime, Parsed)>,
}

impl Loader {
    /// Views for the given terminals. Reads only the files that are needed: those of the
    /// terminals' own sessions, plus (only for terminals without them) every file for the
    /// term id + socket fallback.
    pub(crate) fn load(&mut self, dir: &Path, targets: &[Target]) -> HashMap<u64, CtxView> {
        let mut listing: HashMap<String, [Option<(PathBuf, SystemTime)>; 2]> = HashMap::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let Some((stem, part)) = split_name(&name) else { continue };
                let Some(modified) = entry.metadata().ok().filter(|m| m.len() <= MAX_FILE_BYTES).and_then(|m| m.modified().ok()) else {
                    continue;
                };
                listing.entry(stem.to_string()).or_default()[part as usize] = Some((entry.path(), modified));
            }
        }
        self.cache.retain(|path, _| {
            path.file_name()
                .and_then(|n| n.to_str())
                .and_then(split_name)
                .is_some_and(|(stem, part)| listing.get(stem).is_some_and(|files| files[part as usize].is_some()))
        });

        let mut resolved: Vec<(&Target, String)> = Vec::new();
        let mut need_scan = Vec::new();
        for target in targets {
            let own = target.session_id.as_deref().map(sanitize_id).filter(|id| listing.contains_key(id));
            match own {
                Some(id) => resolved.push((target, id)),
                None => need_scan.push(target),
            }
        }
        if !need_scan.is_empty() {
            let stems: Vec<String> = listing.keys().cloned().collect();
            let candidates: Vec<Candidate> = stems
                .iter()
                .map(|stem| {
                    let (status, modf) = self.read_session(&listing, stem);
                    Candidate {
                        session: stem.clone(),
                        term_id: status.as_ref().and_then(|s| s.term_id.clone()).or(modf.as_ref().and_then(|m| m.term_id.clone())),
                        socket: status.as_ref().and_then(|s| s.socket.clone()).or(modf.as_ref().and_then(|m| m.socket.clone())),
                        updated_ms: status
                            .as_ref()
                            .and_then(|s| s.updated_ms)
                            .into_iter()
                            .chain(modf.as_ref().and_then(|m| m.updated_at))
                            .max()
                            .unwrap_or(0),
                    }
                })
                .collect();
            for target in need_scan {
                if let Some(id) = match_session(None, &target.raw_id, &target.socket, &candidates) {
                    resolved.push((target, id));
                }
            }
        }

        let mut views: HashMap<String, Option<CtxView>> = HashMap::new();
        let mut out = HashMap::new();
        for (target, stem) in resolved {
            let view = views.entry(stem.clone()).or_insert_with(|| {
                let (status, modf) = self.read_session(&listing, &stem);
                merge(&stem, status.as_ref(), modf.as_ref())
            });
            if let Some(view) = view {
                out.insert(target.key, view.clone());
            }
        }
        out
    }

    fn read_session(
        &mut self,
        listing: &HashMap<String, [Option<(PathBuf, SystemTime)>; 2]>,
        stem: &str,
    ) -> (Option<StatusFile>, Option<ModFile>) {
        let Some(files) = listing.get(stem) else { return (None, None) };
        let status = files[Part::Status as usize].as_ref().and_then(|(path, modified)| {
            match self.parsed(path, *modified, Part::Status) {
                Parsed::Status(s) => Some(*s),
                _ => None,
            }
        });
        let modf = files[Part::Mod as usize].as_ref().and_then(|(path, modified)| {
            match self.parsed(path, *modified, Part::Mod) {
                Parsed::Mod(m) => Some(*m),
                _ => None,
            }
        });
        (status, modf)
    }

    fn parsed(&mut self, path: &Path, modified: SystemTime, part: Part) -> Parsed {
        if let Some((at, parsed)) = self.cache.get(path) {
            if *at == modified {
                return parsed.clone();
            }
        }
        let parsed = match std::fs::read_to_string(path) {
            Ok(text) => match part {
                Part::Status => parse_status(&text).map(|s| Parsed::Status(Box::new(s))),
                Part::Mod => parse_mod(&text).map(|m| Parsed::Mod(Box::new(m))),
            }
            .unwrap_or(Parsed::Bad),
            // A file caught mid-write or just removed: do not cache, retry next poll.
            Err(_) => return Parsed::Bad,
        };
        self.cache.insert(path.to_path_buf(), (modified, parsed.clone()));
        parsed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATUS: &str = r#"{ "v":1, "session_id":"abc", "term_id":"3", "socket":"sock", "updated_ms":2000,
        "model_id":"claude-opus-5-5", "model_name":"Opus 5.5", "effort":"high", "used_percentage":6.0,
        "context_window_size":1000000, "current":{"input":4,"output":902,"cache_creation":2803,"cache_read":55613},
        "cost_usd":0.338, "duration_ms":209036, "api_duration_ms":110845, "lines_added":1, "lines_removed":2,
        "cache":{"warm":true,"ttl":"1h","hit_ratio":0.876,"expires_at":1795400300}, "future":{"x":1} }"#;

    const MOD: &str = r#"{ "version":1, "sessionId":"abc", "termId":"3", "socket":"sock", "updatedAt":3000, "ended":false,
        "snapshot":{ "model":"claude-opus-5-5", "total":55674, "max":1000000, "percent":6,
          "rows":[{"name":"System prompt","tokens":4395,"color":"promptBorder","kind":"used"},
                  {"name":"Free space","tokens":900000,"color":"promptBorder","kind":"free"}],
          "mcp":[{"name":"computer","detail":"chrome","tokens":2080,"loaded":false}],
          "memory":[{"name":"CLAUDE.md","detail":"C:\\x\\CLAUDE.md","tokens":1349}],
          "skills":{"count":33,"tokens":6613,"items":[{"name":"dataviz","tokens":482}]}, "at":2500 },
        "agentRuns":[{"id":"a","type":"scout","description":"Scout","status":"done","steps":10,"lastCtx":75006,"total":556480,"startedAt":5,"model":"claude-haiku-5-5"}],
        "usedSkills":{} }"#;

    #[test]
    fn parses_full_status_and_mod() {
        let s = parse_status(STATUS).unwrap();
        assert_eq!(s.model_name.as_deref(), Some("Opus 5.5"));
        assert_eq!(s.current.unwrap().cache_read, Some(55613));
        assert_eq!(s.cache.unwrap().warm, Some(true));
        let m = parse_mod(MOD).unwrap();
        let snap = m.snapshot.unwrap();
        assert_eq!(snap.rows.len(), 2);
        assert!(!snap.mcp[0].loaded);
        assert_eq!(snap.skills.unwrap().items[0].name, "dataviz");
        assert_eq!(m.agent_runs[0].kind.as_deref(), Some("scout"));
        assert_eq!(m.agent_runs[0].last_ctx, Some(75006));
    }

    #[test]
    fn parses_partial_and_rejects_garbage() {
        assert_eq!(parse_status("{}").unwrap(), StatusFile::default());
        assert_eq!(parse_mod(r#"{"snapshot":{}}"#).unwrap().snapshot.unwrap().rows.len(), 0);
        for bad in ["", "nope", "[1,2]", r#"{"used_percentage":"x"}"#, "{\"a\":"] {
            assert!(parse_status(bad).is_none(), "{bad}");
        }
        assert!(parse_mod("garbage").is_none());
    }

    #[test]
    fn merge_prefers_status_and_falls_back_to_mod() {
        let s = parse_status(STATUS).unwrap();
        let m = parse_mod(MOD).unwrap();
        let both = merge("abc", Some(&s), Some(&m)).unwrap();
        assert_eq!(both.percent, Some(6.0));
        assert_eq!(both.used, Some(4 + 2803 + 55613));
        assert_eq!(both.max, Some(1_000_000));
        assert_eq!(both.model.as_deref(), Some("Opus 5.5"));
        assert_eq!(both.effort.as_deref(), Some("high"));
        assert_eq!(both.updated_ms, 3000);
        assert!(both.detailed);
        assert_eq!(both.categories.len(), 2);
        assert_eq!(both.categories[1].kind, CatKind::Free);
        assert_eq!(both.runs.len(), 1);
        assert_eq!(both.lines_added, Some(1));

        let only_mod = merge("abc", None, Some(&m)).unwrap();
        assert_eq!(only_mod.percent, Some(6.0));
        assert_eq!(only_mod.used, Some(55674));
        assert_eq!(only_mod.max, Some(1_000_000));
        assert_eq!(only_mod.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(only_mod.cost_usd, None);

        assert!(merge("abc", None, None).is_none());
    }

    #[test]
    fn status_only_uses_fallback_categories() {
        let s = parse_status(STATUS).unwrap();
        let v = merge("abc", Some(&s), None).unwrap();
        assert!(!v.detailed);
        let names: Vec<_> = v.categories.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["Cache read", "Cache write", "Input", "Free"]);
        assert_eq!(v.categories[0].tokens, 55613);
        assert_eq!(v.categories[3].tokens, 1_000_000 - 58420);
        assert!(fallback_categories(None, Some(10)).is_empty());
        // No max: no Free row.
        let cur = CurrentUsage { input: Some(5), ..Default::default() };
        assert_eq!(fallback_categories(Some(&cur), None).len(), 3);
        // Overfull context: no negative free space.
        assert_eq!(fallback_categories(Some(&cur), Some(3)).len(), 3);
    }

    #[test]
    fn tone_thresholds() {
        assert_eq!(tone(None), Tone::Muted);
        assert_eq!(tone(Some(0.)), Tone::Good);
        assert_eq!(tone(Some(59.)), Tone::Good);
        assert_eq!(tone(Some(59.9)), Tone::Good);
        assert_eq!(tone(Some(60.)), Tone::Warning);
        assert_eq!(tone(Some(79.)), Tone::Warning);
        assert_eq!(tone(Some(80.)), Tone::Danger);
        assert_eq!(tone(Some(100.)), Tone::Danger);
    }

    #[test]
    fn format_tokens_rule() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(950), "950");
        assert_eq!(format_tokens(1000), "1k");
        assert_eq!(format_tokens(55674), "55.7k");
        assert_eq!(format_tokens(200_000), "200k");
        assert_eq!(format_tokens(999_990), "1M");
        assert_eq!(format_tokens(1_000_000), "1M");
        assert_eq!(format_tokens(1_250_000), "1.25M");
        assert_eq!(format_tokens(1_500_000), "1.5M");
    }

    #[test]
    fn percent_helpers() {
        assert_eq!(percent_of(55674, Some(1_000_000)), "5.6%");
        assert_eq!(percent_of(10, None), "0%");
        assert_eq!(percent_of(500_000, Some(1_000_000)), "50%");
        assert_eq!(percent_label(5.6), "6%");
        assert_eq!(age_secs(1000, 6000), 5);
        assert_eq!(age_secs(9000, 1000), 0);
        assert_eq!(sanitize_id("a/b c-1_x"), "abc-1_x");
    }

    fn cand(session: &str, term: &str, socket: &str, updated: u64) -> Candidate {
        Candidate { session: session.into(), term_id: Some(term.into()), socket: Some(socket.into()), updated_ms: updated }
    }

    #[test]
    fn matching_prefers_session_id_then_newest_term_and_socket() {
        let cands = [cand("old", "3", "s1", 10), cand("new", "3", "s1", 20), cand("other", "3", "s2", 99), cand("x", "4", "s1", 99)];
        assert_eq!(match_session(Some("old"), "3", "s1", &cands).as_deref(), Some("old"));
        assert_eq!(match_session(None, "3", "s1", &cands).as_deref(), Some("new"));
        // Unknown session id falls back to term id + socket.
        assert_eq!(match_session(Some("gone"), "3", "s1", &cands).as_deref(), Some("new"));
        // Other socket is never matched by term id.
        assert_eq!(match_session(None, "3", "s3", &cands), None);
        assert_eq!(match_session(None, "9", "s1", &cands), None);
        assert_eq!(match_session(None, "3", "s1", &[]), None);
    }

    #[test]
    fn auto_target_prefers_focused_then_active_then_first() {
        assert_eq!(auto_target(&[1, 2, 3], Some(2), Some(3)), Some(2));
        assert_eq!(auto_target(&[1, 2, 3], Some(9), Some(3)), Some(3));
        assert_eq!(auto_target(&[1, 2, 3], None, None), Some(1));
        assert_eq!(auto_target(&[], Some(1), Some(1)), None);
    }

    #[test]
    fn prune_selects_only_old_files() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000_000);
        let day = Duration::from_secs(86_400);
        let files = [
            (PathBuf::from("old"), now - day * 8),
            (PathBuf::from("edge"), now - day * 7),
            (PathBuf::from("new"), now - day),
            (PathBuf::from("future"), now + day),
        ];
        assert_eq!(select_prune(&files, now, PRUNE_AGE), [PathBuf::from("old")]);
    }

    #[test]
    fn prune_dir_removes_only_ours_and_old() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.status.json"), "{}").unwrap();
        std::fs::write(dir.path().join("keep.txt"), "x").unwrap();
        let later = SystemTime::now() + Duration::from_secs(8 * 86_400);
        assert_eq!(prune_dir(dir.path(), SystemTime::now(), PRUNE_AGE), 0);
        assert_eq!(prune_dir(dir.path(), later, PRUNE_AGE), 1);
        assert!(dir.path().join("keep.txt").exists());
        assert!(!dir.path().join("a.status.json").exists());
        assert_eq!(prune_dir(&dir.path().join("missing"), later, PRUNE_AGE), 0);
    }

    fn target(key: u64, session: Option<&str>, raw: &str, socket: &str) -> Target {
        Target { key, session_id: session.map(Into::into), raw_id: raw.into(), socket: socket.into(), title: String::new(), worktree: None }
    }

    #[test]
    fn loader_reads_by_session_and_fallback_and_follows_changes() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        std::fs::write(p.join("abc.status.json"), STATUS).unwrap();
        std::fs::write(p.join("abc.mod.json"), MOD).unwrap();
        std::fs::write(p.join("zzz.status.json"), "garbage").unwrap();
        std::fs::write(p.join("readme.txt"), "ignored").unwrap();
        let mut loader = Loader::default();
        let targets = [target(1, Some("abc"), "9", "other"), target(2, None, "3", "sock"), target(3, None, "3", "nope"), target(4, Some("zzz"), "8", "sock")];
        let out = loader.load(p, &targets);
        assert_eq!(out.len(), 2);
        assert_eq!(out[&1].session_id, "abc");
        assert!(out[&1].detailed);
        assert_eq!(out[&2].session_id, "abc");

        // Update the status file: the cache must notice the new modification time.
        let changed = STATUS.replace("\"used_percentage\":6.0", "\"used_percentage\":42.0");
        std::fs::write(p.join("abc.status.json"), changed).unwrap();
        let later = SystemTime::now() + Duration::from_secs(5);
        std::fs::File::options().write(true).open(p.join("abc.status.json")).unwrap().set_modified(later).unwrap();
        assert_eq!(loader.load(p, &targets)[&1].percent, Some(42.0));

        // Removing the files drops the view; a missing dir is fine.
        std::fs::remove_file(p.join("abc.status.json")).unwrap();
        std::fs::remove_file(p.join("abc.mod.json")).unwrap();
        assert!(loader.load(p, &targets).is_empty());
        assert!(loader.load(&p.join("none"), &targets).is_empty());
    }

    #[test]
    fn loader_status_only_session() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("abc.status.json"), STATUS).unwrap();
        let out = Loader::default().load(dir.path(), &[target(1, Some("abc"), "3", "sock")]);
        assert!(!out[&1].detailed);
    }
}
