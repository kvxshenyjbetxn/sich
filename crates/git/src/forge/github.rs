use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use super::gh::{GhCli, GhExec, GhOutput, classify};
use super::model::{PullRequest, parse_pr_json};
use super::remote::RemoteRepo;
use super::ForgeError;

const PR_FIELDS: &str = "number,title,url,state,isDraft,mergeable,mergeStateStatus,reviewDecision,baseRefName,headRefName,headRefOid,statusCheckRollup,updatedAt";
const AUTH_TIMEOUT: Duration = Duration::from_secs(15);
const VIEW_TIMEOUT: Duration = Duration::from_secs(30);
const WRITE_TIMEOUT: Duration = Duration::from_secs(90);
const LOG_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeMethod {
    Squash,
    Merge,
    Rebase,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreatePr {
    pub title: String,
    pub body: String,
    pub base: String,
    pub head: String,
    pub draft: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CreatedPr {
    Created { url: String },
    Existing(PullRequest),
}

pub struct GitHub<E: GhExec = GhCli> {
    exec: E,
    dir: PathBuf,
    repo: RemoteRepo,
}

impl GitHub<GhCli> {
    pub fn new(dir: PathBuf, repo: RemoteRepo) -> Self {
        Self::with_exec(GhCli, dir, repo)
    }
}

fn args(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_owned()).collect()
}

impl<E: GhExec> GitHub<E> {
    pub fn with_exec(exec: E, dir: PathBuf, repo: RemoteRepo) -> Self {
        Self { exec, dir, repo }
    }

    pub fn repo(&self) -> &RemoteRepo {
        &self.repo
    }

    fn run(&self, args: &[String], timeout: Duration) -> Result<GhOutput, ForgeError> {
        self.exec.run(&self.dir, args, timeout)
    }

    /// Runs gh and turns any non-zero exit into an error.
    fn run_ok(&self, args: &[String], timeout: Duration) -> Result<GhOutput, ForgeError> {
        let output = self.run(args, timeout)?;
        if output.code == Some(0) {
            Ok(output)
        } else {
            Err(classify(&args[0], &output))
        }
    }

    pub fn auth_status(&self) -> Result<(), ForgeError> {
        let output = self.run(
            &args(&["auth", "status", "--hostname", &self.repo.host]),
            AUTH_TIMEOUT,
        )?;
        if output.code == Some(0) {
            Ok(())
        } else {
            Err(ForgeError::NotAuthenticated)
        }
    }

    pub fn pr_for_branch(&self, branch: &str) -> Result<Option<PullRequest>, ForgeError> {
        if branch.trim().is_empty() {
            return Err(ForgeError::NoBranch);
        }
        if branch.starts_with('-') {
            return Err(ForgeError::Failed(format!("Invalid branch name: {branch:?}")));
        }
        let output = self.run(
            &args(&[
                "pr",
                "view",
                branch,
                "--repo",
                &self.repo.gh_repo_arg(),
                "--json",
                PR_FIELDS,
            ]),
            VIEW_TIMEOUT,
        )?;
        if output.code == Some(0) {
            return parse_pr_json(&output.stdout).map(Some);
        }
        if output.stderr.to_ascii_lowercase().contains("no pull requests found") {
            return Ok(None);
        }
        Err(classify("pr", &output))
    }

    pub fn create_pr(&self, req: &CreatePr) -> Result<CreatedPr, ForgeError> {
        let invalid = |message: &str| Err(ForgeError::Failed(message.to_owned()));
        if req.title.trim().is_empty() {
            return invalid("Title is empty");
        }
        if req.base.trim().is_empty() || req.head.trim().is_empty() {
            return invalid("Base and head branches are required");
        }
        if req.base.starts_with('-') || req.head.starts_with('-') {
            return invalid("Invalid branch name");
        }
        if req.base == req.head {
            return invalid("Base and head branches must differ");
        }
        let mut file = tempfile::Builder::new()
            .prefix("sich-pr-")
            .suffix(".md")
            .tempfile()
            .map_err(|error| ForgeError::Failed(format!("Failed to write PR body: {error}")))?;
        file.write_all(req.body.as_bytes())
            .and_then(|()| file.flush())
            .map_err(|error| ForgeError::Failed(format!("Failed to write PR body: {error}")))?;
        // Close our handle (the path guard still deletes the file on drop).
        let path = file.into_temp_path();
        let title = format!("--title={}", req.title);
        let mut argv = args(&[
            "pr",
            "create",
            "--repo",
            &self.repo.gh_repo_arg(),
            "--base",
            &req.base,
            "--head",
            &req.head,
            &title,
            "--body-file",
        ]);
        argv.push(path.to_string_lossy().into_owned());
        if req.draft {
            argv.push("--draft".to_owned());
        }
        let output = self.run(&argv, WRITE_TIMEOUT);
        drop(path);
        let output = output?;
        if output.code == Some(0) {
            let url = output
                .stdout
                .lines()
                .map(str::trim)
                .rev()
                .find(|line| line.starts_with("http"));
            if let Some(url) = url {
                return Ok(CreatedPr::Created { url: url.to_owned() });
            }
        } else if !output.stderr.to_ascii_lowercase().contains("already exists") {
            return Err(classify("pr", &output));
        }
        match self.pr_for_branch(&req.head)? {
            Some(pr) => Ok(CreatedPr::Existing(pr)),
            None => Err(ForgeError::Failed(
                "gh pr create did not return a pull request URL".to_owned(),
            )),
        }
    }

    pub fn merge_pr(
        &self,
        number: u64,
        method: MergeMethod,
        head_oid: &str,
    ) -> Result<(), ForgeError> {
        let flag = match method {
            MergeMethod::Squash => "--squash",
            MergeMethod::Merge => "--merge",
            MergeMethod::Rebase => "--rebase",
        };
        let mut argv = args(&["pr", "merge", &number.to_string(), "--repo", &self.repo.gh_repo_arg(), flag]);
        if !head_oid.is_empty() {
            if head_oid.starts_with('-') {
                return Err(ForgeError::Failed("Invalid head commit".to_owned()));
            }
            argv.push("--match-head-commit".to_owned());
            argv.push(head_oid.to_owned());
        }
        self.run_ok(&argv, WRITE_TIMEOUT).map(|_| ())
    }

    pub fn set_ready(&self, number: u64, ready: bool) -> Result<(), ForgeError> {
        let mut argv = args(&["pr", "ready", &number.to_string(), "--repo", &self.repo.gh_repo_arg()]);
        if !ready {
            argv.push("--undo".to_owned());
        }
        self.run_ok(&argv, VIEW_TIMEOUT).map(|_| ())
    }

    pub fn close_pr(&self, number: u64) -> Result<(), ForgeError> {
        let argv = args(&["pr", "close", &number.to_string(), "--repo", &self.repo.gh_repo_arg()]);
        self.run_ok(&argv, VIEW_TIMEOUT).map(|_| ())
    }

    pub fn reopen_pr(&self, number: u64) -> Result<(), ForgeError> {
        let argv = args(&["pr", "reopen", &number.to_string(), "--repo", &self.repo.gh_repo_arg()]);
        self.run_ok(&argv, VIEW_TIMEOUT).map(|_| ())
    }

    pub fn rerun_failed(&self, run_ids: &[u64]) -> Result<(), ForgeError> {
        for id in run_ids {
            let argv = args(&[
                "run",
                "rerun",
                &id.to_string(),
                "--failed",
                "--repo",
                &self.repo.gh_repo_arg(),
            ]);
            self.run_ok(&argv, VIEW_TIMEOUT)?;
        }
        Ok(())
    }

    /// Output of the failed steps of one job (`gh run view --job <id> --log-failed`).
    pub fn failed_job_log(&self, job_id: u64) -> Result<String, ForgeError> {
        let argv = args(&[
            "run",
            "view",
            "--job",
            &job_id.to_string(),
            "--log-failed",
            "--repo",
            &self.repo.gh_repo_arg(),
        ]);
        self.run_ok(&argv, LOG_TIMEOUT).map(|output| output.stdout)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::path::Path;
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct FakeExec {
        calls: Mutex<Vec<Vec<String>>>,
        responses: Mutex<VecDeque<Result<GhOutput, ForgeError>>>,
        /// (path, content at call time) of the last `--body-file` argument.
        body_file: Mutex<Option<(PathBuf, String)>>,
    }

    impl FakeExec {
        fn with(responses: Vec<Result<GhOutput, ForgeError>>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
                ..Self::default()
            }
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl GhExec for &FakeExec {
        fn run(&self, _: &Path, args: &[String], _: Duration) -> Result<GhOutput, ForgeError> {
            self.calls.lock().unwrap().push(args.to_vec());
            if let Some(index) = args.iter().position(|arg| arg == "--body-file") {
                let path = PathBuf::from(&args[index + 1]);
                let content = std::fs::read_to_string(&path).unwrap();
                *self.body_file.lock().unwrap() = Some((path, content));
            }
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("unscripted gh call")
        }
    }

    fn ok(stdout: &str) -> Result<GhOutput, ForgeError> {
        Ok(GhOutput {
            code: Some(0),
            stdout: stdout.into(),
            stderr: String::new(),
        })
    }

    fn fail(code: i32, stderr: &str) -> Result<GhOutput, ForgeError> {
        Ok(GhOutput {
            code: Some(code),
            stdout: String::new(),
            stderr: stderr.into(),
        })
    }

    fn github(exec: &FakeExec) -> GitHub<&FakeExec> {
        GitHub::with_exec(
            exec,
            PathBuf::from("."),
            RemoteRepo::parse("https://github.com/o/r.git").unwrap(),
        )
    }

    fn ghe(exec: &FakeExec) -> GitHub<&FakeExec> {
        GitHub::with_exec(
            exec,
            PathBuf::from("."),
            RemoteRepo::parse("git@github.acme.com:o/r.git").unwrap(),
        )
    }

    fn request() -> CreatePr {
        CreatePr {
            title: "Title".into(),
            body: "Body\n\nmore ✓".into(),
            base: "main".into(),
            head: "feature".into(),
            draft: false,
        }
    }

    fn v(parts: &[&str]) -> Vec<String> {
        args(parts)
    }

    #[test]
    fn auth_status_argv_and_failure() {
        let exec = FakeExec::with(vec![ok(""), fail(1, "x")]);
        let gh = ghe(&exec);
        assert_eq!(gh.auth_status(), Ok(()));
        assert_eq!(gh.auth_status(), Err(ForgeError::NotAuthenticated));
        assert_eq!(
            exec.calls()[0],
            v(&["auth", "status", "--hostname", "github.acme.com"])
        );
    }

    #[test]
    fn pr_for_branch_argv_parses_and_handles_missing() {
        let exec = FakeExec::with(vec![
            ok(r#"{"number":7,"headRefName":"feature"}"#),
            fail(1, "no pull requests found for branch \"feature\""),
            fail(4, "try authenticating with: gh auth login"),
        ]);
        let gh = github(&exec);
        let pr = gh.pr_for_branch("feature").unwrap().unwrap();
        assert_eq!(pr.number, 7);
        assert_eq!(gh.pr_for_branch("feature"), Ok(None));
        assert_eq!(gh.pr_for_branch("feature"), Err(ForgeError::NotAuthenticated));
        assert_eq!(
            exec.calls()[0],
            v(&["pr", "view", "feature", "--repo", "o/r", "--json", PR_FIELDS])
        );
    }

    #[test]
    fn pr_for_branch_validates_without_running_gh() {
        let exec = FakeExec::default();
        let gh = github(&exec);
        assert_eq!(gh.pr_for_branch(""), Err(ForgeError::NoBranch));
        assert!(matches!(gh.pr_for_branch("--web"), Err(ForgeError::Failed(_))));
        assert!(exec.calls().is_empty());
    }

    #[test]
    fn create_pr_argv_body_file_and_cleanup() {
        let exec = FakeExec::with(vec![ok("Creating pull request\nhttps://github.com/o/r/pull/9\n")]);
        let gh = github(&exec);
        let result = gh.create_pr(&request()).unwrap();
        assert_eq!(
            result,
            CreatedPr::Created {
                url: "https://github.com/o/r/pull/9".into()
            }
        );
        let (path, content) = exec.body_file.lock().unwrap().clone().unwrap();
        assert_eq!(content, "Body\n\nmore ✓");
        assert!(!path.exists());
        let call = &exec.calls()[0];
        assert_eq!(
            call[..10],
            v(&[
                "pr", "create", "--repo", "o/r", "--base", "main", "--head", "feature",
                "--title=Title", "--body-file"
            ])[..]
        );
        assert_eq!(call.len(), 11);
    }

    #[test]
    fn create_pr_keeps_titles_that_start_with_a_dash() {
        let exec = FakeExec::with(vec![ok("https://github.com/o/r/pull/9")]);
        let gh = github(&exec);
        let mut req = request();
        req.title = "- fix typo".into();
        gh.create_pr(&req).unwrap();
        assert_eq!(exec.calls()[0][8], "--title=- fix typo");
    }

    #[test]
    fn create_pr_draft_flag_and_ghe_repo() {
        let exec = FakeExec::with(vec![ok("https://github.acme.com/o/r/pull/1")]);
        let gh = ghe(&exec);
        let mut req = request();
        req.draft = true;
        gh.create_pr(&req).unwrap();
        let call = &exec.calls()[0];
        assert_eq!(call[3], "github.acme.com/o/r");
        assert_eq!(call.last().unwrap(), "--draft");
    }

    #[test]
    fn create_pr_falls_back_to_existing() {
        let exec = FakeExec::with(vec![
            fail(1, "a pull request for branch \"feature\" into branch \"main\" already exists:\nhttps://github.com/o/r/pull/3"),
            ok(r#"{"number":3,"headRefName":"feature"}"#),
        ]);
        let gh = github(&exec);
        match gh.create_pr(&request()).unwrap() {
            CreatedPr::Existing(pr) => assert_eq!(pr.number, 3),
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(exec.calls()[1][..3], v(&["pr", "view", "feature"])[..]);
        let (path, _) = exec.body_file.lock().unwrap().clone().unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn create_pr_without_url_looks_up_or_fails() {
        let exec = FakeExec::with(vec![
            ok("done"),
            fail(1, "no pull requests found"),
        ]);
        let gh = github(&exec);
        assert!(matches!(gh.create_pr(&request()), Err(ForgeError::Failed(_))));
    }

    #[test]
    fn create_pr_other_failures_are_classified() {
        let exec = FakeExec::with(vec![fail(1, "GraphQL: boom"), fail(4, "")]);
        let gh = github(&exec);
        assert_eq!(
            gh.create_pr(&request()),
            Err(ForgeError::Failed("GraphQL: boom".into()))
        );
        assert_eq!(gh.create_pr(&request()), Err(ForgeError::NotAuthenticated));
    }

    #[test]
    fn create_pr_validation_does_not_call_gh() {
        let exec = FakeExec::default();
        let gh = github(&exec);
        let cases: Vec<Box<dyn Fn(&mut CreatePr)>> = vec![
            Box::new(|r| r.title = "  ".into()),
            Box::new(|r| r.base = String::new()),
            Box::new(|r| r.head = " ".into()),
            Box::new(|r| r.head = "main".into()),
            Box::new(|r| r.base = "--web".into()),
            Box::new(|r| r.head = "-x".into()),
        ];
        for change in cases {
            let mut req = request();
            change(&mut req);
            assert!(matches!(gh.create_pr(&req), Err(ForgeError::Failed(_))));
        }
        assert!(exec.calls().is_empty());
    }

    #[test]
    fn merge_pr_argv_per_method() {
        let exec = FakeExec::with(vec![ok(""), ok(""), ok("")]);
        let gh = github(&exec);
        for method in [MergeMethod::Squash, MergeMethod::Merge, MergeMethod::Rebase] {
            gh.merge_pr(5, method, "abc").unwrap();
        }
        let calls = exec.calls();
        for (call, flag) in calls.iter().zip(["--squash", "--merge", "--rebase"]) {
            assert_eq!(
                *call,
                v(&["pr", "merge", "5", "--repo", "o/r", flag, "--match-head-commit", "abc"])
            );
            assert!(!call.iter().any(|arg| arg == "--delete-branch"));
        }
    }

    #[test]
    fn merge_pr_failure_is_reported() {
        let exec = FakeExec::with(vec![fail(1, "Pull request is not mergeable")]);
        assert_eq!(
            github(&exec).merge_pr(5, MergeMethod::Merge, "abc"),
            Err(ForgeError::Failed("Pull request is not mergeable".into()))
        );
    }

    #[test]
    fn ready_close_reopen_argv() {
        let exec = FakeExec::with(vec![ok(""), ok(""), ok(""), ok("")]);
        let gh = github(&exec);
        gh.set_ready(2, true).unwrap();
        gh.set_ready(2, false).unwrap();
        gh.close_pr(2).unwrap();
        gh.reopen_pr(2).unwrap();
        let calls = exec.calls();
        assert_eq!(calls[0], v(&["pr", "ready", "2", "--repo", "o/r"]));
        assert_eq!(calls[1], v(&["pr", "ready", "2", "--repo", "o/r", "--undo"]));
        assert_eq!(calls[2], v(&["pr", "close", "2", "--repo", "o/r"]));
        assert_eq!(calls[3], v(&["pr", "reopen", "2", "--repo", "o/r"]));
    }

    #[test]
    fn rerun_failed_runs_each_id_and_stops_on_error() {
        let exec = FakeExec::with(vec![ok(""), fail(1, "run not found"), ok("")]);
        let gh = github(&exec);
        let result = gh.rerun_failed(&[10, 20, 30]);
        assert_eq!(result, Err(ForgeError::Failed("run not found".into())));
        let calls = exec.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0], v(&["run", "rerun", "10", "--failed", "--repo", "o/r"]));
        assert_eq!(calls[1][2], "20");
        assert_eq!(gh.rerun_failed(&[]), Ok(()));
    }

    #[test]
    fn failed_job_log_argv_and_errors() {
        let exec = FakeExec::with(vec![ok("log text"), fail(1, "log not found"), Err(ForgeError::TimedOut)]);
        let gh = github(&exec);
        assert_eq!(gh.failed_job_log(77), Ok("log text".to_owned()));
        assert_eq!(
            gh.failed_job_log(78),
            Err(ForgeError::Failed("log not found".into()))
        );
        assert_eq!(gh.failed_job_log(79), Err(ForgeError::TimedOut));
        assert_eq!(
            exec.calls()[0],
            v(&["run", "view", "--job", "77", "--log-failed", "--repo", "o/r"])
        );
    }

    #[test]
    fn exec_errors_propagate() {
        let exec = FakeExec::with(vec![Err(ForgeError::GhMissing), Err(ForgeError::TimedOut)]);
        let gh = github(&exec);
        assert_eq!(gh.pr_for_branch("a"), Err(ForgeError::GhMissing));
        assert_eq!(gh.close_pr(1), Err(ForgeError::TimedOut));
    }
}
