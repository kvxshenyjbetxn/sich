//! Listing the open pull requests of a repository and looking up one by number, for the
//! "new worktree from a pull request" flow. Plain `gh` calls only.
use serde_json::Value;

use super::github::{GitHub, VIEW_TIMEOUT};
use super::gh::GhExec;
use super::remote::RemoteRepo;
use super::ForgeError;

const LIST_FIELDS: &str = "number,title,headRefName,author,isCrossRepository,headRepositoryOwner,headRepository,isDraft,updatedAt";
pub const LIST_LIMIT: u32 = 100;

/// One pull request as shown in the picker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrListItem {
    pub number: u64,
    pub title: String,
    pub head_ref: String,
    pub author: String,
    /// The head branch lives in another repository (a fork).
    pub cross_repository: bool,
    /// Login of the owner of the head repository (`None` when the fork was deleted).
    pub head_owner: Option<String>,
    /// Name of the head repository.
    pub head_repo: Option<String>,
    pub draft: bool,
    pub updated_at: String,
}

impl PrListItem {
    /// `owner:branch` for a fork PR, else the plain branch name.
    pub fn head_label(&self) -> String {
        match (&self.head_owner, self.cross_repository) {
            (Some(owner), true) => format!("{owner}:{}", self.head_ref),
            _ => self.head_ref.clone(),
        }
    }
}

fn str_at<'a>(value: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut current = value;
    for key in path {
        current = current.get(key)?;
    }
    current.as_str().filter(|s| !s.is_empty())
}

/// One PR object of `gh pr list/view --json`; `None` without a number or head branch.
pub fn parse_pr_item(value: &Value) -> Option<PrListItem> {
    let number = value.get("number")?.as_u64().filter(|n| *n > 0)?;
    let head_ref = str_at(value, &["headRefName"])?.to_owned();
    Some(PrListItem {
        number,
        title: str_at(value, &["title"]).unwrap_or_default().to_owned(),
        head_ref,
        author: str_at(value, &["author", "login"]).unwrap_or_default().to_owned(),
        cross_repository: value.get("isCrossRepository").and_then(Value::as_bool).unwrap_or(false),
        head_owner: str_at(value, &["headRepositoryOwner", "login"]).map(str::to_owned),
        head_repo: str_at(value, &["headRepository", "name"]).map(str::to_owned),
        draft: value.get("isDraft").and_then(Value::as_bool).unwrap_or(false),
        updated_at: str_at(value, &["updatedAt"]).unwrap_or_default().to_owned(),
    })
}

/// The JSON array printed by `gh pr list --json`. Entries without a number/head are skipped.
pub fn parse_pr_list(json: &str) -> Result<Vec<PrListItem>, ForgeError> {
    let value: Value = serde_json::from_str(json)
        .map_err(|error| ForgeError::Failed(format!("Invalid gh output: {error}")))?;
    let items = value
        .as_array()
        .ok_or_else(|| ForgeError::Failed("Invalid gh output: expected a list".to_owned()))?;
    Ok(items.iter().filter_map(parse_pr_item).collect())
}

/// A pull request the user typed: a number (`123`, `#123`) or a URL of a PR page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrRef {
    pub number: u64,
    /// Repository named by a URL; `None` for a bare number.
    pub repo: Option<RemoteRepo>,
}

fn parse_number(text: &str) -> Option<u64> {
    if text.is_empty() || text.len() > 9 || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok().filter(|n| *n > 0)
}

pub fn parse_pr_input(text: &str) -> Option<PrRef> {
    let text = text.trim();
    if let Some(number) = parse_number(text.strip_prefix('#').unwrap_or(text)) {
        return Some(PrRef { number, repo: None });
    }
    let (_, rest) = text.split_once("://")?;
    let rest = rest.split(['?', '#']).next()?;
    let mut parts = rest.trim_end_matches('/').split('/');
    let (host, owner, name) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next()? != "pull" {
        return None;
    }
    let number = parse_number(parts.next()?)?;
    let repo = RemoteRepo::parse(&format!("https://{host}/{owner}/{name}"))?;
    Some(PrRef { number, repo: Some(repo) })
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_owned()).collect()
}

impl<E: GhExec> GitHub<E> {
    /// Open pull requests of this (base) repository, newest activity first, at most [`LIST_LIMIT`].
    pub fn list_open_prs(&self) -> Result<Vec<PrListItem>, ForgeError> {
        let limit = LIST_LIMIT.to_string();
        let args = argv(&[
            "pr",
            "list",
            "--repo",
            &self.repo().gh_repo_arg(),
            "--state",
            "open",
            "--limit",
            &limit,
            "--json",
            LIST_FIELDS,
        ]);
        let output = self.run_ok(&args, VIEW_TIMEOUT)?;
        parse_pr_list(&output.stdout)
    }

    /// One pull request (any state) by number.
    pub fn pr_summary(&self, number: u64) -> Result<PrListItem, ForgeError> {
        let args = argv(&[
            "pr",
            "view",
            &number.to_string(),
            "--repo",
            &self.repo().gh_repo_arg(),
            "--json",
            LIST_FIELDS,
        ]);
        let output = self.run_ok(&args, VIEW_TIMEOUT)?;
        let value: Value = serde_json::from_str(&output.stdout)
            .map_err(|error| ForgeError::Failed(format!("Invalid gh output: {error}")))?;
        parse_pr_item(&value).ok_or_else(|| ForgeError::Failed("Invalid gh output".to_owned()))
    }

    /// Clone URL of the repository the PR head lives in (`None` when that fork was deleted).
    pub fn pr_head_clone_url(&self, number: u64) -> Result<Option<String>, ForgeError> {
        let endpoint = self.repo_endpoint(&format!("pulls/{number}"));
        let args = self.api_argv(&[&endpoint, "--jq", ".head.repo.clone_url"]);
        let output = self.run_ok(&args, VIEW_TIMEOUT)?;
        let url = output.stdout.trim();
        Ok((!url.is_empty() && url != "null").then(|| url.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::time::Duration;

    use super::super::gh::GhOutput;
    use super::*;

    #[derive(Default)]
    struct FakeExec {
        calls: Mutex<Vec<Vec<String>>>,
        responses: Mutex<VecDeque<Result<GhOutput, ForgeError>>>,
    }

    impl FakeExec {
        fn with(responses: Vec<Result<GhOutput, ForgeError>>) -> Self {
            Self { responses: Mutex::new(responses.into()), ..Self::default() }
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl GhExec for &FakeExec {
        fn run(&self, _: &Path, args: &[String], _: Duration) -> Result<GhOutput, ForgeError> {
            self.calls.lock().unwrap().push(args.to_vec());
            self.responses.lock().unwrap().pop_front().expect("unscripted gh call")
        }
    }

    fn ok(stdout: &str) -> Result<GhOutput, ForgeError> {
        Ok(GhOutput { code: Some(0), stdout: stdout.into(), stderr: String::new() })
    }

    fn github<'a>(exec: &'a FakeExec, url: &str) -> GitHub<&'a FakeExec> {
        GitHub::with_exec(exec, PathBuf::from("."), RemoteRepo::parse(url).unwrap())
    }

    fn v(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|p| (*p).to_owned()).collect()
    }

    const LIST: &str = r#"[
      {"number":12,"title":"Add feature","headRefName":"feat/x","author":{"login":"alice","is_bot":false},
       "isCrossRepository":false,"headRepositoryOwner":{"id":"1","login":"o"},"headRepository":{"id":"2","name":"r"},
       "isDraft":false,"updatedAt":"2026-05-01T10:00:00Z"},
      {"number":13,"title":"WIP from a fork","headRefName":"main","author":{"login":"bob"},
       "isCrossRepository":true,"headRepositoryOwner":{"login":"bobby"},"headRepository":{"name":"r-fork"},
       "isDraft":true,"updatedAt":"2026-05-02T10:00:00Z"},
      {"number":14,"title":"Deleted fork","headRefName":"patch-1","author":null,
       "isCrossRepository":true,"headRepositoryOwner":null,"headRepository":null,
       "isDraft":false,"updatedAt":"2026-05-03T10:00:00Z"},
      {"title":"no number","headRefName":"x"},
      {"number":15,"title":"no head"}
    ]"#;

    #[test]
    fn parses_same_repo_fork_draft_and_deleted_fork() {
        let items = parse_pr_list(LIST).unwrap();
        assert_eq!(items.iter().map(|i| i.number).collect::<Vec<_>>(), [12, 13, 14]);
        let (a, b, c) = (&items[0], &items[1], &items[2]);
        assert_eq!((a.title.as_str(), a.head_ref.as_str(), a.author.as_str()), ("Add feature", "feat/x", "alice"));
        assert!(!a.cross_repository && !a.draft);
        assert_eq!(a.head_owner.as_deref(), Some("o"));
        assert_eq!(a.head_label(), "feat/x");
        assert!(b.cross_repository && b.draft);
        assert_eq!((b.head_owner.as_deref(), b.head_repo.as_deref()), (Some("bobby"), Some("r-fork")));
        assert_eq!(b.head_label(), "bobby:main");
        assert_eq!(c.author, "");
        assert_eq!((c.head_owner.as_deref(), c.head_repo.as_deref()), (None, None));
        assert_eq!(c.head_label(), "patch-1");
    }

    #[test]
    fn invalid_json_is_an_error() {
        assert!(matches!(parse_pr_list("not json"), Err(ForgeError::Failed(_))));
        assert!(matches!(parse_pr_list("{}"), Err(ForgeError::Failed(_))));
        assert_eq!(parse_pr_list("[]").unwrap(), vec![]);
    }

    #[test]
    fn list_argv_targets_the_base_repo_with_open_state_and_limit() {
        let exec = FakeExec::with(vec![ok(LIST)]);
        let items = github(&exec, "https://github.com/o/r.git").list_open_prs().unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(
            exec.calls(),
            vec![v(&[
                "pr", "list", "--repo", "o/r", "--state", "open", "--limit", "100", "--json", LIST_FIELDS
            ])]
        );
        let exec = FakeExec::with(vec![ok("[]")]);
        github(&exec, "https://ghe.example.com/team/app").list_open_prs().unwrap();
        assert_eq!(exec.calls()[0][3], "ghe.example.com/team/app");
    }

    #[test]
    fn list_maps_gh_failures() {
        let exec = FakeExec::with(vec![Ok(GhOutput {
            code: Some(4),
            stdout: String::new(),
            stderr: "gh auth login".into(),
        })]);
        assert_eq!(github(&exec, "https://github.com/o/r").list_open_prs(), Err(ForgeError::NotAuthenticated));
        let exec = FakeExec::with(vec![Err(ForgeError::GhMissing)]);
        assert_eq!(github(&exec, "https://github.com/o/r").list_open_prs(), Err(ForgeError::GhMissing));
    }

    #[test]
    fn summary_uses_pr_view() {
        let exec = FakeExec::with(vec![ok(
            r#"{"number":13,"title":"t","headRefName":"main","author":{"login":"bob"},"isCrossRepository":true,
               "headRepositoryOwner":{"login":"bobby"},"headRepository":{"name":"r-fork"},"isDraft":false,"updatedAt":""}"#,
        )]);
        let pr = github(&exec, "https://github.com/o/r").pr_summary(13).unwrap();
        assert_eq!((pr.number, pr.head_owner.as_deref()), (13, Some("bobby")));
        assert_eq!(
            exec.calls(),
            vec![v(&["pr", "view", "13", "--repo", "o/r", "--json", LIST_FIELDS])]
        );
        let exec = FakeExec::with(vec![ok("{}")]);
        assert!(github(&exec, "https://github.com/o/r").pr_summary(1).is_err());
    }

    #[test]
    fn clone_url_comes_from_the_api() {
        let exec = FakeExec::with(vec![ok("https://github.com/bobby/r-fork.git\n"), ok("null\n"), ok("")]);
        let gh = github(&exec, "https://github.com/o/r");
        assert_eq!(gh.pr_head_clone_url(13).unwrap().as_deref(), Some("https://github.com/bobby/r-fork.git"));
        assert_eq!(gh.pr_head_clone_url(14).unwrap(), None);
        assert_eq!(gh.pr_head_clone_url(15).unwrap(), None);
        assert_eq!(exec.calls()[0], v(&["api", "repos/o/r/pulls/13", "--jq", ".head.repo.clone_url"]));
        let exec = FakeExec::with(vec![ok("")]);
        github(&exec, "https://ghe.example.com/o/r").pr_head_clone_url(1).unwrap();
        assert_eq!(
            exec.calls()[0],
            v(&["api", "repos/o/r/pulls/1", "--jq", ".head.repo.clone_url", "--hostname", "ghe.example.com"])
        );
    }

    #[test]
    fn parses_numbers_and_urls() {
        let bare = |n| Some(PrRef { number: n, repo: None });
        assert_eq!(parse_pr_input("123"), bare(123));
        assert_eq!(parse_pr_input("  #45 "), bare(45));
        let url = |n| Some(PrRef { number: n, repo: RemoteRepo::parse("https://github.com/o/r") });
        for text in [
            "https://github.com/o/r/pull/7",
            "https://github.com/o/r/pull/7/",
            "https://github.com/o/r/pull/7/files",
            "https://github.com/o/r/pull/7#issuecomment-1",
            "https://github.com/o/r/pull/7?diff=split",
            " https://github.com/o/r/pull/7 ",
        ] {
            assert_eq!(parse_pr_input(text), url(7), "{text}");
        }
        assert_eq!(
            parse_pr_input("https://ghe.example.com/team/app/pull/3").unwrap().repo,
            RemoteRepo::parse("https://ghe.example.com/team/app")
        );
    }

    #[test]
    fn rejects_garbage_inputs() {
        for text in [
            "",
            "#",
            "0",
            "abc",
            "-5",
            "12x",
            "99999999999",
            "https://github.com/o/r/issues/7",
            "https://github.com/o/r/pull/",
            "https://github.com/o/r/pull/x",
            "https://github.com/o/pull/7",
            "github.com/o/r/pull/7",
        ] {
            assert_eq!(parse_pr_input(text), None, "{text}");
        }
    }
}
