//! Pull request details (description, people, labels) and their edits, through `gh`.
//! Everything targets the base repository. Text always travels as a single argv element.
use std::time::Duration;

use serde_json::Value;

use super::gh::{GhExec, GhOutput};
use super::github::GitHub;
use super::ForgeError;

const READ_TIMEOUT: Duration = Duration::from_secs(30);
const WRITE_TIMEOUT: Duration = Duration::from_secs(60);
const DETAILS_FIELDS: &str = "body,author,createdAt,labels,assignees,reviewRequests,latestReviews";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Label {
    pub name: String,
    /// Hex colour without `#` (may be empty when unknown).
    pub color: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reviewer {
    /// A user login, or `org/team-slug` for a team.
    pub login: String,
    pub is_team: bool,
}

/// A candidate for the user pickers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoUser {
    pub login: String,
    pub name: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PrDetails {
    pub body: String,
    pub author: String,
    pub created_at: String,
    pub labels: Vec<Label>,
    pub assignees: Vec<String>,
    pub requested_reviewers: Vec<Reviewer>,
    /// Latest review per reviewer: `APPROVED`, `CHANGES_REQUESTED` or `COMMENTED`.
    pub reviews: Vec<(String, String)>,
}

/// One change to the details, applied optimistically and then sent to GitHub.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DetailsEdit {
    AddLabel(Label),
    RemoveLabel(String),
    AddAssignee(String),
    RemoveAssignee(String),
    AddReviewer(Reviewer),
    RemoveReviewer(String),
    SetBody(String),
}

impl PrDetails {
    /// The details after `edit` (the caller keeps the old value for a rollback).
    pub fn with_edit(&self, edit: &DetailsEdit) -> PrDetails {
        let mut next = self.clone();
        match edit {
            DetailsEdit::AddLabel(label) => {
                if !next.labels.iter().any(|l| l.name.eq_ignore_ascii_case(&label.name)) {
                    next.labels.push(label.clone());
                }
            }
            DetailsEdit::RemoveLabel(name) => next.labels.retain(|l| !l.name.eq_ignore_ascii_case(name)),
            DetailsEdit::AddAssignee(login) => {
                if !next.assignees.iter().any(|a| a.eq_ignore_ascii_case(login)) {
                    next.assignees.push(login.clone());
                }
            }
            DetailsEdit::RemoveAssignee(login) => next.assignees.retain(|a| !a.eq_ignore_ascii_case(login)),
            DetailsEdit::AddReviewer(reviewer) => {
                if !next.requested_reviewers.iter().any(|r| r.login.eq_ignore_ascii_case(&reviewer.login)) {
                    next.requested_reviewers.push(reviewer.clone());
                }
            }
            DetailsEdit::RemoveReviewer(login) => {
                next.requested_reviewers.retain(|r| !r.login.eq_ignore_ascii_case(login));
            }
            DetailsEdit::SetBody(body) => next.body = body.clone(),
        }
        next
    }

    /// Review state of `login` among the latest reviews, if any.
    pub fn review_of(&self, login: &str) -> Option<&str> {
        self.reviews.iter().find(|(l, _)| l.eq_ignore_ascii_case(login)).map(|(_, s)| s.as_str())
    }
}

/// GitHub login syntax: alphanumerics and hyphens, at most 39 characters.
pub fn is_valid_login(login: &str) -> bool {
    !login.is_empty()
        && login.len() <= 39
        && login.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && login.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn invalid_json(error: impl std::fmt::Display) -> ForgeError {
    ForgeError::Failed(format!("Invalid gh output: {error}"))
}

fn str_of<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

/// Parses `gh pr view --json body,author,createdAt,labels,assignees,reviewRequests,latestReviews`.
pub fn parse_details(json: &str) -> Result<PrDetails, ForgeError> {
    let value: Value = serde_json::from_str(json).map_err(invalid_json)?;
    let list = |key: &str| value.get(key).and_then(Value::as_array).cloned().unwrap_or_default();
    let labels = list("labels")
        .iter()
        .filter_map(|l| {
            let name = str_of(l, "name");
            (!name.is_empty()).then(|| Label {
                name: name.to_owned(),
                color: str_of(l, "color").trim_start_matches('#').to_owned(),
            })
        })
        .collect();
    let assignees = list("assignees")
        .iter()
        .map(|a| str_of(a, "login").to_owned())
        .filter(|login| !login.is_empty())
        .collect();
    let requested_reviewers = list("reviewRequests")
        .iter()
        .filter_map(|r| {
            let login = str_of(r, "login");
            if !login.is_empty() {
                return Some(Reviewer { login: login.to_owned(), is_team: false });
            }
            let slug = [str_of(r, "slug"), str_of(r, "name")].into_iter().find(|s| !s.is_empty())?;
            let org = r.get("organization").map(|o| str_of(o, "login")).unwrap_or("");
            let login = if org.is_empty() { slug.to_owned() } else { format!("{org}/{slug}") };
            Some(Reviewer { login, is_team: true })
        })
        .collect();
    let mut reviews: Vec<(String, String)> = Vec::new();
    for review in list("latestReviews") {
        let login = review.get("author").map(|a| str_of(a, "login")).unwrap_or("");
        let state = str_of(&review, "state");
        if login.is_empty() || state.is_empty() {
            continue;
        }
        match reviews.iter_mut().find(|(l, _)| l == login) {
            Some(existing) => existing.1 = state.to_owned(),
            None => reviews.push((login.to_owned(), state.to_owned())),
        }
    }
    Ok(PrDetails {
        body: str_of(&value, "body").to_owned(),
        author: value.get("author").map(|a| str_of(a, "login")).unwrap_or("").to_owned(),
        created_at: str_of(&value, "createdAt").to_owned(),
        labels,
        assignees,
        requested_reviewers,
        reviews,
    })
}

/// Parses `name\tcolor` lines (the repository's labels).
pub fn parse_repo_labels(stdout: &str) -> Vec<Label> {
    stdout
        .lines()
        .filter_map(|line| {
            let (name, color) = line.split_once('\t').unwrap_or((line, ""));
            let name = name.trim();
            (!name.is_empty()).then(|| Label {
                name: name.to_owned(),
                color: color.trim().trim_start_matches('#').to_owned(),
            })
        })
        .collect()
}

/// Parses the `assignableUsers` GraphQL answer.
pub fn parse_assignable_users(json: &str) -> Result<Vec<RepoUser>, ForgeError> {
    let value: Value = serde_json::from_str(json).map_err(invalid_json)?;
    let nodes = value
        .pointer("/data/repository/assignableUsers/nodes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Ok(nodes
        .iter()
        .filter_map(|n| {
            let login = str_of(n, "login");
            (!login.is_empty()).then(|| RepoUser { login: login.to_owned(), name: str_of(n, "name").to_owned() })
        })
        .collect())
}

pub(super) fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_owned()).collect()
}

impl<E: GhExec> GitHub<E> {
    /// `gh api` prefix (with `--hostname` for Enterprise hosts) followed by `rest`.
    pub(super) fn api_args(&self, rest: &[&str]) -> Vec<String> {
        let mut args = argv(&["api"]);
        if self.repo().host != "github.com" {
            args.push("--hostname".to_owned());
            args.push(self.repo().host.clone());
        }
        args.extend(rest.iter().map(|part| (*part).to_owned()));
        args
    }

    pub(super) fn pr_path(&self, number: u64) -> String {
        format!("repos/{}/{}/pulls/{number}", self.repo().owner, self.repo().name)
    }

    pub fn pr_details(&self, number: u64) -> Result<PrDetails, ForgeError> {
        let output = self.run_ok(
            &argv(&["pr", "view", &number.to_string(), "--repo", &self.repo().gh_repo_arg(), "--json", DETAILS_FIELDS]),
            READ_TIMEOUT,
        )?;
        parse_details(&output.stdout)
    }

    /// Sets the title and description. The title is trimmed and must not be empty.
    pub fn edit_pr_text(&self, number: u64, title: &str, body: &str) -> Result<(), ForgeError> {
        let title = title.trim();
        if title.is_empty() {
            return Err(ForgeError::Failed("Title is empty".to_owned()));
        }
        let path = self.pr_path(number);
        let args = self.api_args(&["-X", "PATCH", &path, "-f", &format!("title={title}"), "-f", &format!("body={body}")]);
        self.run_ok(&args, WRITE_TIMEOUT).map(|_| ())
    }

    pub fn add_reviewers(&self, number: u64, reviewers: &[String]) -> Result<(), ForgeError> {
        self.change_reviewers(number, reviewers, true)
    }

    pub fn remove_reviewers(&self, number: u64, reviewers: &[String]) -> Result<(), ForgeError> {
        self.change_reviewers(number, reviewers, false)
    }

    fn change_reviewers(&self, number: u64, reviewers: &[String], add: bool) -> Result<(), ForgeError> {
        if reviewers.is_empty() || reviewers.iter().any(|r| r.trim().is_empty()) {
            return Err(ForgeError::Failed("Reviewer is empty".to_owned()));
        }
        if reviewers.iter().all(|r| is_valid_login(r)) {
            let path = format!("{}/requested_reviewers", self.pr_path(number));
            let method = if add { "POST" } else { "DELETE" };
            for login in reviewers {
                let args = self.api_args(&["-X", method, &path, "-f", &format!("reviewers[]={login}")]);
                self.run_ok(&args, WRITE_TIMEOUT)?;
            }
            return Ok(());
        }
        // Teams and unusual names: let `gh pr edit` resolve them.
        let flag = if add { "--add-reviewer" } else { "--remove-reviewer" };
        self.pr_edit(number, flag, &reviewers.join(","))
    }

    fn pr_edit(&self, number: u64, flag: &str, value: &str) -> Result<(), ForgeError> {
        if value.trim().is_empty() {
            return Err(ForgeError::Failed("Value is empty".to_owned()));
        }
        let args = argv(&["pr", "edit", &number.to_string(), "--repo", &self.repo().gh_repo_arg(), flag, value]);
        self.run_ok(&args, WRITE_TIMEOUT).map(|_| ())
    }

    pub fn add_label(&self, number: u64, label: &str) -> Result<(), ForgeError> {
        self.pr_edit(number, "--add-label", label)
    }

    pub fn remove_label(&self, number: u64, label: &str) -> Result<(), ForgeError> {
        self.pr_edit(number, "--remove-label", label)
    }

    pub fn add_assignee(&self, number: u64, login: &str) -> Result<(), ForgeError> {
        self.pr_edit(number, "--add-assignee", login)
    }

    pub fn remove_assignee(&self, number: u64, login: &str) -> Result<(), ForgeError> {
        self.pr_edit(number, "--remove-assignee", login)
    }

    /// Sends one edit to GitHub (`SetBody` goes through `edit_pr_text`).
    pub fn apply_edit(&self, number: u64, edit: &DetailsEdit) -> Result<(), ForgeError> {
        match edit {
            DetailsEdit::AddLabel(label) => self.add_label(number, &label.name),
            DetailsEdit::RemoveLabel(name) => self.remove_label(number, name),
            DetailsEdit::AddAssignee(login) => self.add_assignee(number, login),
            DetailsEdit::RemoveAssignee(login) => self.remove_assignee(number, login),
            DetailsEdit::AddReviewer(reviewer) => self.add_reviewers(number, std::slice::from_ref(&reviewer.login)),
            DetailsEdit::RemoveReviewer(login) => self.remove_reviewers(number, std::slice::from_ref(login)),
            DetailsEdit::SetBody(_) => Err(ForgeError::Unsupported),
        }
    }

    /// All labels of the repository (name and colour).
    pub fn repo_labels(&self) -> Result<Vec<Label>, ForgeError> {
        let path = format!("repos/{}/{}/labels?per_page=100", self.repo().owner, self.repo().name);
        let args = self.api_args(&["--paginate", &path, "--jq", ".[] | [.name, .color] | @tsv"]);
        let output: GhOutput = self.run_ok(&args, READ_TIMEOUT)?;
        Ok(parse_repo_labels(&output.stdout))
    }

    /// Users that can be assigned or asked for a review (first 100).
    pub fn assignable_users(&self) -> Result<Vec<RepoUser>, ForgeError> {
        let query = "query($owner:String!,$name:String!){repository(owner:$owner,name:$name){assignableUsers(first:100){nodes{login name}}}}";
        let args = self.api_args(&[
            "graphql",
            "-f",
            &format!("query={query}"),
            "-f",
            &format!("owner={}", self.repo().owner),
            "-f",
            &format!("name={}", self.repo().name),
        ]);
        let output = self.run_ok(&args, READ_TIMEOUT)?;
        parse_assignable_users(&output.stdout)
    }
}

#[cfg(test)]
pub(super) mod testutil {
    use std::collections::VecDeque;
    use std::path::Path;
    use std::sync::Mutex;
    use std::time::Duration;

    use super::super::gh::{GhExec, GhOutput};
    use super::super::ForgeError;

    #[derive(Default)]
    pub struct FakeExec {
        pub calls: Mutex<Vec<Vec<String>>>,
        pub responses: Mutex<VecDeque<Result<GhOutput, ForgeError>>>,
    }

    impl FakeExec {
        pub fn with(responses: Vec<Result<GhOutput, ForgeError>>) -> Self {
            Self { responses: Mutex::new(responses.into()), ..Self::default() }
        }

        pub fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl GhExec for &FakeExec {
        fn run(&self, _: &Path, args: &[String], _: Duration) -> Result<GhOutput, ForgeError> {
            self.calls.lock().unwrap().push(args.to_vec());
            self.responses.lock().unwrap().pop_front().expect("unscripted gh call")
        }
    }

    pub fn ok(stdout: &str) -> Result<GhOutput, ForgeError> {
        Ok(GhOutput { code: Some(0), stdout: stdout.into(), stderr: String::new() })
    }

    pub fn fail(stderr: &str) -> Result<GhOutput, ForgeError> {
        Ok(GhOutput { code: Some(1), stdout: String::new(), stderr: stderr.into() })
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::testutil::*;
    use super::*;
    use crate::forge::RemoteRepo;

    fn github(exec: &FakeExec) -> GitHub<&FakeExec> {
        GitHub::with_exec(
            exec,
            PathBuf::from("."),
            RemoteRepo { host: "github.com".into(), owner: "o".into(), name: "r".into() },
        )
    }

    fn strs(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|p| (*p).to_owned()).collect()
    }

    const DETAILS: &str = r#"{
        "body": "Hello\n- item",
        "author": {"login": "alice"},
        "createdAt": "2026-01-02T03:04:05Z",
        "labels": [{"name": "bug", "color": "d73a4a"}, {"name": "", "color": "x"}],
        "assignees": [{"login": "bob"}],
        "reviewRequests": [{"__typename": "User", "login": "carol"}, {"__typename": "Team", "name": "Core", "slug": "core", "organization": {"login": "acme"}}],
        "latestReviews": [{"author": {"login": "dave"}, "state": "APPROVED"}, {"author": {"login": "dave"}, "state": "COMMENTED"}, {"author": null, "state": "APPROVED"}]
    }"#;

    #[test]
    fn parses_details() {
        let d = parse_details(DETAILS).unwrap();
        assert_eq!(d.body, "Hello\n- item");
        assert_eq!(d.author, "alice");
        assert_eq!(d.labels, vec![Label { name: "bug".into(), color: "d73a4a".into() }]);
        assert_eq!(d.assignees, vec!["bob"]);
        assert_eq!(
            d.requested_reviewers,
            vec![
                Reviewer { login: "carol".into(), is_team: false },
                Reviewer { login: "acme/core".into(), is_team: true }
            ]
        );
        assert_eq!(d.reviews, vec![("dave".to_owned(), "COMMENTED".to_owned())]);
        assert_eq!(d.review_of("DAVE"), Some("COMMENTED"));
    }

    #[test]
    fn parsing_is_tolerant_and_rejects_garbage() {
        assert_eq!(parse_details("{}").unwrap(), PrDetails::default());
        assert!(parse_details("nope").is_err());
        assert!(parse_assignable_users("[").is_err());
        assert!(parse_assignable_users("{}").unwrap().is_empty());
    }

    #[test]
    fn parses_users_and_labels() {
        let users = parse_assignable_users(
            r#"{"data":{"repository":{"assignableUsers":{"nodes":[{"login":"a","name":"A"},{"login":"b","name":null}]}}}}"#,
        )
        .unwrap();
        assert_eq!(
            users,
            vec![RepoUser { login: "a".into(), name: "A".into() }, RepoUser { login: "b".into(), name: String::new() }]
        );
        assert_eq!(
            parse_repo_labels("bug\td73a4a\n\nwip\n"),
            vec![Label { name: "bug".into(), color: "d73a4a".into() }, Label { name: "wip".into(), color: String::new() }]
        );
    }

    #[test]
    fn login_validation() {
        for ok in ["a", "octo-cat", "A1", &"a".repeat(39)] {
            assert!(is_valid_login(ok), "{ok}");
        }
        for bad in ["", "-a", "a b", "org/team", "a_b", &"a".repeat(40)] {
            assert!(!is_valid_login(bad), "{bad}");
        }
    }

    #[test]
    fn details_call_argv() {
        let exec = FakeExec::with(vec![ok(DETAILS)]);
        github(&exec).pr_details(7).unwrap();
        assert_eq!(exec.calls(), vec![strs(&["pr", "view", "7", "--repo", "o/r", "--json", DETAILS_FIELDS])]);
    }

    #[test]
    fn edit_text_sends_each_text_as_one_argument() {
        let exec = FakeExec::with(vec![ok("")]);
        github(&exec).edit_pr_text(3, "  -dash title  ", "line1\nline2\n- bullet\n").unwrap();
        assert_eq!(
            exec.calls(),
            vec![strs(&["api", "-X", "PATCH", "repos/o/r/pulls/3", "-f", "title=-dash title", "-f", "body=line1\nline2\n- bullet\n"])]
        );
    }

    #[test]
    fn empty_title_is_rejected_without_calling_gh() {
        let exec = FakeExec::with(vec![]);
        assert!(github(&exec).edit_pr_text(3, "   ", "b").is_err());
        assert!(exec.calls().is_empty());
    }

    #[test]
    fn reviewers_use_the_api_one_call_per_login() {
        let exec = FakeExec::with(vec![ok(""), ok(""), ok("")]);
        let gh = github(&exec);
        gh.add_reviewers(5, &["alice".into(), "bob".into()]).unwrap();
        gh.remove_reviewers(5, &["alice".into()]).unwrap();
        assert_eq!(
            exec.calls(),
            vec![
                strs(&["api", "-X", "POST", "repos/o/r/pulls/5/requested_reviewers", "-f", "reviewers[]=alice"]),
                strs(&["api", "-X", "POST", "repos/o/r/pulls/5/requested_reviewers", "-f", "reviewers[]=bob"]),
                strs(&["api", "-X", "DELETE", "repos/o/r/pulls/5/requested_reviewers", "-f", "reviewers[]=alice"]),
            ]
        );
    }

    #[test]
    fn invalid_logins_fall_back_to_pr_edit() {
        let exec = FakeExec::with(vec![ok(""), ok("")]);
        let gh = github(&exec);
        gh.add_reviewers(5, &["alice".into(), "acme/core".into()]).unwrap();
        gh.remove_reviewers(5, &["acme/core".into()]).unwrap();
        assert_eq!(
            exec.calls(),
            vec![
                strs(&["pr", "edit", "5", "--repo", "o/r", "--add-reviewer", "alice,acme/core"]),
                strs(&["pr", "edit", "5", "--repo", "o/r", "--remove-reviewer", "acme/core"]),
            ]
        );
        assert!(gh.add_reviewers(5, &[]).is_err());
        assert!(gh.add_reviewers(5, &[" ".into()]).is_err());
    }

    #[test]
    fn labels_and_assignees_use_pr_edit() {
        let exec = FakeExec::with(vec![ok(""), ok(""), ok(""), ok("")]);
        let gh = github(&exec);
        gh.add_label(2, "good first issue").unwrap();
        gh.remove_label(2, "-odd").unwrap();
        gh.add_assignee(2, "bob").unwrap();
        gh.remove_assignee(2, "bob").unwrap();
        assert!(gh.add_label(2, " ").is_err());
        let calls = exec.calls();
        assert_eq!(calls.len(), 4);
        assert_eq!(calls[0], strs(&["pr", "edit", "2", "--repo", "o/r", "--add-label", "good first issue"]));
        assert_eq!(calls[1], strs(&["pr", "edit", "2", "--repo", "o/r", "--remove-label", "-odd"]));
        assert_eq!(calls[2][5], "--add-assignee");
        assert_eq!(calls[3][5], "--remove-assignee");
    }

    #[test]
    fn failures_are_reported() {
        let exec = FakeExec::with(vec![fail("boom")]);
        assert_eq!(github(&exec).add_label(1, "x"), Err(ForgeError::Failed("boom".into())));
    }

    #[test]
    fn repo_lists_argv() {
        let exec = FakeExec::with(vec![
            ok("a\tff0000\n"),
            ok(r#"{"data":{"repository":{"assignableUsers":{"nodes":[]}}}}"#),
        ]);
        let gh = github(&exec);
        assert_eq!(gh.repo_labels().unwrap().len(), 1);
        gh.assignable_users().unwrap();
        let calls = exec.calls();
        assert_eq!(calls[0][..3], strs(&["api", "--paginate", "repos/o/r/labels?per_page=100"])[..]);
        assert_eq!(calls[1][..2], strs(&["api", "graphql"])[..]);
        assert!(calls[1].contains(&"owner=o".to_owned()) && calls[1].contains(&"name=r".to_owned()));
    }

    #[test]
    fn enterprise_hosts_pass_hostname() {
        let exec = FakeExec::with(vec![ok("")]);
        let gh = GitHub::with_exec(
            &exec,
            PathBuf::from("."),
            RemoteRepo { host: "ghe.example.com".into(), owner: "o".into(), name: "r".into() },
        );
        gh.edit_pr_text(1, "t", "b").unwrap();
        assert_eq!(exec.calls()[0][..3], strs(&["api", "--hostname", "ghe.example.com"])[..]);
    }

    #[test]
    fn optimistic_edits_apply_and_roll_back() {
        let base = parse_details(DETAILS).unwrap();
        let label = Label { name: "feature".into(), color: "00ff00".into() };
        let added = base.with_edit(&DetailsEdit::AddLabel(label.clone()));
        assert_eq!(added.labels.len(), 2);
        assert_eq!(added.with_edit(&DetailsEdit::AddLabel(label)).labels.len(), 2);
        assert_eq!(added.with_edit(&DetailsEdit::RemoveLabel("BUG".into())).labels.len(), 1);
        assert_eq!(base.with_edit(&DetailsEdit::AddAssignee("eve".into())).assignees, vec!["bob", "eve"]);
        assert!(base.with_edit(&DetailsEdit::RemoveAssignee("Bob".into())).assignees.is_empty());
        let r = Reviewer { login: "zed".into(), is_team: false };
        assert_eq!(base.with_edit(&DetailsEdit::AddReviewer(r)).requested_reviewers.len(), 3);
        assert_eq!(base.with_edit(&DetailsEdit::RemoveReviewer("carol".into())).requested_reviewers.len(), 1);
        assert_eq!(base.with_edit(&DetailsEdit::SetBody("x".into())).body, "x");
        // Rollback is the untouched original.
        assert_eq!(base.labels.len(), 1);
    }
}
