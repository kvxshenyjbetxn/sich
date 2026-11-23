//! Fixture-based tests: every file lives in a temp dir, nothing reads the real agent directories.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::{Value, json};
use sich_proto::{AgentKind, AgentState, AgentStatus, ProjectInfo, TerminalInfo, WorktreeInfo};

use super::*;

const SID: &str = "11111111-2222-3333-4444-555555555555";

fn write_lines(path: &Path, lines: &[Value]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(path, text).unwrap();
}

fn set_mtime(path: &Path, secs: u64) {
    let file = std::fs::File::options().write(true).open(path).unwrap();
    file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(secs)).unwrap();
}

fn c_user(text: &str) -> Value {
    json!({"type": "user", "sessionId": SID, "cwd": "/work/app", "gitBranch": "main",
           "timestamp": "2026-11-01T10:00:00.000Z", "message": {"role": "user", "content": text}})
}

fn c_assistant(text: &str, model: &str) -> Value {
    json!({"type": "assistant", "sessionId": SID, "cwd": "/work/app", "gitBranch": "main",
           "timestamp": "2026-11-01T10:00:05.000Z",
           "message": {"role": "assistant", "model": model, "content": [{"type": "text", "text": text}]}})
}

fn claude_file(root: &Path, project: &str, id: &str, lines: &[Value]) -> PathBuf {
    let path = root.join("projects").join(project).join(format!("{id}.jsonl"));
    write_lines(&path, lines);
    path
}

fn parse_claude(lines: &[Value]) -> Option<SessionMeta> {
    let dir = tempfile::tempdir().unwrap();
    let path = claude_file(dir.path(), "p", SID, lines);
    let len = std::fs::metadata(&path).unwrap().len();
    parse_file(AgentKind::Claude, &path, 42, len)
}

#[test]
fn claude_session_summary() {
    let meta = parse_claude(&[
        c_user("  fix the\n parser  "),
        c_assistant("Looking at it.", "claude-opus-5"),
        c_user("now add tests"),
        c_assistant("Added tests.", "claude-opus-5"),
    ])
    .unwrap();
    assert_eq!(meta.kind, AgentKind::Claude);
    assert_eq!(meta.id, SID);
    assert_eq!(meta.cwd, Some(PathBuf::from("/work/app")));
    assert_eq!(meta.branch.as_deref(), Some("main"));
    assert_eq!(meta.first_prompt, "fix the parser");
    assert_eq!(meta.last_prompt, "now add tests");
    assert_eq!(meta.last_assistant.as_deref(), Some("Added tests."));
    assert_eq!(meta.model.as_deref(), Some("claude-opus-5"));
    assert_eq!(meta.title.as_deref(), Some("fix the parser"));
    assert_eq!(meta.created_ms, Some(1_793_527_200_000));
    assert_eq!(meta.updated_ms, 42);
    assert_eq!(meta.display_title(), "fix the parser");
}

#[test]
fn claude_id_is_the_file_name_not_a_record_field() {
    let dir = tempfile::tempdir().unwrap();
    let path = claude_file(dir.path(), "p", "aaaaaaaa-bbbb", &[c_user("hi")]);
    let meta = parse_file(AgentKind::Claude, &path, 1, std::fs::metadata(&path).unwrap().len()).unwrap();
    assert_eq!(meta.id, "aaaaaaaa-bbbb");
}

#[test]
fn title_priority_is_custom_then_ai_then_first_prompt() {
    let ai = json!({"type": "ai-title", "aiTitle": "AI made title"});
    let custom = json!({"type": "custom-title", "customTitle": "My   name"});
    let only_prompt = parse_claude(&[c_user("first"), c_assistant("a", "m")]).unwrap();
    assert_eq!(only_prompt.title.as_deref(), Some("first"));
    let with_ai = parse_claude(&[c_user("first"), ai.clone()]).unwrap();
    assert_eq!(with_ai.title.as_deref(), Some("AI made title"));
    let with_both = parse_claude(&[c_user("first"), custom.clone(), ai.clone()]).unwrap();
    assert_eq!(with_both.title.as_deref(), Some("My name"));
    // A cleared custom title falls back to the AI title.
    let cleared = json!({"type": "custom-title", "customTitle": ""});
    let after_clear = parse_claude(&[c_user("first"), custom, ai, cleared]).unwrap();
    assert_eq!(after_clear.title.as_deref(), Some("AI made title"));
    // A long first prompt becomes a title of at most 120 chars; the prompt keeps up to 400.
    let long = "word ".repeat(200);
    let meta = parse_claude(&[c_user(&long)]).unwrap();
    assert_eq!(meta.title.as_deref().unwrap().chars().count(), 120);
    assert!(meta.title.as_deref().unwrap().ends_with('\u{2026}'));
    assert_eq!(meta.first_prompt.chars().count(), 400);
}

#[test]
fn injected_and_non_prompt_user_records_are_skipped() {
    let meta_user = json!({"type": "user", "isMeta": true, "message": {"role": "user", "content": "meta caveat"}});
    let command = c_user("<command-name>/clear</command-name>");
    let local = c_user("<local-command-stdout>ok</local-command-stdout>");
    let reminder = c_user("<system-reminder>remember</system-reminder>");
    let tool_result = json!({"type": "user", "message": {"role": "user",
        "content": [{"type": "tool_result", "tool_use_id": "x", "content": "output"}]}});
    let sidechain = json!({"type": "user", "isSidechain": true, "message": {"role": "user", "content": "subagent task"}});
    let meta = parse_claude(&[meta_user, command, local, reminder, tool_result, sidechain, c_user("the real prompt")]).unwrap();
    assert_eq!(meta.first_prompt, "the real prompt");
    assert_eq!(meta.last_prompt, "the real prompt");
}

#[test]
fn session_without_a_user_prompt_is_hidden() {
    assert!(parse_claude(&[c_assistant("hello", "m")]).is_none());
    assert!(parse_claude(&[json!({"type": "ai-title", "aiTitle": "x"}), c_user("<system-reminder>x</system-reminder>")]).is_none());
    assert!(parse_claude(&[]).is_none());
}

#[test]
fn last_prompt_record_fills_in_when_no_message_survives() {
    let meta = parse_claude(&[json!({"type": "last-prompt", "lastPrompt": "from the record"})]).unwrap();
    assert_eq!(meta.first_prompt, "from the record");
    assert_eq!(meta.last_prompt, "from the record");
}

#[test]
fn malformed_and_partial_lines_are_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("projects").join("p").join(format!("{SID}.jsonl"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let text = format!(
        "not json at all\n{{\"type\":\"user\",\"message\":{{\"content\":\n{}\n[1,2,3]\n{{\"type\":5,\"cwd\":7}}\n\n{}\n{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"te",
        c_user("first real"),
        c_user("second real"),
    );
    std::fs::write(&path, text).unwrap();
    let len = std::fs::metadata(&path).unwrap().len();
    let meta = parse_file(AgentKind::Claude, &path, 1, len).unwrap();
    assert_eq!(meta.first_prompt, "first real");
    assert_eq!(meta.last_prompt, "second real");
    assert_eq!(meta.last_assistant, None);
}

#[test]
fn unreadable_or_missing_file_is_hidden_not_a_panic() {
    assert!(parse_file(AgentKind::Claude, Path::new("/definitely/not/here.jsonl"), 1, 10).is_none());
}

#[test]
fn huge_file_is_read_only_at_the_head_and_the_tail() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("projects").join("p").join(format!("{SID}.jsonl"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let filler = json!({"type": "tools-used", "blob": "x".repeat(4000)}).to_string();
    let mut text = format!("{}\n", c_user("head prompt"));
    for _ in 0..(2 * 1024 * 1024 / filler.len() + 1) {
        text.push_str(&filler);
        text.push('\n');
    }
    // Records in the unread middle must not leak into the summary.
    text.push_str(&format!("{}\n", json!({"type": "ai-title", "aiTitle": "MIDDLE TITLE"})));
    text.push_str(&format!("{}\n", c_user("middle prompt")));
    for _ in 0..(2 * 1024 * 1024 / filler.len() + 1) {
        text.push_str(&filler);
        text.push('\n');
    }
    text.push_str(&format!("{}\n", c_user("tail prompt")));
    text.push_str(&format!("{}\n", c_assistant("tail answer", "claude-x")));
    std::fs::write(&path, &text).unwrap();
    let len = std::fs::metadata(&path).unwrap().len();
    assert!(len > 4 * 1024 * 1024);
    let meta = parse_file(AgentKind::Claude, &path, 1, len).unwrap();
    assert_eq!(meta.first_prompt, "head prompt");
    assert_eq!(meta.last_prompt, "tail prompt");
    assert_eq!(meta.last_assistant.as_deref(), Some("tail answer"));
    assert_eq!(meta.title.as_deref(), Some("head prompt"), "the title record sits in the unread middle");
    let lines = head_and_tail_lines(&path, len).unwrap();
    assert!(lines.iter().map(|l| l.len() as u64).sum::<u64>() <= HEAD_BYTES + TAIL_BYTES);
}

#[test]
fn a_line_cut_by_the_head_or_tail_boundary_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("big.jsonl");
    let line = format!("{}\n", json!({"type": "x", "pad": "y".repeat(1000)}));
    let count = (3 * 1024 * 1024) / line.len();
    std::fs::write(&path, line.repeat(count)).unwrap();
    let len = std::fs::metadata(&path).unwrap().len();
    let lines = head_and_tail_lines(&path, len).unwrap();
    assert!(!lines.is_empty());
    for l in &lines {
        assert!(serde_json::from_str::<Value>(l).is_ok(), "cut line survived: {}", &l[..l.len().min(40)]);
    }
}

fn codex_lines(id: &str) -> Vec<Value> {
    vec![
        json!({"timestamp": "2026-11-02T08:00:00.000Z", "type": "session_meta",
               "payload": {"id": id, "cwd": "C:\\work\\app", "git": {"branch": "feat/x", "commit_hash": "abc"}}}),
        json!({"type": "response_item", "payload": {"type": "message", "role": "user",
               "content": [{"type": "input_text", "text": "<environment_context>cwd</environment_context>"}]}}),
        json!({"type": "turn_context", "payload": {"cwd": "C:\\work\\other", "model": "gpt-5-codex"}}),
        json!({"type": "event_msg", "payload": {"type": "user_message", "message": "implement   the thing"}}),
        json!({"type": "event_msg", "payload": {"type": "agent_message", "message": "Working on it"}}),
        json!({"type": "event_msg", "payload": {"type": "token_count", "info": {}}}),
        json!({"type": "event_msg", "payload": {"type": "user_message", "message": "and the tests"}}),
        json!({"type": "event_msg", "payload": {"type": "agent_message", "message": "Done"}}),
    ]
}

fn codex_file(home: &Path, name: &str, lines: &[Value]) -> PathBuf {
    let path = home.join("sessions").join("2026").join("11").join("02").join(name);
    write_lines(&path, lines);
    path
}

const CID: &str = "0198a1b2-c3d4-7000-8000-000000000001";

#[test]
fn codex_session_summary() {
    let dir = tempfile::tempdir().unwrap();
    let path = codex_file(dir.path(), &format!("rollout-2026-11-02T08-00-00-{CID}.jsonl"), &codex_lines(CID));
    let len = std::fs::metadata(&path).unwrap().len();
    let meta = parse_file(AgentKind::Codex, &path, 7, len).unwrap();
    assert_eq!(meta.kind, AgentKind::Codex);
    assert_eq!(meta.id, CID);
    assert_eq!(meta.cwd, Some(PathBuf::from("C:\\work\\app")), "session_meta cwd wins over later turn contexts");
    assert_eq!(meta.branch.as_deref(), Some("feat/x"));
    assert_eq!(meta.model.as_deref(), Some("gpt-5-codex"));
    assert_eq!(meta.first_prompt, "implement the thing");
    assert_eq!(meta.last_prompt, "and the tests");
    assert_eq!(meta.last_assistant.as_deref(), Some("Done"));
    assert_eq!(meta.title.as_deref(), Some("implement the thing"));
    assert_eq!(meta.created_ms, Some(1_793_606_400_000));
}

#[test]
fn codex_falls_back_to_the_file_name_id_response_items_and_turn_context_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let lines = vec![
        json!({"type": "turn_context", "payload": {"cwd": "/w/ctx", "model": "m1"}}),
        json!({"type": "response_item", "payload": {"type": "message", "role": "user",
               "content": [{"type": "input_text", "text": "# AGENTS.md instructions for /w"}]}}),
        json!({"type": "response_item", "payload": {"type": "message", "role": "user",
               "content": [{"type": "input_text", "text": "real request"}]}}),
        json!({"type": "response_item", "payload": {"type": "message", "role": "assistant",
               "content": [{"type": "output_text", "text": "real answer"}]}}),
    ];
    let path = codex_file(dir.path(), &format!("rollout-2026-11-02T08-00-00-{CID}.jsonl"), &lines);
    let meta = parse_file(AgentKind::Codex, &path, 1, std::fs::metadata(&path).unwrap().len()).unwrap();
    assert_eq!(meta.id, CID);
    assert_eq!(meta.cwd, Some(PathBuf::from("/w/ctx")));
    assert_eq!(meta.first_prompt, "real request");
    assert_eq!(meta.last_assistant.as_deref(), Some("real answer"));
}

#[test]
fn codex_subagent_threads_and_empty_sessions_are_hidden() {
    let dir = tempfile::tempdir().unwrap();
    let mut lines = codex_lines(CID);
    lines[0]["payload"]["source"] = json!({"subagent": {"thread_spawn": {}}});
    let path = codex_file(dir.path(), &format!("rollout-2026-11-02T08-00-00-{CID}.jsonl"), &lines);
    assert!(parse_file(AgentKind::Codex, &path, 1, std::fs::metadata(&path).unwrap().len()).is_none());
    let empty = codex_file(dir.path(), "rollout-2026-11-02T09-00-00-eeeeeeee-ffff.jsonl", &codex_lines(CID)[..1]);
    assert!(parse_file(AgentKind::Codex, &empty, 1, std::fs::metadata(&empty).unwrap().len()).is_none());
}

#[test]
fn codex_file_name_ids() {
    assert_eq!(codex::id_from_file_name(&format!("rollout-2026-11-02T08-00-00-{CID}.jsonl")).as_deref(), Some(CID));
    assert_eq!(codex::id_from_file_name("rollout-2026-11-02T08-00-00-.jsonl"), None);
    assert_eq!(codex::id_from_file_name("rollout-short.jsonl"), None);
    assert_eq!(codex::id_from_file_name("other-2026-11-02T08-00-00-abc.jsonl"), None);
    assert_eq!(codex::id_from_file_name("rollout-2026-11-02T08-00-00-abc.txt"), None);
}

fn dirs_of(root: &Path) -> TranscriptDirs {
    TranscriptDirs { claude_config: Some(root.join("claude")), codex_home: Some(root.join("codex")) }
}

#[test]
fn claude_discovery_takes_direct_children_of_project_dirs_only() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("claude");
    let good = claude_file(&config, "E--proj", "aaaaaaaa-0001", &[c_user("x")]);
    let nested = config.join("projects").join("E--proj").join("aaaaaaaa-0001").join("subagents").join("agent-1.jsonl");
    write_lines(&nested, &[c_user("sub")]);
    write_lines(&config.join("projects").join("top-level.jsonl"), &[c_user("loose")]);
    write_lines(&config.join("projects").join("E--proj").join("notes.txt"), &[json!({})]);
    let (files, error) = discover_claude(&config);
    assert_eq!(error, None);
    assert_eq!(files.iter().map(|f| f.path.clone()).collect::<Vec<_>>(), vec![good]);
    assert_eq!(discover_claude(&dir.path().join("none")), (Vec::new(), None));
}

#[test]
fn codex_discovery_matches_rollouts_within_the_depth_bound() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("codex");
    let ok = codex_file(&home, "rollout-2026-11-02T08-00-00-aaaa1111.jsonl", &[json!({})]);
    write_lines(&home.join("sessions").join("rollout-top.jsonl"), &[json!({})]);
    write_lines(&home.join("sessions").join("2026").join("11").join("02").join("other.jsonl"), &[json!({})]);
    write_lines(&home.join("sessions").join("2026").join("11").join("02").join("rollout-x.txt"), &[json!({})]);
    let deep = home.join("sessions").join("a").join("b").join("c").join("d").join("e").join("rollout-deep.jsonl");
    write_lines(&deep, &[json!({})]);
    let at_limit = home.join("sessions").join("a").join("b").join("c").join("d").join("rollout-limit.jsonl");
    write_lines(&at_limit, &[json!({})]);
    let (files, error) = discover_codex(&home);
    assert_eq!(error, None);
    let mut paths: Vec<PathBuf> = files.into_iter().map(|f| f.path).collect();
    paths.sort();
    let mut expected = vec![ok, home.join("sessions").join("rollout-top.jsonl"), at_limit];
    expected.sort();
    assert_eq!(paths, expected);
    assert_eq!(discover_codex(&dir.path().join("none")), (Vec::new(), None));
}

#[test]
fn scan_returns_the_newest_files_first_and_honors_the_limit() {
    let dir = tempfile::tempdir().unwrap();
    let dirs = dirs_of(dir.path());
    let mut paths = Vec::new();
    for (i, name) in ["aaaaaaaa-0001", "aaaaaaaa-0002", "aaaaaaaa-0003"].into_iter().enumerate() {
        let path = claude_file(dir.path().join("claude").as_path(), "p", name, &[c_user(&format!("prompt {i}"))]);
        set_mtime(&path, 1000 + [30, 10, 20][i]);
        paths.push(path);
    }
    let codex = codex_file(&dir.path().join("codex"), &format!("rollout-2026-11-02T08-00-00-{CID}.jsonl"), &codex_lines(CID));
    set_mtime(&codex, 1025);
    let mut scanner = Scanner::default();
    let all = scanner.scan(&dirs, 10);
    assert_eq!(all.total_files, 4);
    assert_eq!(all.error, None);
    let order: Vec<&str> = all.sessions.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(order, vec!["aaaaaaaa-0001", CID, "aaaaaaaa-0003", "aaaaaaaa-0002"]);
    assert!(all.sessions.windows(2).all(|w| w[0].updated_ms >= w[1].updated_ms));
    let limited = scanner.scan(&dirs, 2);
    assert_eq!(limited.total_files, 4);
    let order: Vec<&str> = limited.sessions.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(order, vec!["aaaaaaaa-0001", CID]);
}

#[test]
fn scan_hides_empty_sessions_and_reports_the_missing_directories() {
    let dir = tempfile::tempdir().unwrap();
    let dirs = dirs_of(dir.path());
    claude_file(&dir.path().join("claude"), "p", "aaaaaaaa-0001", &[c_assistant("only the agent", "m")]);
    let result = Scanner::default().scan(&dirs, 10);
    assert_eq!(result.total_files, 1);
    assert!(result.sessions.is_empty());
    let none = Scanner::default().scan(&TranscriptDirs::default(), 10);
    assert!(none.sessions.is_empty());
    assert!(none.error.is_some());
}

#[test]
fn scan_marks_whether_the_working_directory_exists() {
    let dir = tempfile::tempdir().unwrap();
    let dirs = dirs_of(dir.path());
    let real = dir.path().join("project");
    std::fs::create_dir_all(&real).unwrap();
    let mut here = c_user("here");
    here["cwd"] = json!(real.to_str().unwrap());
    claude_file(&dir.path().join("claude"), "p", "aaaaaaaa-0001", &[here]);
    let mut gone = c_user("gone");
    gone["cwd"] = json!(dir.path().join("deleted").to_str().unwrap());
    claude_file(&dir.path().join("claude"), "p", "aaaaaaaa-0002", &[gone]);
    let mut result = Scanner::default().scan(&dirs, 10).sessions;
    result.sort_by(|a, b| a.id.cmp(&b.id));
    assert_eq!(result.iter().map(|s| s.cwd_exists).collect::<Vec<_>>(), vec![true, false]);
}

#[test]
fn unchanged_files_are_not_parsed_again() {
    let dir = tempfile::tempdir().unwrap();
    let dirs = dirs_of(dir.path());
    let a = claude_file(&dir.path().join("claude"), "p", "aaaaaaaa-0001", &[c_user("one")]);
    claude_file(&dir.path().join("claude"), "p", "aaaaaaaa-0002", &[c_user("two")]);
    set_mtime(&a, 1000);
    let mut scanner = Scanner::default();
    scanner.scan(&dirs, 10);
    assert_eq!(scanner.parse_count(), 2);
    let again = scanner.scan(&dirs, 10);
    assert_eq!(scanner.parse_count(), 2, "nothing changed: served from the cache");
    assert_eq!(again.sessions.len(), 2);
    // A new mtime or length re-parses just that file.
    write_lines(&a, &[c_user("one"), c_user("one changed")]);
    set_mtime(&a, 2000);
    let changed = scanner.scan(&dirs, 10);
    assert_eq!(scanner.parse_count(), 3);
    assert_eq!(changed.sessions.iter().find(|s| s.id == "aaaaaaaa-0001").unwrap().last_prompt, "one changed");
    // Empty sessions are cached too; a file that disappears leaves the cache.
    std::fs::remove_file(&a).unwrap();
    let after = scanner.scan(&dirs, 10);
    assert_eq!(after.sessions.len(), 1);
    assert_eq!(scanner.parse_count(), 3);
    assert_eq!(scanner.cache.len(), 1);
}

#[test]
fn files_beyond_the_limit_are_not_parsed() {
    let dir = tempfile::tempdir().unwrap();
    let dirs = dirs_of(dir.path());
    for i in 0..5u64 {
        let path = claude_file(&dir.path().join("claude"), "p", &format!("aaaaaaaa-000{i}"), &[c_user("x")]);
        set_mtime(&path, 1000 + i);
    }
    let mut scanner = Scanner::default();
    scanner.scan(&dirs, 2);
    assert_eq!(scanner.parse_count(), 2);
    scanner.scan(&dirs, 4);
    assert_eq!(scanner.parse_count(), 4, "only the two newly shown files are parsed");
}

#[test]
fn claude_turns_merge_runs_and_skip_noise() {
    let dir = tempfile::tempdir().unwrap();
    let tool_use = json!({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "tool_use", "name": "Read"}]}});
    let tool_result = json!({"type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "content": "x"}]}});
    let meta = json!({"type": "user", "isMeta": true, "message": {"role": "user", "content": "caveat"}});
    let path = claude_file(
        dir.path(),
        "p",
        SID,
        &[
            c_user("q1"),
            c_assistant("a1", "m"),
            tool_use,
            tool_result,
            c_assistant("a1b", "m"),
            meta,
            json!({"type": "ai-title", "aiTitle": "t"}),
            c_user("q2"),
            c_assistant(&"long ".repeat(300), "m"),
        ],
    );
    let turns = session_turns(&path, 8);
    let shown: Vec<(Role, &str)> = turns.iter().map(|t| (t.role, t.text.as_str())).take(3).collect();
    assert_eq!(shown, vec![(Role::User, "q1"), (Role::Assistant, "a1 a1b"), (Role::User, "q2")]);
    assert_eq!(turns.len(), 4);
    assert_eq!(turns[3].role, Role::Assistant);
    assert_eq!(turns[3].text.chars().count(), 600);
    let last_two = session_turns(&path, 2);
    assert_eq!(last_two.len(), 2);
    assert_eq!(last_two[0].text, "q2");
    assert!(session_turns(&path, 0).is_empty());
    assert!(session_turns(&dir.path().join("none.jsonl"), 8).is_empty());
}

#[test]
fn codex_turns_prefer_event_messages() {
    let dir = tempfile::tempdir().unwrap();
    let mut lines = codex_lines(CID);
    lines.insert(
        1,
        json!({"type": "response_item", "payload": {"type": "message", "role": "assistant",
               "content": [{"type": "output_text", "text": "duplicate from response item"}]}}),
    );
    let path = codex_file(dir.path(), &format!("rollout-2026-11-02T08-00-00-{CID}.jsonl"), &lines);
    let turns = session_turns(&path, 8);
    let shown: Vec<(Role, &str)> = turns.iter().map(|t| (t.role, t.text.as_str())).collect();
    assert_eq!(
        shown,
        vec![
            (Role::User, "implement the thing"),
            (Role::Assistant, "Working on it"),
            (Role::User, "and the tests"),
            (Role::Assistant, "Done"),
        ]
    );
}

#[test]
fn turns_read_only_the_tail_of_a_big_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("projects").join("p").join(format!("{SID}.jsonl"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let filler = json!({"type": "tools-used", "blob": "x".repeat(4000)}).to_string();
    let mut text = format!("{}\n", c_user("ancient prompt"));
    for _ in 0..(3 * 1024 * 1024 / filler.len()) {
        text.push_str(&filler);
        text.push('\n');
    }
    text.push_str(&format!("{}\n{}\n", c_user("recent prompt"), c_assistant("recent answer", "m")));
    std::fs::write(&path, text).unwrap();
    let turns = session_turns(&path, 8);
    assert_eq!(turns.iter().map(|t| t.text.as_str()).collect::<Vec<_>>(), vec!["recent prompt", "recent answer"]);
}

// ---- filter ----------------------------------------------------------------------------------

fn meta(kind: AgentKind, id: &str, cwd: Option<&str>) -> SessionMeta {
    SessionMeta {
        kind,
        id: id.into(),
        path: PathBuf::from(format!("/s/{id}.jsonl")),
        cwd: cwd.map(PathBuf::from),
        cwd_exists: true,
        branch: Some("Feature/Login".into()),
        title: Some("Refactor Auth".into()),
        first_prompt: "Please refactor".into(),
        last_prompt: "ship it".into(),
        last_assistant: None,
        model: None,
        created_ms: None,
        updated_ms: 1,
    }
}

fn project(path: &str, worktrees: &[&str]) -> ProjectInfo {
    ProjectInfo {
        id: Default::default(),
        name: "proj".into(),
        path: PathBuf::from(path),
        is_git: true,
        worktrees: worktrees
            .iter()
            .map(|p| WorktreeInfo { path: PathBuf::from(p), branch: None, head: String::new(), is_main: false, dirty: false })
            .collect(),
        error: None,
        group: None,
    }
}

#[test]
fn inside_means_equal_or_below_with_whole_components() {
    assert!(is_inside(Path::new("/w/app"), Path::new("/w/app")));
    assert!(is_inside(Path::new("/w/app/src/x"), Path::new("/w/app")));
    assert!(is_inside(Path::new("/w/app/"), Path::new("/w/app")));
    assert!(!is_inside(Path::new("/w/app-two"), Path::new("/w/app")));
    assert!(!is_inside(Path::new("/w"), Path::new("/w/app")));
    assert!(!is_inside(Path::new("/w/app"), Path::new("")));
    if cfg!(windows) {
        assert!(is_inside(Path::new(r"C:\Work\App\src"), Path::new("c:/work/app/")));
        assert!(!is_inside(Path::new(r"C:\Work\App2"), Path::new("c:/work/app")));
        assert!(is_inside(Path::new(r"C:\Work"), Path::new(r"C:\")));
    } else {
        assert!(is_inside(Path::new("/w"), Path::new("/")));
    }
}

#[test]
fn scope_filters_by_worktree_project_or_everything() {
    let projects = vec![project("/w/app", &["/w/app", "/w/wt/feat"]), project("/w/other", &["/w/other"])];
    let selected = Some(Path::new("/w/wt/feat"));
    let cwd = |p: &'static str| Some(Path::new(p));
    assert!(in_scope(Scope::Worktree, cwd("/w/wt/feat"), selected, &projects));
    assert!(in_scope(Scope::Worktree, cwd("/w/wt/feat/sub"), selected, &projects));
    assert!(!in_scope(Scope::Worktree, cwd("/w/app"), selected, &projects));
    assert!(!in_scope(Scope::Worktree, None, selected, &projects));
    assert!(in_scope(Scope::Project, cwd("/w/app"), selected, &projects));
    assert!(in_scope(Scope::Project, cwd("/w/app/pkg"), selected, &projects));
    assert!(in_scope(Scope::Project, cwd("/w/wt/feat/x"), selected, &projects));
    assert!(!in_scope(Scope::Project, cwd("/w/other"), selected, &projects));
    assert!(in_scope(Scope::All, cwd("/anywhere"), selected, &projects));
    assert!(in_scope(Scope::All, None, selected, &projects));
    // Unknown project: the project scope degrades to the worktree.
    assert!(in_scope(Scope::Project, cwd("/x/y/z"), Some(Path::new("/x/y")), &projects));
    assert!(!in_scope(Scope::Project, cwd("/x/q"), Some(Path::new("/x/y")), &projects));
    // Home selected: nothing narrows the list.
    assert!(in_scope(Scope::Worktree, cwd("/anywhere"), None, &projects));
    assert!(in_scope(Scope::Worktree, None, None, &projects));
    if cfg!(windows) {
        let projects = vec![project(r"C:\Repo", &[r"C:\Repo", r"C:\Repo-wt\feat"])];
        assert!(in_scope(Scope::Worktree, Some(Path::new("c:/repo/SRC")), Some(Path::new(r"C:\Repo\")), &projects));
        assert!(in_scope(Scope::Project, Some(Path::new("c:/REPO-WT/feat")), Some(Path::new(r"C:\Repo")), &projects));
    }
}

#[test]
fn search_matches_title_prompts_branch_and_cwd_case_insensitively() {
    let m = meta(AgentKind::Claude, "a", Some("/work/Billing"));
    for query in ["auth", "REFACTOR", "ship", "feature/login", "billing", "  Auth  ", ""] {
        assert!(matches_query(&m, query), "{query}");
    }
    assert!(!matches_query(&m, "zebra"));
    let bare = SessionMeta { title: None, branch: None, cwd: None, ..m };
    assert!(!matches_query(&bare, "auth"));
    assert!(matches_query(&bare, "please"));
}

#[test]
fn filter_combines_agent_toggles_scope_and_search() {
    let projects = vec![project("/w/app", &["/w/app"])];
    let sessions = vec![
        meta(AgentKind::Claude, "c1", Some("/w/app")),
        meta(AgentKind::Codex, "x1", Some("/w/app/sub")),
        meta(AgentKind::Claude, "c2", Some("/elsewhere")),
    ];
    let base = Filter { scope: Scope::Worktree, query: "", claude: true, codex: true, selected: Some(Path::new("/w/app")), projects: &projects };
    assert_eq!(filter_sessions(&sessions, &base), vec![0, 1]);
    assert_eq!(filter_sessions(&sessions, &Filter { codex: false, ..base.clone() }), vec![0]);
    assert_eq!(filter_sessions(&sessions, &Filter { claude: false, ..base.clone() }), vec![1]);
    assert_eq!(filter_sessions(&sessions, &Filter { claude: false, codex: false, ..base.clone() }), Vec::<usize>::new());
    assert_eq!(filter_sessions(&sessions, &Filter { scope: Scope::All, ..base.clone() }), vec![0, 1, 2]);
    assert_eq!(filter_sessions(&sessions, &Filter { query: "nomatch", ..base }), Vec::<usize>::new());
}

fn terminal(id: u64, session: Option<&str>, exited: bool, worktree: Option<&str>) -> TerminalInfo {
    TerminalInfo {
        id,
        title: String::new(),
        cwd: PathBuf::from("/w/app"),
        cols: 80,
        rows: 24,
        exited: exited.then_some(Some(0)),
        created_unix_ms: 0,
        worktree: worktree.map(PathBuf::from),
        agent: Some(AgentStatus {
            kind: AgentKind::Claude,
            state: AgentState::Working,
            unread: false,
            since_unix_ms: 0,
            detail: None,
            model: None,
            title: None,
            prompt: None,
            message: None,
            subagents: Vec::new(),
            session_id: session.map(str::to_owned),
        }),
    }
}

#[test]
fn live_terminals_map_to_sessions_by_session_id() {
    let mut no_agent = terminal(4, None, false, None);
    no_agent.agent = None;
    let terminals = vec![
        terminal(1, Some("sess-one"), false, Some("/w/app")),
        terminal(2, Some("sess-dead"), true, None),
        terminal(3, None, false, None),
        no_agent,
    ];
    let live = live_sessions(&terminals);
    assert_eq!(live.len(), 1);
    let one = &live["sess-one"];
    assert_eq!(one.term, 1);
    assert_eq!(one.worktree.as_deref(), Some(Path::new("/w/app")));
    assert_eq!(one.status.state, AgentState::Working);
    assert!(live_sessions(&Vec::<TerminalInfo>::new()).is_empty());
}
