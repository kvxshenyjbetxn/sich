//! Text for the "Send to agent" action: a failed-job log tail and the prompt built from it.
//!
//! Pure functions only. The text is pasted into the user's agent terminal (never submitted).
use std::path::{Path, PathBuf};

use super::model::{FailedJob, PullRequest};

pub const LOG_TAIL_LINES: usize = 80;
pub const LOG_TAIL_BYTES: usize = 8 * 1024;

/// Removes ANSI escape sequences (CSI, OSC and two-byte escapes).
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                for next in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&next) {
                        break;
                    }
                }
            }
            Some(']') => {
                while let Some(next) = chars.next() {
                    if next == '\u{7}' {
                        break;
                    }
                    if next == '\u{1b}' {
                        chars.next_if_eq(&'\\');
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

fn looks_like_timestamp(token: &str) -> bool {
    let bytes = token.as_bytes();
    bytes.len() >= 19
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[4] == b'-'
        && token.contains('T')
        && token.ends_with('Z')
}

/// `<job>\t<step>\t<timestamp> <text>` -> `<text>`; lines in other shapes are kept as they are.
fn clean_line(line: &str) -> String {
    let line = line.trim_end_matches(['\r', '\n']);
    let mut parts = line.splitn(3, '\t');
    let (first, second, third) = (parts.next(), parts.next(), parts.next());
    let body = match (second, third) {
        (Some(_), Some(rest)) => rest,
        _ => first.unwrap_or(""),
    };
    let body = body.trim_start_matches('\u{feff}');
    let body = match body.split_once(' ') {
        Some((token, rest)) if looks_like_timestamp(token) => rest,
        _ if looks_like_timestamp(body) => "",
        _ => body,
    };
    strip_ansi(body).trim_end().to_owned()
}

/// Last `max_lines` non-empty lines of a `gh run view --log-failed` output without the job/step
/// columns, timestamps and ANSI escapes, capped to `max_bytes` (cut from the start).
pub fn log_tail(raw: &str, max_lines: usize, max_bytes: usize) -> String {
    if max_lines == 0 || max_bytes == 0 {
        return String::new();
    }
    let mut lines: Vec<String> = raw
        .lines()
        .map(clean_line)
        .filter(|line| !line.trim().is_empty())
        .collect();
    if lines.len() > max_lines {
        lines.drain(..lines.len() - max_lines);
    }
    let text = lines.join("\n");
    if text.len() <= max_bytes {
        return text;
    }
    let mut start = text.len() - max_bytes;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_owned()
}

/// Prompt describing the failed checks of `pr`. `None` log means it could not be fetched.
pub fn agent_prompt(pr: &PullRequest, failures: &[(FailedJob, Option<String>)]) -> String {
    let mut out = format!("CI failed on PR #{} \"{}\" ({}).\n", pr.number, pr.title, pr.url);
    for (job, tail) in failures {
        out.push_str("\nFailed check: ");
        out.push_str(&job.name);
        if let Some(url) = &job.url {
            out.push_str(&format!(" ({url})"));
        }
        out.push('\n');
        match tail.as_deref().map(str::trim_end).filter(|t| !t.is_empty()) {
            Some(tail) => {
                // A fence longer than any backtick run in the log keeps the block intact.
                let longest = tail
                    .split(|c| c != '`')
                    .map(str::len)
                    .max()
                    .unwrap_or(0);
                let fence = "`".repeat(longest.max(2) + 1);
                out.push_str(&format!("Log tail:\n{fence}\n{tail}\n{fence}\n"));
            }
            None => out.push_str("(log not available)\n"),
        }
    }
    out.push_str("\nInvestigate the failure, fix it, and tell me what you changed.");
    out
}

const MAX_TITLE_CHARS: usize = 120;
const MAX_NAMES: usize = 5;
/// How many report files are kept in the reports directory.
pub const KEEP_REPORTS: usize = 20;

/// Control characters become spaces and runs of spaces collapse into one.
fn one_line(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last_space = true;
    for c in text.chars() {
        let c = if c.is_control() { ' ' } else { c };
        if c == ' ' {
            if !last_space {
                out.push(' ');
            }
            last_space = true;
        } else {
            out.push(c);
            last_space = false;
        }
    }
    out.trim_end().to_owned()
}

/// The single line pasted into the agent terminal. It has no line breaks, so it can never run
/// as several commands if the terminal turns out to be a plain shell; the full report is in
/// `report_path`.
pub fn agent_prompt_line(pr: &PullRequest, failed_names: &[String], report_path: &Path) -> String {
    let mut title = one_line(&pr.title);
    if title.chars().count() > MAX_TITLE_CHARS {
        title = title.chars().take(MAX_TITLE_CHARS).collect::<String>().trim_end().to_owned() + "…";
    }
    let names: Vec<String> = failed_names
        .iter()
        .take(MAX_NAMES)
        .map(|name| one_line(name))
        .filter(|name| !name.is_empty())
        .collect();
    let mut list = names.join(", ");
    if failed_names.len() > MAX_NAMES {
        list.push_str(&format!(", +{} more", failed_names.len() - MAX_NAMES));
    }
    let path = one_line(&report_path.display().to_string());
    let checks = if list.is_empty() { String::new() } else { format!(": {list}") };
    format!(
        "CI failed on PR #{} \"{title}\"{checks}. Full report with log tails: {path}. Read it, fix the failure, and tell me what you changed.",
        pr.number
    )
}

fn report_stamp(name: &str) -> Option<u128> {
    name.strip_suffix(".md")?.rsplit('-').next()?.parse().ok()
}

/// Writes `content` to `<dir>/pr-<number>-<unix_ms>.md` (creating `dir`) and prunes the
/// directory to the `KEEP_REPORTS` newest reports.
pub fn write_report(dir: &Path, number: u64, unix_ms: u128, content: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("pr-{number}-{unix_ms}.md"));
    std::fs::write(&path, content)?;
    prune_reports(dir, KEEP_REPORTS);
    Ok(path)
}

/// Keeps the `keep` newest `*.md` files of `dir` (by the timestamp in the name, then by name)
/// and deletes the rest. Errors are ignored.
pub fn prune_reports(dir: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<(u128, String, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.ends_with(".md").then(|| (report_stamp(&name).unwrap_or(0), name, entry.path()))
        })
        .collect();
    files.sort_by(|a, b| (b.0, &b.1).cmp(&(a.0, &a.1)));
    for (_, _, path) in files.into_iter().skip(keep) {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::{PrState, parse_pr_json};

    fn pr() -> PullRequest {
        let mut pr = parse_pr_json(r#"{"number":123,"title":"Fix it","url":"https://github.com/o/r/pull/123"}"#)
            .unwrap();
        pr.state = PrState::Open;
        pr
    }

    fn job(name: &str, url: Option<&str>) -> FailedJob {
        FailedJob {
            name: name.into(),
            url: url.map(Into::into),
            job_id: None,
        }
    }

    #[test]
    fn tail_strips_columns_timestamps_and_ansi() {
        let raw = "build\tRun tests\t2026-11-22T10:00:00.1234567Z \u{1b}[31merror\u{1b}[0m: boom\n\
                   build\tRun tests\t2026-11-22T10:00:01.0000000Z second\r\n";
        assert_eq!(log_tail(raw, 80, 8192), "error: boom\nsecond");
    }

    #[test]
    fn tail_keeps_last_non_empty_lines() {
        let raw = "j\ts\t2026-11-22T10:00:00Z a\n\nj\ts\t2026-11-22T10:00:00Z \nj\ts\t2026-11-22T10:00:00Z b\nj\ts\t2026-11-22T10:00:00Z c\n";
        assert_eq!(log_tail(raw, 2, 8192), "b\nc");
        assert_eq!(log_tail(raw, 10, 8192), "a\nb\nc");
        assert_eq!(log_tail(raw, 0, 8192), "");
        assert_eq!(log_tail(raw, 5, 0), "");
    }

    #[test]
    fn tail_caps_bytes_on_a_char_boundary() {
        let raw = "ééééé\nabc";
        // "ééééé\nabc" is 14 bytes; keeping 5 would cut in the middle of "é".
        let tail = log_tail(raw, 10, 5);
        assert!(tail.len() <= 5);
        assert!(tail.ends_with("abc"));
        assert!(std::str::from_utf8(tail.as_bytes()).is_ok());
        assert_eq!(log_tail("abcdef", 10, 3), "def");
    }

    #[test]
    fn tail_handles_odd_input() {
        assert_eq!(log_tail("", 80, 8192), "");
        assert_eq!(log_tail("\n\n  \n", 80, 8192), "");
        // No tabs / no timestamp: kept as is.
        assert_eq!(log_tail("plain line\nother", 80, 8192), "plain line\nother");
        // Tabs inside the text after the timestamp survive; only two columns are stripped.
        assert_eq!(
            log_tail("j\ts\t2026-11-22T10:00:00Z a\tb", 80, 8192),
            "a\tb"
        );
        // Timestamp-only line and BOM.
        assert_eq!(log_tail("j\ts\t2026-11-22T10:00:00Z\nj\ts\t\u{feff}2026-11-22T10:00:00Z x", 80, 8192), "x");
        // Unterminated escape does not panic.
        assert_eq!(log_tail("a\u{1b}[31", 80, 8192), "a");
        assert_eq!(log_tail("a\u{1b}]0;title\u{7}b", 80, 8192), "ab");
    }

    #[test]
    fn prompt_single_failure_with_log() {
        let text = agent_prompt(
            &pr(),
            &[(job("CI / test", Some("https://x/job/1")), Some("boom\nline".into()))],
        );
        assert_eq!(
            text,
            "CI failed on PR #123 \"Fix it\" (https://github.com/o/r/pull/123).\n\
             \n\
             Failed check: CI / test (https://x/job/1)\n\
             Log tail:\n\
             ```\n\
             boom\n\
             line\n\
             ```\n\
             \n\
             Investigate the failure, fix it, and tell me what you changed."
        );
        assert!(!text.ends_with('\n'));
    }

    #[test]
    fn prompt_multi_failure_and_missing_logs() {
        let text = agent_prompt(
            &pr(),
            &[
                (job("A", None), None),
                (job("B", Some("https://x/b")), Some("  \n".into())),
                (job("C", None), Some("tail".into())),
            ],
        );
        assert_eq!(text.matches("Failed check: ").count(), 3);
        assert_eq!(text.matches("(log not available)").count(), 2);
        assert_eq!(text.matches("Log tail:").count(), 1);
        assert!(text.contains("Failed check: A\n(log not available)"));
        assert!(text.contains("Failed check: B (https://x/b)\n(log not available)"));
    }

    #[test]
    fn prompt_without_failures_and_with_backticks() {
        let text = agent_prompt(&pr(), &[]);
        assert!(text.starts_with("CI failed on PR #123"));
        assert!(text.ends_with("tell me what you changed."));
        let text = agent_prompt(&pr(), &[(job("A", None), Some("```\nx\n```".into()))]);
        assert!(text.contains("````\n```\nx\n```\n````"));
    }

    #[test]
    fn line_is_single_and_sanitized() {
        let mut p = pr();
        p.title = "a\r\nb\x07  c\td".into();
        let names = vec!["x\ny".to_owned(), " z ".to_owned()];
        let line = agent_prompt_line(&p, &names, Path::new("C:/my dir/r\n.md"));
        assert!(!line.chars().any(char::is_control), "{line:?}");
        assert!(line.starts_with("CI failed on PR #123 \"a b c d\": x y, z. Full report"), "{line}");
        assert!(line.contains("C:/my dir/r .md."), "{line}");
        assert!(line.ends_with("tell me what you changed."));
    }

    #[test]
    fn line_caps_names_and_title() {
        let mut p = pr();
        p.title = "é".repeat(300);
        let names: Vec<String> = (1..=8).map(|n| format!("n{n}")).collect();
        let line = agent_prompt_line(&p, &names, Path::new("/tmp/r.md"));
        assert!(line.contains(": n1, n2, n3, n4, n5, +3 more. "), "{line}");
        assert!(!line.contains("n6"));
        let title = line.split('"').nth(1).unwrap();
        assert_eq!(title.chars().count(), 121);
        assert!(title.ends_with('…'));
        let none = agent_prompt_line(&pr(), &[], Path::new("/tmp/r.md"));
        assert!(none.starts_with("CI failed on PR #123 \"Fix it\". Full report"), "{none}");
    }

    #[test]
    fn write_report_names_file_and_prunes_to_newest() {
        let dir = tempfile::tempdir().unwrap();
        let reports = dir.path().join("nested").join("agent-prompts");
        std::fs::create_dir_all(&reports).unwrap();
        std::fs::write(reports.join("note.txt"), "keep").unwrap();
        for ms in 1..=25u128 {
            let path = write_report(&reports, 7, 1000 + ms, "body").unwrap();
            assert_eq!(path.file_name().unwrap().to_str().unwrap(), format!("pr-7-{}.md", 1000 + ms));
        }
        let mut names: Vec<String> = std::fs::read_dir(&reports)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".md"))
            .collect();
        names.sort();
        assert_eq!(names.len(), KEEP_REPORTS);
        assert_eq!(names[0], "pr-7-1006.md");
        assert!(names.contains(&"pr-7-1025.md".to_owned()));
        assert!(reports.join("note.txt").exists());
        assert_eq!(std::fs::read_to_string(reports.join("pr-7-1025.md")).unwrap(), "body");
    }

    #[test]
    fn prune_ignores_missing_dir() {
        let dir = tempfile::tempdir().unwrap();
        prune_reports(&dir.path().join("none"), 3);
    }
}
