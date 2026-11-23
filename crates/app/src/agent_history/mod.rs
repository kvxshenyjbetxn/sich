//! Agent session history: finds the session files the official agent CLIs keep on the user's disk
//! (Claude Code and Codex) and reads a small summary of each. It only READS those files, never
//! writes them, never touches credentials, and fails soft on anything unexpected.
//!
//! Everything here is plain `std` + `serde_json`: the base directories are parameters, there is no
//! GPUI. The panel (`agents_panel`) runs `Scanner::scan` on a background thread.
//!
//! Bounds: discovery uses metadata only; just the newest `limit` files are parsed, and a file is
//! read as its first and last 256 KiB at most (the whole file only when it is that small). An
//! unchanged file (same mtime and length) is never parsed twice.

mod claude;
mod codex;
mod filter;
mod text;

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use sich_proto::AgentKind;

pub(crate) use filter::{Filter, LiveSession, Scope, filter_sessions, is_inside, live_sessions};
#[cfg(test)]
pub(crate) use filter::{in_scope, matches_query};

use crate::handoff::TranscriptDirs;

/// Session files parsed per scan; "Show more" raises it by this much.
pub(crate) const DEFAULT_LIMIT: usize = 300;
/// How much of the start and of the end of a session file is read.
pub(crate) const HEAD_BYTES: u64 = 256 * 1024;
pub(crate) const TAIL_BYTES: u64 = 256 * 1024;
/// How much of the end of a session file the expanded row reads.
pub(crate) const TURNS_TAIL_BYTES: u64 = 1024 * 1024;
const TITLE_MAX: usize = 120;
const TEXT_MAX: usize = 400;
const TURN_MAX: usize = 600;
/// Codex keeps `sessions/YYYY/MM/DD/rollout-*.jsonl`; same depth bound as the transcript lookup.
const CODEX_DEPTH: usize = 4;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SessionMeta {
    pub kind: AgentKind,
    /// The id `claude --resume` / `codex resume` takes.
    pub id: String,
    pub path: PathBuf,
    /// Where the session started (needed to resume it).
    pub cwd: Option<PathBuf>,
    /// Whether `cwd` is a directory right now (checked at scan time).
    pub cwd_exists: bool,
    pub branch: Option<String>,
    /// Custom title > AI title > the first prompt; single line.
    pub title: Option<String>,
    pub first_prompt: String,
    pub last_prompt: String,
    pub last_assistant: Option<String>,
    pub model: Option<String>,
    pub created_ms: Option<u64>,
    /// File modification time.
    pub updated_ms: u64,
}

impl SessionMeta {
    /// What the row shows as its title.
    pub(crate) fn display_title(&self) -> &str {
        self.title.as_deref().filter(|t| !t.is_empty()).unwrap_or(&self.first_prompt)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Role {
    User,
    Assistant,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Turn {
    pub role: Role,
    pub text: String,
}

/// What a parser found in the lines it saw.
#[derive(Default, Debug)]
struct Parsed {
    id: Option<String>,
    cwd: Option<String>,
    branch: Option<String>,
    custom_title: Option<String>,
    ai_title: Option<String>,
    first_prompt: Option<String>,
    last_prompt: Option<String>,
    last_assistant: Option<String>,
    model: Option<String>,
    created_ms: Option<u64>,
    /// A Codex sub-agent thread, not a session of the user.
    subagent: bool,
}

// ---------------------------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------------------------

fn lines_of(bytes: &[u8], drop_first: bool, drop_last: bool) -> Vec<String> {
    let mut parts: Vec<&[u8]> = bytes.split(|&b| b == b'\n').collect();
    if drop_last {
        parts.pop();
    }
    if drop_first && !parts.is_empty() {
        parts.remove(0);
    }
    parts
        .into_iter()
        .map(|part| String::from_utf8_lossy(part).trim().to_owned())
        .filter(|line| !line.is_empty())
        .collect()
}

fn read_range(file: &mut File, start: u64, max: u64) -> std::io::Result<Vec<u8>> {
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(max).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// The complete lines of the first `HEAD_BYTES` and the last `TAIL_BYTES` of the file, in file
/// order (the whole file when it is no bigger than both). A cut line at either boundary is dropped.
fn head_and_tail_lines(path: &Path, len: u64) -> std::io::Result<Vec<String>> {
    let mut file = File::open(path)?;
    if len <= HEAD_BYTES + TAIL_BYTES {
        return Ok(lines_of(&read_range(&mut file, 0, HEAD_BYTES + TAIL_BYTES)?, false, false));
    }
    let mut lines = lines_of(&read_range(&mut file, 0, HEAD_BYTES)?, false, true);
    lines.extend(lines_of(&read_range(&mut file, len - TAIL_BYTES, TAIL_BYTES)?, true, false));
    Ok(lines)
}

/// The complete lines of the last `max` bytes of the file.
fn tail_lines(path: &Path, max: u64) -> std::io::Result<Vec<String>> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(max);
    Ok(lines_of(&read_range(&mut file, start, max)?, start > 0, false))
}

// ---------------------------------------------------------------------------------------------
// Parsing one file
// ---------------------------------------------------------------------------------------------

fn to_meta(kind: AgentKind, path: &Path, updated_ms: u64, parsed: Parsed) -> Option<SessionMeta> {
    if parsed.subagent {
        return None;
    }
    let file_name = path.file_name()?.to_string_lossy().into_owned();
    // Claude names the file after the session; a resumed session's records can still carry the old id.
    let id = match kind {
        AgentKind::Claude => path.file_stem().map(|s| s.to_string_lossy().into_owned()),
        _ => parsed.id.or_else(|| codex::id_from_file_name(&file_name)),
    }
    .filter(|id| !id.is_empty())?;
    let first_prompt = parsed.first_prompt.or_else(|| parsed.last_prompt.clone())?;
    let last_prompt = parsed.last_prompt.unwrap_or_else(|| first_prompt.clone());
    let title = parsed
        .custom_title
        .or(parsed.ai_title)
        .or_else(|| Some(text::one_line(&first_prompt, TITLE_MAX)))
        .filter(|t| !t.is_empty());
    Some(SessionMeta {
        kind,
        id,
        path: path.to_path_buf(),
        cwd: parsed.cwd.filter(|c| !c.trim().is_empty()).map(PathBuf::from),
        cwd_exists: false,
        branch: parsed.branch,
        title,
        first_prompt,
        last_prompt,
        last_assistant: parsed.last_assistant,
        model: parsed.model,
        created_ms: parsed.created_ms,
        updated_ms,
    })
}

/// Parses already-read lines (head and tail of one file). `None` hides the session.
fn parse_lines(kind: AgentKind, path: &Path, updated_ms: u64, lines: &[String]) -> Option<SessionMeta> {
    let parsed = match kind {
        AgentKind::Claude => claude::parse(lines.iter().map(String::as_str)),
        _ => codex::parse(lines.iter().map(String::as_str)),
    };
    to_meta(kind, path, updated_ms, parsed)
}

/// Reads and parses one file; `None` when it is unreadable or has no user prompt.
pub(crate) fn parse_file(kind: AgentKind, path: &Path, updated_ms: u64, len: u64) -> Option<SessionMeta> {
    let lines = head_and_tail_lines(path, len).ok()?;
    parse_lines(kind, path, updated_ms, &lines)
}

// ---------------------------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileEntry {
    pub kind: AgentKind,
    pub path: PathBuf,
    pub mtime_ms: u64,
    pub len: u64,
}

fn entry_of(kind: AgentKind, path: PathBuf, meta: &std::fs::Metadata) -> FileEntry {
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as u64);
    FileEntry { kind, path, mtime_ms, len: meta.len() }
}

fn is_missing(err: &std::io::Error) -> bool {
    matches!(err.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory)
}

/// `<config dir>/projects/*/*.jsonl`: only direct children of each project directory.
pub(crate) fn discover_claude(config_dir: &Path) -> (Vec<FileEntry>, Option<String>) {
    let projects = config_dir.join("projects");
    let reader = match std::fs::read_dir(&projects) {
        Ok(reader) => reader,
        Err(err) if is_missing(&err) => return (Vec::new(), None),
        Err(err) => return (Vec::new(), Some(format!("{}: {err}", projects.display()))),
    };
    let mut files = Vec::new();
    for project in reader.flatten() {
        let Ok(inner) = std::fs::read_dir(project.path()) else { continue };
        for entry in inner.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "jsonl") {
                continue;
            }
            if let Ok(meta) = entry.metadata()
                && meta.is_file()
            {
                files.push(entry_of(AgentKind::Claude, path, &meta));
            }
        }
    }
    (files, None)
}

/// `<codex home>/sessions/**/rollout-*.jsonl`, at most `CODEX_DEPTH` directories deep.
pub(crate) fn discover_codex(codex_home: &Path) -> (Vec<FileEntry>, Option<String>) {
    fn walk(dir: &Path, depth: usize, out: &mut Vec<FileEntry>) -> Option<std::io::Error> {
        let reader = match std::fs::read_dir(dir) {
            Ok(reader) => reader,
            Err(err) => return (!is_missing(&err)).then_some(err),
        };
        for entry in reader.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            let path = entry.path();
            if meta.is_dir() {
                if depth < CODEX_DEPTH {
                    walk(&path, depth + 1, out);
                }
            } else if meta.is_file() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with("rollout-") && name.ends_with(".jsonl") {
                    out.push(entry_of(AgentKind::Codex, path, &meta));
                }
            }
        }
        None
    }
    let root = codex_home.join("sessions");
    let mut files = Vec::new();
    let error = walk(&root, 0, &mut files).map(|err| format!("{}: {err}", root.display()));
    (files, error)
}

/// Every session file of both agents, newest first (metadata only: nothing is opened).
pub(crate) fn discover(dirs: &TranscriptDirs) -> (Vec<FileEntry>, Option<String>) {
    let mut files = Vec::new();
    let mut error = None;
    if let Some(dir) = &dirs.claude_config {
        let (found, err) = discover_claude(dir);
        files.extend(found);
        error = error.or(err);
    }
    if let Some(dir) = &dirs.codex_home {
        let (found, err) = discover_codex(dir);
        files.extend(found);
        error = error.or(err);
    }
    files.sort_by(|a, b| b.mtime_ms.cmp(&a.mtime_ms).then_with(|| a.path.cmp(&b.path)));
    (files, error)
}

// ---------------------------------------------------------------------------------------------
// Scanner with cache
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub(crate) struct ScanResult {
    /// Sessions of the newest files, newest first; empty sessions are left out.
    pub sessions: Vec<SessionMeta>,
    /// Session files that exist (parsed or not).
    pub total_files: usize,
    /// The first problem met (an unreadable directory, or no agent directory at all).
    pub error: Option<String>,
}

#[derive(Default)]
pub(crate) struct Scanner {
    /// Per file: (mtime, length) it was parsed at, and the result (`None` = hidden).
    cache: HashMap<PathBuf, (u64, u64, Option<SessionMeta>)>,
    /// Files actually parsed so far (observable cache behavior).
    parses: usize,
}

impl Scanner {
    #[cfg(test)]
    pub(crate) fn parse_count(&self) -> usize {
        self.parses
    }

    /// Lists the newest `limit` session files and returns their summaries. Unchanged files come
    /// from the cache; never panics on bad input.
    pub(crate) fn scan(&mut self, dirs: &TranscriptDirs, limit: usize) -> ScanResult {
        let (files, mut error) = discover(dirs);
        if dirs.claude_config.is_none() && dirs.codex_home.is_none() {
            error = Some("no agent directories".to_owned());
        }
        let total_files = files.len();
        let present: std::collections::HashSet<&Path> = files.iter().map(|f| f.path.as_path()).collect();
        self.cache.retain(|path, _| present.contains(path.as_path()));
        let mut sessions = Vec::new();
        for file in files.iter().take(limit) {
            let cached = self.cache.get(&file.path).filter(|(mtime, len, _)| *mtime == file.mtime_ms && *len == file.len);
            let meta = match cached {
                Some((_, _, meta)) => meta.clone(),
                None => {
                    self.parses += 1;
                    let meta = parse_file(file.kind, &file.path, file.mtime_ms, file.len);
                    self.cache.insert(file.path.clone(), (file.mtime_ms, file.len, meta.clone()));
                    meta
                }
            };
            if let Some(mut meta) = meta {
                meta.cwd_exists = meta.cwd.as_deref().is_some_and(Path::is_dir);
                sessions.push(meta);
            }
        }
        ScanResult { sessions, total_files, error }
    }
}

// ---------------------------------------------------------------------------------------------
// Turns of one session (expanded row)
// ---------------------------------------------------------------------------------------------

/// Same-role turns in a row become one; each is cut to `TURN_MAX` chars.
fn merge_turns(raw: Vec<(Role, String)>) -> Vec<Turn> {
    let mut merged: Vec<(Role, String)> = Vec::new();
    for (role, text) in raw {
        match merged.last_mut() {
            Some((last_role, last_text)) if *last_role == role => {
                last_text.push(' ');
                last_text.push_str(&text);
            }
            _ => merged.push((role, text)),
        }
    }
    merged
        .into_iter()
        .map(|(role, text)| Turn { role, text: text::one_line(&text, TURN_MAX) })
        .filter(|turn| !turn.text.is_empty())
        .collect()
}

/// The last `max_turns` turns (user prompts and agent replies with text) among `lines`.
fn turns_from_lines(lines: &[String], max_turns: usize) -> Vec<Turn> {
    let (mut claude, mut events, mut items) = (Vec::new(), Vec::new(), Vec::new());
    for line in lines {
        if let Some(turn) = claude::turn(line) {
            claude.push(turn);
        } else if let Some((role, text, event)) = codex::turn(line) {
            if event { events.push((role, text)) } else { items.push((role, text)) }
        }
    }
    let raw = if !claude.is_empty() {
        claude
    } else if !events.is_empty() {
        events
    } else {
        items
    };
    let mut turns = merge_turns(raw);
    let skip = turns.len().saturating_sub(max_turns);
    turns.drain(..skip);
    turns
}

/// The last `max_turns` turns of a session file; reads only its last `TURNS_TAIL_BYTES`. Empty on
/// any read problem.
pub(crate) fn session_turns(path: &Path, max_turns: usize) -> Vec<Turn> {
    match tail_lines(path, TURNS_TAIL_BYTES) {
        Ok(lines) => turns_from_lines(&lines, max_turns),
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests;
