//! Files changed by a pull request and their per-user "viewed" flags, through `gh`.
use std::collections::HashMap;
use std::time::Duration;

use serde_json::Value;

use super::gh::GhExec;
use super::github::GitHub;
use super::ForgeError;

const READ_TIMEOUT: Duration = Duration::from_secs(30);
const WRITE_TIMEOUT: Duration = Duration::from_secs(60);
const PAGE_SIZE: usize = 100;
/// GitHub lists at most 3000 files of a pull request.
pub const MAX_FILES: usize = 3000;
const MAX_PAGES: usize = MAX_FILES / PAGE_SIZE;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Removed,
    Modified,
    Renamed,
    Copied,
    Changed,
    Unchanged,
}

impl FileStatus {
    pub fn parse(status: &str) -> Self {
        match status {
            "added" => Self::Added,
            "removed" => Self::Removed,
            "renamed" => Self::Renamed,
            "copied" => Self::Copied,
            "changed" => Self::Changed,
            "unchanged" => Self::Unchanged,
            _ => Self::Modified,
        }
    }

    pub fn letter(self) -> char {
        match self {
            Self::Added => 'A',
            Self::Removed => 'D',
            Self::Modified | Self::Changed => 'M',
            Self::Renamed => 'R',
            Self::Copied => 'C',
            Self::Unchanged => 'U',
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrFile {
    pub path: String,
    pub previous_path: Option<String>,
    pub status: FileStatus,
    pub additions: u32,
    pub deletions: u32,
}

/// Viewed flags of a pull request and the node id needed to change them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ViewedState {
    pub pr_id: String,
    pub viewed: HashMap<String, bool>,
}

fn invalid_json(error: impl std::fmt::Display) -> ForgeError {
    ForgeError::Failed(format!("Invalid gh output: {error}"))
}

/// One page of `repos/O/R/pulls/N/files`.
pub fn parse_files_page(json: &str) -> Result<Vec<PrFile>, ForgeError> {
    let value: Value = serde_json::from_str(json).map_err(invalid_json)?;
    let items = value.as_array().ok_or_else(|| invalid_json("expected an array"))?;
    let text = |item: &Value, key: &str| item.get(key).and_then(Value::as_str).unwrap_or("").to_owned();
    let count = |item: &Value, key: &str| item.get(key).and_then(Value::as_u64).unwrap_or(0).min(u64::from(u32::MAX)) as u32;
    Ok(items
        .iter()
        .filter_map(|item| {
            let path = text(item, "filename");
            (!path.is_empty()).then(|| PrFile {
                path,
                previous_path: Some(text(item, "previous_filename")).filter(|p| !p.is_empty()),
                status: FileStatus::parse(&text(item, "status")),
                additions: count(item, "additions"),
                deletions: count(item, "deletions"),
            })
        })
        .collect())
}

/// One page of the viewed-state GraphQL query: (pull request id, files, next cursor).
pub fn parse_viewed_page(json: &str) -> Result<(String, Vec<(String, bool)>, Option<String>), ForgeError> {
    let value: Value = serde_json::from_str(json).map_err(invalid_json)?;
    let pr = value
        .pointer("/data/repository/pullRequest")
        .filter(|pr| !pr.is_null())
        .ok_or_else(|| invalid_json("pull request not found"))?;
    let id = pr.get("id").and_then(Value::as_str).unwrap_or("").to_owned();
    let files = pr.get("files");
    let nodes = files.and_then(|f| f.get("nodes")).and_then(Value::as_array).cloned().unwrap_or_default();
    let viewed = nodes
        .iter()
        .filter_map(|node| {
            let path = node.get("path").and_then(Value::as_str)?;
            let state = node.get("viewerViewedState").and_then(Value::as_str).unwrap_or("");
            Some((path.to_owned(), state == "VIEWED"))
        })
        .collect();
    let info = files.and_then(|f| f.get("pageInfo"));
    let next = info
        .filter(|i| i.get("hasNextPage").and_then(Value::as_bool) == Some(true))
        .and_then(|i| i.get("endCursor"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    Ok((id, viewed, next))
}

const VIEWED_QUERY: &str = "query($owner:String!,$name:String!,$number:Int!,$after:String){repository(owner:$owner,name:$name){pullRequest(number:$number){id files(first:100,after:$after){pageInfo{hasNextPage endCursor} nodes{path viewerViewedState}}}}}";

impl<E: GhExec> GitHub<E> {
    /// Every file of the pull request (up to 3000), page by page.
    pub fn pr_files(&self, number: u64) -> Result<Vec<PrFile>, ForgeError> {
        let mut all = Vec::new();
        for page in 1..=MAX_PAGES {
            let path = format!("{}/files?per_page={PAGE_SIZE}&page={page}", self.pr_path(number));
            let output = self.run_ok(&self.api_args(&[&path]), READ_TIMEOUT)?;
            let files = parse_files_page(&output.stdout)?;
            let full = files.len() >= PAGE_SIZE;
            all.extend(files);
            if !full {
                break;
            }
        }
        all.truncate(MAX_FILES);
        Ok(all)
    }

    /// The viewer's "viewed" flags for every file, and the pull request's node id.
    pub fn viewed_state(&self, number: u64) -> Result<ViewedState, ForgeError> {
        let mut state = ViewedState::default();
        let mut after: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let mut rest = vec![
                "graphql".to_owned(),
                "-f".to_owned(),
                format!("query={VIEWED_QUERY}"),
                "-f".to_owned(),
                format!("owner={}", self.repo().owner),
                "-f".to_owned(),
                format!("name={}", self.repo().name),
                "-F".to_owned(),
                format!("number={number}"),
            ];
            if let Some(cursor) = &after {
                rest.push("-f".to_owned());
                rest.push(format!("after={cursor}"));
            }
            let rest: Vec<&str> = rest.iter().map(String::as_str).collect();
            let output = self.run_ok(&self.api_args(&rest), READ_TIMEOUT)?;
            let (id, files, next) = parse_viewed_page(&output.stdout)?;
            if state.pr_id.is_empty() {
                state.pr_id = id;
            }
            state.viewed.extend(files);
            match next {
                Some(cursor) => after = Some(cursor),
                None => break,
            }
        }
        Ok(state)
    }

    /// Marks (or unmarks) one file as viewed.
    pub fn set_file_viewed(&self, pr_id: &str, path: &str, viewed: bool) -> Result<(), ForgeError> {
        if pr_id.is_empty() || path.is_empty() {
            return Err(ForgeError::Failed("Missing pull request id or path".to_owned()));
        }
        let mutation = if viewed { "markFileAsViewed" } else { "unmarkFileAsViewed" };
        let query = format!(
            "mutation($pullRequestId:ID!,$path:String!){{ {mutation}(input:{{pullRequestId:$pullRequestId,path:$path}}){{pullRequest{{id}}}} }}"
        );
        let args = self.api_args(&[
            "graphql",
            "-f",
            &format!("query={query}"),
            "-f",
            &format!("pullRequestId={pr_id}"),
            "-f",
            &format!("path={path}"),
        ]);
        self.run_ok(&args, WRITE_TIMEOUT).map(|_| ())
    }
}

/// (additions, deletions) over all files.
pub fn totals(files: &[PrFile]) -> (u32, u32) {
    files.iter().fold((0, 0), |(a, d), f| (a.saturating_add(f.additions), d.saturating_add(f.deletions)))
}

/// How many of `files` are marked viewed.
pub fn viewed_count(files: &[PrFile], viewed: &HashMap<String, bool>) -> usize {
    files.iter().filter(|f| viewed.get(&f.path).copied().unwrap_or(false)).count()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::super::details::testutil::*;
    use super::*;
    use crate::forge::RemoteRepo;

    fn github(exec: &FakeExec) -> GitHub<&FakeExec> {
        GitHub::with_exec(
            exec,
            PathBuf::from("."),
            RemoteRepo { host: "github.com".into(), owner: "o".into(), name: "r".into() },
        )
    }

    fn page(count: usize, offset: usize) -> String {
        let items: Vec<String> = (0..count)
            .map(|i| format!(r#"{{"filename":"f{}","status":"added","additions":1,"deletions":0}}"#, offset + i))
            .collect();
        format!("[{}]", items.join(","))
    }

    #[test]
    fn parses_a_files_page() {
        let files = parse_files_page(
            r#"[{"filename":"b.rs","previous_filename":"a.rs","status":"renamed","additions":2,"deletions":3},
                {"filename":"c","status":"weird"},{"status":"added"}]"#,
        )
        .unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].previous_path.as_deref(), Some("a.rs"));
        assert_eq!(files[0].status, FileStatus::Renamed);
        assert_eq!((files[0].additions, files[0].deletions), (2, 3));
        assert_eq!(files[1].status, FileStatus::Modified);
        assert_eq!(files[1].previous_path, None);
        assert!(parse_files_page("{}").is_err());
        assert!(parse_files_page("x").is_err());
        assert_eq!(totals(&files), (2, 3));
    }

    #[test]
    fn status_letters() {
        assert_eq!(FileStatus::parse("removed").letter(), 'D');
        assert_eq!(FileStatus::parse("added").letter(), 'A');
        assert_eq!(FileStatus::parse("copied").letter(), 'C');
        assert_eq!(FileStatus::parse("changed").letter(), 'M');
    }

    #[test]
    fn pages_until_a_short_page() {
        let exec = FakeExec::with(vec![ok(&page(100, 0)), ok(&page(5, 100))]);
        let files = github(&exec).pr_files(9).unwrap();
        assert_eq!(files.len(), 105);
        let calls = exec.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0], vec!["api".to_owned(), "repos/o/r/pulls/9/files?per_page=100&page=1".to_owned()]);
        assert_eq!(calls[1][1], "repos/o/r/pulls/9/files?per_page=100&page=2");
    }

    #[test]
    fn stops_at_the_file_limit() {
        let responses = (0..40).map(|i| ok(&page(100, i * 100))).collect();
        let exec = FakeExec::with(responses);
        assert_eq!(github(&exec).pr_files(1).unwrap().len(), MAX_FILES);
        assert_eq!(exec.calls().len(), 30);
    }

    #[test]
    fn files_error_is_propagated() {
        let exec = FakeExec::with(vec![fail("nope")]);
        assert_eq!(github(&exec).pr_files(1), Err(ForgeError::Failed("nope".into())));
    }

    const VIEWED_1: &str = r#"{"data":{"repository":{"pullRequest":{"id":"PR_1","files":{"pageInfo":{"hasNextPage":true,"endCursor":"CUR"},"nodes":[{"path":"a","viewerViewedState":"VIEWED"},{"path":"b","viewerViewedState":"UNVIEWED"}]}}}}}"#;
    const VIEWED_2: &str = r#"{"data":{"repository":{"pullRequest":{"id":"PR_1","files":{"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[{"path":"c","viewerViewedState":"DISMISSED"}]}}}}}"#;

    #[test]
    fn parses_viewed_pages() {
        let (id, files, next) = parse_viewed_page(VIEWED_1).unwrap();
        assert_eq!(id, "PR_1");
        assert_eq!(files, vec![("a".to_owned(), true), ("b".to_owned(), false)]);
        assert_eq!(next.as_deref(), Some("CUR"));
        assert_eq!(parse_viewed_page(VIEWED_2).unwrap().2, None);
        assert!(parse_viewed_page(r#"{"data":{"repository":{"pullRequest":null}}}"#).is_err());
        assert!(parse_viewed_page("x").is_err());
    }

    #[test]
    fn viewed_state_pages_with_the_cursor() {
        let exec = FakeExec::with(vec![ok(VIEWED_1), ok(VIEWED_2)]);
        let state = github(&exec).viewed_state(4).unwrap();
        assert_eq!(state.pr_id, "PR_1");
        assert_eq!(state.viewed.len(), 3);
        assert!(state.viewed["a"] && !state.viewed["b"] && !state.viewed["c"]);
        let calls = exec.calls();
        assert!(calls[0].contains(&"number=4".to_owned()));
        assert!(!calls[0].iter().any(|a| a.starts_with("after=")));
        assert!(calls[1].contains(&"after=CUR".to_owned()));
        let files = vec![
            PrFile { path: "a".into(), previous_path: None, status: FileStatus::Added, additions: 0, deletions: 0 },
            PrFile { path: "b".into(), previous_path: None, status: FileStatus::Added, additions: 0, deletions: 0 },
        ];
        assert_eq!(viewed_count(&files, &state.viewed), 1);
    }

    #[test]
    fn marks_and_unmarks_viewed() {
        let exec = FakeExec::with(vec![ok("{}"), ok("{}"), fail("denied")]);
        let gh = github(&exec);
        gh.set_file_viewed("PR_1", "dir/-odd file.rs", true).unwrap();
        gh.set_file_viewed("PR_1", "x", false).unwrap();
        assert!(gh.set_file_viewed("PR_1", "x", true).is_err());
        assert!(gh.set_file_viewed("", "x", true).is_err());
        let calls = exec.calls();
        assert_eq!(calls.len(), 3);
        assert!(calls[0][3].contains("markFileAsViewed(") && !calls[0][3].contains("unmark"));
        assert!(calls[0].contains(&"pullRequestId=PR_1".to_owned()));
        assert!(calls[0].contains(&"path=dir/-odd file.rs".to_owned()));
        assert!(calls[1][3].contains("unmarkFileAsViewed("));
    }
}
