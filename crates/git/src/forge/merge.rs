//! Merge metadata of a repository (which merge methods are allowed) and auto-merge.
use serde::Deserialize;

use super::github::{MergeMethod, VIEW_TIMEOUT, WRITE_TIMEOUT};
use super::{ForgeError, GhExec, GitHub};

const OPTIONS_QUERY: &str = "query($owner:String!,$repo:String!){ repository(owner:$owner,name:$repo){ viewerDefaultMergeMethod mergeCommitAllowed rebaseMergeAllowed squashMergeAllowed autoMergeAllowed } }";
const ENABLE_MUTATION: &str = "mutation($pullRequestId:ID!,$mergeMethod:PullRequestMergeMethod!,$expectedHeadOid:GitObjectID){ enablePullRequestAutoMerge(input:{pullRequestId:$pullRequestId,mergeMethod:$mergeMethod,expectedHeadOid:$expectedHeadOid}){pullRequest{id}} }";
/// What the user sees when GitHub refuses auto-merge because the PR can be merged right away.
pub const ALREADY_MERGEABLE: &str = "Already mergeable - merge directly";

/// Which merge methods the repository allows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeOptions {
    /// In the order squash, merge, rebase.
    pub allowed: Vec<MergeMethod>,
    /// The viewer's default method, when it is allowed.
    pub default: Option<MergeMethod>,
    pub auto_merge_allowed: bool,
}

impl MergeOptions {
    pub fn allows(&self, method: MergeMethod) -> bool {
        self.allowed.contains(&method)
    }

    /// `preferred` when allowed, else the repository default, else the first allowed method.
    pub fn pick(&self, preferred: MergeMethod) -> Option<MergeMethod> {
        if self.allows(preferred) {
            Some(preferred)
        } else {
            self.default.or_else(|| self.allowed.first().copied())
        }
    }
}

impl MergeMethod {
    /// The `PullRequestMergeMethod` GraphQL enum value.
    pub fn graphql(self) -> &'static str {
        match self {
            Self::Squash => "SQUASH",
            Self::Merge => "MERGE",
            Self::Rebase => "REBASE",
        }
    }

    /// Parses `SQUASH` / `MERGE` / `REBASE` (any case).
    pub fn from_graphql(value: &str) -> Option<Self> {
        match value.trim().to_ascii_uppercase().as_str() {
            "SQUASH" => Some(Self::Squash),
            "MERGE" => Some(Self::Merge),
            "REBASE" => Some(Self::Rebase),
            _ => None,
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Response {
    data: Option<Data>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Data {
    repository: Option<Raw>,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Raw {
    viewer_default_merge_method: Option<String>,
    merge_commit_allowed: bool,
    rebase_merge_allowed: bool,
    squash_merge_allowed: bool,
    auto_merge_allowed: bool,
}

pub fn parse_merge_options(json: &str) -> Result<MergeOptions, ForgeError> {
    let response: Response = serde_json::from_str(json)
        .map_err(|error| ForgeError::Failed(format!("Invalid gh output: {error}")))?;
    let raw = response
        .data
        .and_then(|data| data.repository)
        .ok_or_else(|| ForgeError::Failed("Repository not found".to_owned()))?;
    let mut allowed = Vec::new();
    for (flag, method) in [
        (raw.squash_merge_allowed, MergeMethod::Squash),
        (raw.merge_commit_allowed, MergeMethod::Merge),
        (raw.rebase_merge_allowed, MergeMethod::Rebase),
    ] {
        if flag {
            allowed.push(method);
        }
    }
    let default = raw
        .viewer_default_merge_method
        .as_deref()
        .and_then(MergeMethod::from_graphql)
        .filter(|method| allowed.contains(method));
    Ok(MergeOptions { allowed, default, auto_merge_allowed: raw.auto_merge_allowed })
}

/// Maps a failed auto-merge call: "clean status" means the PR can simply be merged.
pub fn map_auto_merge_error(error: ForgeError) -> ForgeError {
    match &error {
        ForgeError::Failed(message) if message.to_ascii_lowercase().contains("clean status") => {
            ForgeError::Failed(ALREADY_MERGEABLE.to_owned())
        }
        _ => error,
    }
}

impl<E: GhExec> GitHub<E> {
    pub fn merge_options(&self) -> Result<MergeOptions, ForgeError> {
        let repo = self.repo();
        let argv = self.api_argv(&[
            "graphql",
            "-f",
            &format!("query={OPTIONS_QUERY}"),
            "-f",
            &format!("owner={}", repo.owner),
            "-f",
            &format!("repo={}", repo.name),
        ]);
        parse_merge_options(&self.run_ok(&argv, VIEW_TIMEOUT)?.stdout)
    }

    /// Enables auto-merge of the pull request with the node id `node_id`.
    pub fn enable_auto_merge(&self, node_id: &str, method: MergeMethod, head_oid: &str) -> Result<(), ForgeError> {
        if node_id.is_empty() || node_id.starts_with('-') || node_id.starts_with('@') {
            return Err(ForgeError::Failed("Unknown pull request id".to_owned()));
        }
        if head_oid.starts_with(['-', '@']) {
            return Err(ForgeError::Failed("Invalid head commit".to_owned()));
        }
        let mut parts = vec![
            "graphql".to_owned(),
            "-f".to_owned(),
            format!("query={ENABLE_MUTATION}"),
            "-f".to_owned(),
            format!("pullRequestId={node_id}"),
            "-f".to_owned(),
            format!("mergeMethod={}", method.graphql()),
        ];
        if !head_oid.is_empty() {
            parts.push("-f".to_owned());
            parts.push(format!("expectedHeadOid={head_oid}"));
        }
        let parts: Vec<&str> = parts.iter().map(String::as_str).collect();
        self.run_ok(&self.api_argv(&parts), WRITE_TIMEOUT).map(|_| ()).map_err(map_auto_merge_error)
    }

    pub fn disable_auto_merge(&self, number: u64) -> Result<(), ForgeError> {
        let argv = vec![
            "pr".to_owned(),
            "merge".to_owned(),
            number.to_string(),
            "--disable-auto".to_owned(),
            "--repo".to_owned(),
            self.repo().gh_repo_arg(),
        ];
        self.run_ok(&argv, WRITE_TIMEOUT).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::time::Duration;

    use super::super::gh::GhOutput;
    use super::super::remote::RemoteRepo;
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

    fn fail(stderr: &str) -> Result<GhOutput, ForgeError> {
        Ok(GhOutput { code: Some(1), stdout: String::new(), stderr: stderr.into() })
    }

    fn github(exec: &FakeExec) -> GitHub<&FakeExec> {
        GitHub::with_exec(exec, PathBuf::from("."), RemoteRepo::parse("https://github.com/o/r.git").unwrap())
    }

    fn v(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|p| (*p).to_owned()).collect()
    }

    const OPTIONS: &str = r#"{"data":{"repository":{"viewerDefaultMergeMethod":"REBASE","mergeCommitAllowed":true,"rebaseMergeAllowed":true,"squashMergeAllowed":false,"autoMergeAllowed":true}}}"#;

    #[test]
    fn options_parse_allowed_methods_and_default() {
        let options = parse_merge_options(OPTIONS).unwrap();
        assert_eq!(options.allowed, vec![MergeMethod::Merge, MergeMethod::Rebase]);
        assert_eq!(options.default, Some(MergeMethod::Rebase));
        assert!(options.auto_merge_allowed);
        // A default that is not allowed is dropped.
        let json = OPTIONS.replace("REBASE\"", "SQUASH\"");
        assert_eq!(parse_merge_options(&json).unwrap().default, None);
        assert!(matches!(parse_merge_options("{}"), Err(ForgeError::Failed(_))));
        assert!(matches!(parse_merge_options("x"), Err(ForgeError::Failed(_))));
    }

    #[test]
    fn pick_prefers_the_choice_then_default_then_first() {
        let options = parse_merge_options(OPTIONS).unwrap();
        assert_eq!(options.pick(MergeMethod::Merge), Some(MergeMethod::Merge));
        assert_eq!(options.pick(MergeMethod::Squash), Some(MergeMethod::Rebase));
        let no_default = MergeOptions { allowed: vec![MergeMethod::Merge], default: None, auto_merge_allowed: false };
        assert_eq!(no_default.pick(MergeMethod::Squash), Some(MergeMethod::Merge));
        let none = MergeOptions { allowed: vec![], default: None, auto_merge_allowed: false };
        assert_eq!(none.pick(MergeMethod::Squash), None);
    }

    #[test]
    fn graphql_method_names_round_trip() {
        for method in [MergeMethod::Squash, MergeMethod::Merge, MergeMethod::Rebase] {
            assert_eq!(MergeMethod::from_graphql(method.graphql()), Some(method));
        }
        assert_eq!(MergeMethod::from_graphql("squash"), Some(MergeMethod::Squash));
        assert_eq!(MergeMethod::from_graphql("x"), None);
    }

    #[test]
    fn merge_options_argv() {
        let exec = FakeExec::with(vec![ok(OPTIONS)]);
        github(&exec).merge_options().unwrap();
        let call = exec.calls.lock().unwrap()[0].clone();
        assert_eq!(call[..3], v(&["api", "graphql", "-f"])[..]);
        assert!(call[3].starts_with("query=query($owner:String!,$repo:String!){ repository(owner:$owner,name:$repo){ viewerDefaultMergeMethod"));
        assert!(call[3].contains("autoMergeAllowed"));
        assert_eq!(call[4..], v(&["-f", "owner=o", "-f", "repo=r"])[..]);
    }

    #[test]
    fn enable_auto_merge_argv_and_validation() {
        let exec = FakeExec::with(vec![ok("{}"), ok("{}")]);
        let gh = github(&exec);
        gh.enable_auto_merge("PR_x", MergeMethod::Squash, "abc").unwrap();
        gh.enable_auto_merge("PR_x", MergeMethod::Rebase, "").unwrap();
        let calls = exec.calls.lock().unwrap().clone();
        assert!(calls[0][3].starts_with("query=mutation($pullRequestId:ID!,$mergeMethod:PullRequestMergeMethod!,$expectedHeadOid:GitObjectID){ enablePullRequestAutoMerge(input:{pullRequestId:$pullRequestId,mergeMethod:$mergeMethod,expectedHeadOid:$expectedHeadOid}){pullRequest{id}} }"));
        assert_eq!(calls[0][4..], v(&["-f", "pullRequestId=PR_x", "-f", "mergeMethod=SQUASH", "-f", "expectedHeadOid=abc"])[..]);
        assert_eq!(calls[1][4..], v(&["-f", "pullRequestId=PR_x", "-f", "mergeMethod=REBASE"])[..]);
        for bad in ["", "-x", "@file"] {
            assert!(gh.enable_auto_merge(bad, MergeMethod::Merge, "a").is_err());
        }
        assert!(gh.enable_auto_merge("PR_x", MergeMethod::Merge, "@f").is_err());
        assert_eq!(exec.calls.lock().unwrap().len(), 2, "validation does not call gh");
    }

    #[test]
    fn clean_status_becomes_a_friendly_message() {
        let exec = FakeExec::with(vec![
            fail("GraphQL: Pull request Pull request is in clean status (enablePullRequestAutoMerge)"),
            fail("GraphQL: something else"),
        ]);
        let gh = github(&exec);
        assert_eq!(
            gh.enable_auto_merge("PR_x", MergeMethod::Squash, "a"),
            Err(ForgeError::Failed(ALREADY_MERGEABLE.into()))
        );
        assert_eq!(
            gh.enable_auto_merge("PR_x", MergeMethod::Squash, "a"),
            Err(ForgeError::Failed("GraphQL: something else".into()))
        );
    }

    #[test]
    fn disable_auto_merge_argv() {
        let exec = FakeExec::with(vec![ok("")]);
        github(&exec).disable_auto_merge(12).unwrap();
        assert_eq!(
            exec.calls.lock().unwrap()[0],
            v(&["pr", "merge", "12", "--disable-auto", "--repo", "o/r"])
        );
    }
}
