//! Preparing a worktree for a pull request: which base repository the PR lives in, where its
//! head comes from (a branch of a local remote, or GitHub's `refs/pull/<N>/head`), the local
//! branch name, and the fetch/remote/upstream git steps.
//!
//! Planning is pure ([`plan`]); the git steps are in [`prepare`] and [`apply_upstream`]. Every
//! call is a plain `git` command run through the same runner as the rest of Sich.
use std::time::{Duration, Instant};

use crate::forge::{GitHub, PrListItem, RemoteRepo};
use crate::runner::{GitError, Repo, text};

/// Git config key marking a remote that Sich added for a fork PR.
pub const CREATED_KEY: &str = "sich-created";

/// The repository pull requests are opened against, and the local remote pointing at it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BaseRepo {
    pub repo: RemoteRepo,
    pub remote: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BaseError {
    /// The project has no remote that points at a GitHub repository.
    NoGithubRemote,
    Git(String),
}

/// Picks the base repository the way the PR panel does: the remote `gh repo set-default` marked,
/// else the parent of a fork head repository (`fork_parent` is only called when needed), else the
/// head repository itself.
pub fn resolve_base_repo(
    remotes: &[(String, Option<RemoteRepo>)],
    gh_resolved: Option<&str>,
    push_remote: &str,
    head: &RemoteRepo,
    fork_parent: impl FnOnce() -> Option<RemoteRepo>,
) -> BaseRepo {
    let from_gh = gh_resolved
        .and_then(|name| remotes.iter().find(|(n, _)| n == name))
        .and_then(|(_, repo)| repo.clone())
        .filter(RemoteRepo::is_github);
    let repo = from_gh.or_else(fork_parent).unwrap_or_else(|| head.clone());
    let remote = gh_resolved
        .into_iter()
        .chain([push_remote])
        .chain(remotes.iter().map(|(name, _)| name.as_str()))
        .find_map(|name| {
            let (_, candidate) = remotes.iter().find(|(n, _)| n == name)?;
            candidate.as_ref().filter(|c| c.same_repo(&repo)).map(|_| name.to_owned())
        });
    BaseRepo { repo, remote }
}

/// The base repository of the project at `repo` (asks `gh` only for a fork's parent).
pub fn project_base(repo: &Repo) -> Result<BaseRepo, BaseError> {
    let git = |e: GitError| BaseError::Git(e.to_string());
    let Some(push_remote) = repo.push_remote().map_err(git)? else {
        return Err(BaseError::NoGithubRemote);
    };
    let head = repo
        .remote_repo()
        .map_err(git)?
        .filter(RemoteRepo::is_github)
        .ok_or(BaseError::NoGithubRemote)?;
    let remotes = repo.remotes().unwrap_or_default();
    let gh_resolved = repo.gh_resolved_base().ok().flatten();
    let root = repo.root().to_path_buf();
    Ok(resolve_base_repo(&remotes, gh_resolved.as_deref(), &push_remote, &head, || {
        GitHub::new(root, head.clone()).fork_parent().ok().flatten()
    }))
}

/// The facts of a pull request that decide how its branch is fetched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrHead {
    pub number: u64,
    pub head_ref: String,
    pub cross_repository: bool,
    pub head_owner: Option<String>,
    pub head_repo: Option<String>,
}

impl From<&PrListItem> for PrHead {
    fn from(pr: &PrListItem) -> Self {
        Self {
            number: pr.number,
            head_ref: pr.head_ref.clone(),
            cross_repository: pr.cross_repository,
            head_owner: pr.head_owner.clone(),
            head_repo: pr.head_repo.clone(),
        }
    }
}

/// How the commit to start the worktree from is fetched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fetch {
    /// `+refs/heads/<branch>:refs/remotes/<remote>/<branch>`.
    Head { remote: String, branch: String },
    /// `+refs/pull/<N>/head:refs/sich/pull/<N>`.
    Pull { remote: String, number: u64 },
}

impl Fetch {
    fn remote(&self) -> &str {
        match self {
            Self::Head { remote, .. } | Self::Pull { remote, .. } => remote,
        }
    }

    fn refspec(&self) -> String {
        match self {
            Self::Head { remote, branch } => format!("+refs/heads/{branch}:refs/remotes/{remote}/{branch}"),
            Self::Pull { number, .. } => format!("+refs/pull/{number}/head:refs/sich/pull/{number}"),
        }
    }

    fn tracking_ref(&self) -> String {
        match self {
            Self::Head { remote, branch } => format!("refs/remotes/{remote}/{branch}"),
            Self::Pull { number, .. } => format!("refs/sich/pull/{number}"),
        }
    }
}

/// Where the new branch should push/pull afterwards.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpstreamPlan {
    None,
    /// A branch of an existing remote.
    Remote { remote: String, branch: String },
    /// The PR head repository (a fork) is already a local remote.
    ForkExisting { remote: String, branch: String },
    /// The fork is not a remote yet: add `name` pointing at its clone URL.
    ForkNew { name: String, branch: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// Local branch the worktree is created on.
    pub branch: String,
    /// The branch already exists locally: it is used as is (no upstream change).
    pub reuse_branch: bool,
    pub fetch: Fetch,
    /// Tried when `fetch` fails (the head branch is gone); then no upstream is set.
    pub fallback: Option<Fetch>,
    pub upstream: UpstreamPlan,
}

/// Rejects names that git would refuse or that could be taken as an option. The authoritative
/// check is `git check-ref-format --branch` in [`prepare`].
pub fn check_name_shape(kind: &str, name: &str) -> Result<(), String> {
    let bad = name.is_empty()
        || name.starts_with(['-', '/'])
        || name.ends_with('/')
        || name.contains("..")
        || name.contains("//")
        || name.contains("@{")
        || name == "@"
        || name.contains(['\\', '\0'])
        || name.chars().any(|c| c.is_whitespace() || c.is_control());
    if bad { Err(format!("Invalid {kind} name: {name:?}")) } else { Ok(()) }
}

/// A remote name derived from the fork owner: `pr-<owner>`, with `-2`, `-3`… when taken.
pub fn fork_remote_name(owner: &str, taken: &[String]) -> String {
    let clean: String = owner
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    let base = format!("pr-{}", clean.trim_matches('-'));
    let base = base.trim_end_matches('-').to_owned();
    let mut name = base.clone();
    let mut n = 2;
    while taken.iter().any(|t| t == &name) {
        name = format!("{base}-{n}");
        n += 1;
    }
    name
}

/// Decides how to fetch a PR and which local branch to use. Pure: `branch_exists` answers
/// whether a local branch with that name exists.
pub fn plan(
    pr: &PrHead,
    base: &BaseRepo,
    remotes: &[(String, Option<RemoteRepo>)],
    branch_exists: impl Fn(&str) -> bool,
) -> Result<Plan, String> {
    check_name_shape("branch", &pr.head_ref)?;
    let base_remote = base
        .remote
        .clone()
        .or_else(|| {
            remotes
                .iter()
                .find(|(_, repo)| repo.as_ref().is_some_and(|r| r.same_repo(&base.repo)))
                .map(|(name, _)| name.clone())
        })
        .ok_or_else(|| format!("No git remote points to {}", base.repo.gh_repo_arg()))?;
    check_name_shape("remote", &base_remote)?;
    let pull = Fetch::Pull { remote: base_remote.clone(), number: pr.number };

    if !pr.cross_repository {
        let reuse_branch = branch_exists(&pr.head_ref);
        return Ok(Plan {
            branch: pr.head_ref.clone(),
            reuse_branch,
            fetch: Fetch::Head { remote: base_remote.clone(), branch: pr.head_ref.clone() },
            fallback: Some(pull),
            upstream: if reuse_branch {
                UpstreamPlan::None
            } else {
                UpstreamPlan::Remote { remote: base_remote, branch: pr.head_ref.clone() }
            },
        });
    }

    let branch = if branch_exists(&pr.head_ref) {
        format!("pr-{}-{}", pr.number, pr.head_ref)
    } else {
        pr.head_ref.clone()
    };
    let upstream = match (&pr.head_owner, &pr.head_repo) {
        (Some(owner), Some(name)) => {
            let fork = RemoteRepo { host: base.repo.host.clone(), owner: owner.clone(), name: name.clone() };
            match remotes.iter().find(|(_, repo)| repo.as_ref().is_some_and(|r| r.same_repo(&fork))) {
                Some((remote, _)) => {
                    check_name_shape("remote", remote)?;
                    UpstreamPlan::ForkExisting { remote: remote.clone(), branch: pr.head_ref.clone() }
                }
                None => {
                    let taken: Vec<String> = remotes.iter().map(|(n, _)| n.clone()).collect();
                    UpstreamPlan::ForkNew { name: fork_remote_name(owner, &taken), branch: pr.head_ref.clone() }
                }
            }
        }
        _ => UpstreamPlan::None,
    };
    Ok(Plan { branch, reuse_branch: false, fetch: pull, fallback: None, upstream })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Upstream {
    pub remote: String,
    pub branch: String,
}

/// The result of [`prepare`]: create the worktree on `branch` at `base_sha`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prepared {
    pub branch: String,
    pub base_sha: String,
    /// To apply with [`apply_upstream`] once the branch exists.
    pub upstream: Option<Upstream>,
    /// Non-fatal problems (the worktree is still created).
    pub notes: Vec<String>,
}

fn err(error: GitError) -> String {
    error.to_string()
}

fn exists(repo: &Repo, full_ref: &str) -> bool {
    repo.read(&["rev-parse", "--verify", "--quiet", full_ref]).is_ok()
}

/// Whether a local branch named `name` exists.
pub fn local_branch_exists(repo: &Repo, name: &str) -> bool {
    exists(repo, &format!("refs/heads/{name}"))
}

/// Names of all remotes and their parsed URLs.
pub fn remotes(repo: &Repo) -> Vec<(String, Option<RemoteRepo>)> {
    repo.remotes().unwrap_or_default()
}

fn run_fetch(repo: &Repo, fetch: &Fetch) -> Result<String, String> {
    repo.write_output(&["fetch", "--no-tags", fetch.remote(), &fetch.refspec()], &[]).map_err(err)?;
    let tracking = fetch.tracking_ref();
    let sha = repo
        .read(&["rev-parse", "--verify", &format!("{tracking}^{{commit}}")])
        .map_err(err)?;
    Ok(text(&sha).trim().to_owned())
}

fn narrow_fetch(repo: &Repo, remote: &str, branch: &str) -> Result<(), String> {
    let fetch = Fetch::Head { remote: remote.to_owned(), branch: branch.to_owned() };
    repo.write_output(&["fetch", "--no-tags", remote, &fetch.refspec()], &[]).map(|_| ()).map_err(err)
}

/// Fetches the PR head and sets up the remote for a fork. Call from a background thread.
/// `clone_url` is only called for [`UpstreamPlan::ForkNew`].
pub fn prepare(
    repo: &Repo,
    plan: &Plan,
    clone_url: impl FnOnce() -> Result<Option<String>, String>,
) -> Result<Prepared, String> {
    check_name_shape("branch", &plan.branch)?;
    repo.read(&["check-ref-format", "--branch", &plan.branch])
        .map_err(|_| format!("Invalid branch name: {}", plan.branch))?;
    let mut notes = Vec::new();

    let (base_sha, used_fallback) = match run_fetch(repo, &plan.fetch) {
        Ok(sha) => (sha, false),
        Err(first) => match &plan.fallback {
            Some(fallback) => {
                let sha = run_fetch(repo, fallback).map_err(|second| format!("{first}\n{second}"))?;
                (sha, true)
            }
            None => return Err(first),
        },
    };

    let mut upstream = None;
    if !plan.reuse_branch && !used_fallback {
        match &plan.upstream {
            UpstreamPlan::None => {}
            UpstreamPlan::Remote { remote, branch } => {
                upstream = Some(Upstream { remote: remote.clone(), branch: branch.clone() });
            }
            UpstreamPlan::ForkExisting { remote, branch } => {
                if let Err(error) = narrow_fetch(repo, remote, branch) {
                    notes.push(format!("fetch {remote}/{branch}: {error}"));
                }
                upstream = Some(Upstream { remote: remote.clone(), branch: branch.clone() });
            }
            UpstreamPlan::ForkNew { name, branch } => match clone_url() {
                Ok(Some(url)) if !url.starts_with('-') && !url.chars().any(char::is_whitespace) => {
                    match add_fork_remote(repo, name, branch, &url) {
                        Ok(()) => {
                            if let Err(error) = narrow_fetch(repo, name, branch) {
                                notes.push(format!("fetch {name}/{branch}: {error}"));
                            }
                            upstream = Some(Upstream { remote: name.clone(), branch: branch.clone() });
                        }
                        Err(error) => notes.push(error),
                    }
                }
                Ok(_) => notes.push("The PR's fork has no usable clone URL".to_owned()),
                Err(error) => notes.push(error),
            },
        }
    }
    Ok(Prepared { branch: plan.branch.clone(), base_sha, upstream, notes })
}

fn add_fork_remote(repo: &Repo, name: &str, branch: &str, url: &str) -> Result<(), String> {
    check_name_shape("remote", name)?;
    repo.write_output(&["remote", "add", "-t", branch, "--no-tags", name, url], &[])
        .map_err(|error| format!("add remote {name}: {error}"))?;
    repo.write_output(&["config", &format!("remote.{name}.{CREATED_KEY}"), "true"], &[])
        .map_err(|error| format!("mark remote {name}: {error}"))?;
    Ok(())
}

/// Makes `branch` track `<remote>/<branch>` (`git branch --set-upstream-to`, falling back to
/// writing the same config when the remote-tracking ref is missing).
pub fn apply_upstream(repo: &Repo, branch: &str, upstream: &Upstream) -> Result<(), String> {
    check_name_shape("branch", branch)?;
    check_name_shape("remote", &upstream.remote)?;
    check_name_shape("branch", &upstream.branch)?;
    let target = format!("{}/{}", upstream.remote, upstream.branch);
    if repo.write_output(&["branch", "--set-upstream-to", &target, branch], &[]).is_ok() {
        return Ok(());
    }
    repo.write_output(&["config", &format!("branch.{branch}.remote"), &upstream.remote], &[]).map_err(err)?;
    repo.write_output(
        &["config", &format!("branch.{branch}.merge"), &format!("refs/heads/{}", upstream.branch)],
        &[],
    )
    .map(|_| ())
    .map_err(err)
}

/// Polls until the local branch exists (the daemon creates it with the worktree). Returns false on timeout.
pub fn wait_for_branch(repo: &Repo, branch: &str, timeout: Duration, poll: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if local_branch_exists(repo, branch) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(poll);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::test_repo::TestRepo;

    fn rr(owner: &str, name: &str) -> RemoteRepo {
        RemoteRepo { host: "github.com".into(), owner: owner.into(), name: name.into() }
    }

    fn base() -> BaseRepo {
        BaseRepo { repo: rr("up", "app"), remote: Some("upstream".into()) }
    }

    fn remotes() -> Vec<(String, Option<RemoteRepo>)> {
        vec![("origin".into(), Some(rr("me", "app"))), ("upstream".into(), Some(rr("up", "app")))]
    }

    fn pr(number: u64, head: &str) -> PrHead {
        PrHead {
            number,
            head_ref: head.into(),
            cross_repository: false,
            head_owner: Some("up".into()),
            head_repo: Some("app".into()),
        }
    }

    fn fork_pr(number: u64, head: &str, owner: &str) -> PrHead {
        PrHead {
            cross_repository: true,
            head_owner: Some(owner.into()),
            head_repo: Some("app".into()),
            ..pr(number, head)
        }
    }

    fn head(remote: &str, branch: &str) -> Fetch {
        Fetch::Head { remote: remote.into(), branch: branch.into() }
    }

    fn pull(remote: &str, number: u64) -> Fetch {
        Fetch::Pull { remote: remote.into(), number }
    }

    #[test]
    fn base_resolution_matches_the_pr_panel() {
        let head_repo = rr("me", "app");
        let result = resolve_base_repo(&remotes(), Some("upstream"), "origin", &head_repo, || None);
        assert_eq!(result, BaseRepo { repo: rr("up", "app"), remote: Some("upstream".into()) });
        let result = resolve_base_repo(&remotes(), None, "origin", &head_repo, || Some(rr("up", "app")));
        assert_eq!(result.remote.as_deref(), Some("upstream"));
        // Not a fork: the push repository itself.
        let result = resolve_base_repo(&remotes(), None, "origin", &head_repo, || None);
        assert_eq!(result, BaseRepo { repo: rr("me", "app"), remote: Some("origin".into()) });
        // Parent without a local remote.
        let only_origin = vec![("origin".to_owned(), Some(rr("me", "app")))];
        let result = resolve_base_repo(&only_origin, None, "origin", &head_repo, || Some(rr("up", "app")));
        assert_eq!(result, BaseRepo { repo: rr("up", "app"), remote: None });
        // Case-insensitive comparison.
        let result = resolve_base_repo(&only_origin, None, "origin", &head_repo, || Some(rr("ME", "APP")));
        assert_eq!(result.remote.as_deref(), Some("origin"));
    }

    #[test]
    fn same_repo_pr_fetches_the_head_branch_with_pull_ref_fallback() {
        let p = plan(&pr(12, "feat/x"), &base(), &remotes(), |_| false).unwrap();
        assert_eq!(
            p,
            Plan {
                branch: "feat/x".into(),
                reuse_branch: false,
                fetch: head("upstream", "feat/x"),
                fallback: Some(pull("upstream", 12)),
                upstream: UpstreamPlan::Remote { remote: "upstream".into(), branch: "feat/x".into() },
            }
        );
    }

    #[test]
    fn same_repo_pr_reuses_an_existing_local_branch_without_touching_its_upstream() {
        let p = plan(&pr(12, "feat/x"), &base(), &remotes(), |b| b == "feat/x").unwrap();
        assert_eq!(p.branch, "feat/x");
        assert!(p.reuse_branch);
        assert_eq!(p.upstream, UpstreamPlan::None);
    }

    #[test]
    fn base_remote_is_found_among_remotes_when_unknown() {
        let b = BaseRepo { repo: rr("up", "app"), remote: None };
        let p = plan(&pr(1, "x"), &b, &remotes(), |_| false).unwrap();
        assert_eq!(p.fetch, head("upstream", "x"));
        let lonely = vec![("origin".to_owned(), Some(rr("me", "app")))];
        let error = plan(&pr(1, "x"), &b, &lonely, |_| false).unwrap_err();
        assert!(error.contains("up/app"), "{error}");
    }

    #[test]
    fn fork_pr_uses_the_pull_ref_and_adds_a_remote_for_the_fork() {
        let p = plan(&fork_pr(7, "fix", "bobby"), &base(), &remotes(), |_| false).unwrap();
        assert_eq!(p.branch, "fix");
        assert_eq!(p.fetch, pull("upstream", 7));
        assert_eq!(p.fallback, None);
        assert_eq!(p.upstream, UpstreamPlan::ForkNew { name: "pr-bobby".into(), branch: "fix".into() });
    }

    #[test]
    fn fork_pr_branch_name_collision_gets_a_pr_prefix() {
        let p = plan(&fork_pr(7, "main", "bobby"), &base(), &remotes(), |b| b == "main").unwrap();
        assert_eq!(p.branch, "pr-7-main");
        // The upstream still tracks the fork's own branch name.
        assert_eq!(p.upstream, UpstreamPlan::ForkNew { name: "pr-bobby".into(), branch: "main".into() });
        assert!(!p.reuse_branch);
    }

    #[test]
    fn fork_that_is_already_a_remote_is_reused() {
        let p = plan(&fork_pr(7, "fix", "me"), &base(), &remotes(), |_| false).unwrap();
        assert_eq!(p.upstream, UpstreamPlan::ForkExisting { remote: "origin".into(), branch: "fix".into() });
    }

    #[test]
    fn deleted_fork_has_no_upstream() {
        let mut gone = fork_pr(9, "patch-1", "x");
        gone.head_owner = None;
        gone.head_repo = None;
        let p = plan(&gone, &base(), &remotes(), |_| false).unwrap();
        assert_eq!((p.fetch, p.upstream), (pull("upstream", 9), UpstreamPlan::None));
    }

    #[test]
    fn fork_remote_names_are_unique_and_safe() {
        assert_eq!(fork_remote_name("bobby", &[]), "pr-bobby");
        assert_eq!(fork_remote_name("bobby", &["pr-bobby".into()]), "pr-bobby-2");
        assert_eq!(fork_remote_name("bobby", &["pr-bobby".into(), "pr-bobby-2".into()]), "pr-bobby-3");
        assert_eq!(fork_remote_name("we ird/n@me", &[]), "pr-we-ird-n-me");
        assert_eq!(fork_remote_name("-x-", &[]), "pr-x");
        let taken = vec![("origin".to_owned(), None), ("pr-bobby".to_owned(), None)];
        let p = plan(&fork_pr(7, "fix", "bobby"), &base(), &[remotes(), taken].concat(), |_| false).unwrap();
        assert_eq!(p.upstream, UpstreamPlan::ForkNew { name: "pr-bobby-2".into(), branch: "fix".into() });
    }

    #[test]
    fn unsafe_names_are_rejected() {
        for bad in ["", "-x", "--upload-pack=x", "a..b", "a b", "/x", "x/", "a//b", "a\\b", "a\tb"] {
            assert!(plan(&pr(1, bad), &base(), &remotes(), |_| false).is_err(), "{bad:?}");
        }
        let weird = BaseRepo { repo: rr("up", "app"), remote: Some("-evil".into()) };
        assert!(plan(&pr(1, "x"), &weird, &remotes(), |_| false).is_err());
    }

    // ---- git steps on temp repos ----------------------------------------------------------

    struct World {
        work: TestRepo,
        upstream: String,
        fork: String,
        feat_sha: String,
        pull_sha: String,
        fork_sha: String,
    }

    fn bare(work: &TestRepo, name: &str) -> String {
        let path = work.temp.path().join(name).to_str().unwrap().to_owned();
        work.git(&["init", "--bare", "-q", "-b", "main", &path]);
        path
    }

    /// `work` (clone with `origin` = bare upstream) plus a bare fork. Upstream has `main`,
    /// `feat/x`, and `refs/pull/5/head` / `refs/pull/6/head`; the fork has branch `fix`.
    fn world() -> World {
        let work = TestRepo::new();
        let (upstream, fork) = (bare(&work, "upstream.git"), bare(&work, "fork.git"));
        work.git(&["remote", "add", "origin", &upstream]);
        work.file("a.txt", "one\n");
        work.commit("first");
        work.git(&["push", "-q", "origin", "main"]);
        work.git(&["checkout", "-q", "-b", "feat/x"]);
        work.file("b.txt", "feat\n");
        work.commit("feat");
        let feat_sha = work.git(&["rev-parse", "HEAD"]).trim().to_owned();
        work.git(&["push", "-q", "origin", "feat/x"]);
        work.git(&["checkout", "-q", "-b", "tmp-pull"]);
        work.file("c.txt", "pull\n");
        work.commit("pull head");
        let pull_sha = work.git(&["rev-parse", "HEAD"]).trim().to_owned();
        work.git(&["push", "-q", "origin", "HEAD:refs/pull/5/head"]);
        work.git(&["checkout", "-q", "-b", "tmp-fork"]);
        work.file("d.txt", "fork\n");
        work.commit("fork head");
        let fork_sha = work.git(&["rev-parse", "HEAD"]).trim().to_owned();
        work.git(&["push", "-q", "origin", "HEAD:refs/pull/6/head"]);
        work.git(&["push", "-q", &fork, "HEAD:refs/heads/fix"]);
        work.git(&["checkout", "-q", "main"]);
        work.git(&["branch", "-q", "-D", "feat/x", "tmp-pull", "tmp-fork"]);
        World { work, upstream, fork, feat_sha, pull_sha, fork_sha }
    }

    fn local_plan(w: &World, repo: &Repo, pr: &PrHead) -> Plan {
        let base = BaseRepo { repo: rr("up", "app"), remote: Some("origin".into()) };
        plan(pr, &base, &[("origin".to_owned(), Some(rr("up", "app")))], |b| local_branch_exists(repo, b))
            .unwrap_or_else(|e| panic!("{e} ({})", w.upstream))
    }

    fn no_url() -> Result<Option<String>, String> {
        panic!("clone URL must not be requested")
    }

    #[test]
    fn same_repo_fetch_resolves_the_head_sha_and_sets_the_upstream() {
        let w = world();
        let repo = w.work.open();
        let p = local_plan(&w, &repo, &pr(12, "feat/x"));
        let prepared = prepare(&repo, &p, no_url).unwrap();
        assert_eq!(prepared.branch, "feat/x");
        assert_eq!(prepared.base_sha, w.feat_sha);
        assert!(prepared.notes.is_empty());
        assert_eq!(prepared.upstream, Some(Upstream { remote: "origin".into(), branch: "feat/x".into() }));
        assert!(exists(&repo, "refs/remotes/origin/feat/x"));
        // The branch does not exist yet; the daemon creates it, then the upstream is applied.
        assert!(!local_branch_exists(&repo, "feat/x"));
        w.work.git(&["branch", "feat/x", &prepared.base_sha]);
        apply_upstream(&repo, "feat/x", prepared.upstream.as_ref().unwrap()).unwrap();
        assert_eq!(
            w.work.git(&["rev-parse", "--abbrev-ref", "feat/x@{upstream}"]).trim(),
            "origin/feat/x"
        );
    }

    #[test]
    fn missing_head_branch_falls_back_to_the_pull_ref_without_upstream() {
        let w = world();
        let repo = w.work.open();
        let p = local_plan(&w, &repo, &pr(5, "deleted-branch"));
        let prepared = prepare(&repo, &p, no_url).unwrap();
        assert_eq!(prepared.branch, "deleted-branch");
        assert_eq!(prepared.base_sha, w.pull_sha);
        assert_eq!(prepared.upstream, None);
        assert!(exists(&repo, "refs/sich/pull/5"));
    }

    #[test]
    fn existing_local_branch_is_kept_and_gets_no_upstream() {
        let w = world();
        let repo = w.work.open();
        w.work.git(&["branch", "feat/x", "main"]);
        let p = local_plan(&w, &repo, &pr(12, "feat/x"));
        assert!(p.reuse_branch);
        let prepared = prepare(&repo, &p, no_url).unwrap();
        assert_eq!(prepared.upstream, None);
        assert_eq!(prepared.base_sha, w.feat_sha);
    }

    #[test]
    fn missing_head_and_pull_ref_is_an_error() {
        let w = world();
        let repo = w.work.open();
        let p = local_plan(&w, &repo, &pr(99, "nope"));
        let error = prepare(&repo, &p, no_url).unwrap_err();
        assert!(!error.is_empty());
    }

    #[test]
    fn fork_pr_adds_a_marked_remote_and_tracks_the_fork_branch() {
        let w = world();
        let repo = w.work.open();
        // A local `fix` branch forces the `pr-<N>-` prefix.
        w.work.git(&["branch", "fix", "main"]);
        let p = local_plan(&w, &repo, &fork_pr(6, "fix", "bobby"));
        assert_eq!(p.branch, "pr-6-fix");
        let fork = w.fork.clone();
        let prepared = prepare(&repo, &p, move || Ok(Some(fork))).unwrap();
        assert_eq!(prepared.branch, "pr-6-fix");
        assert_eq!(prepared.base_sha, w.fork_sha);
        assert!(prepared.notes.is_empty(), "{:?}", prepared.notes);
        assert_eq!(prepared.upstream, Some(Upstream { remote: "pr-bobby".into(), branch: "fix".into() }));
        assert_eq!(w.work.git(&["config", "--get", "remote.pr-bobby.url"]).trim(), w.fork);
        assert_eq!(w.work.git(&["config", "--get", "remote.pr-bobby.sich-created"]).trim(), "true");
        assert_eq!(w.work.git(&["config", "--get", "remote.pr-bobby.tagopt"]).trim(), "--no-tags");
        assert!(exists(&repo, "refs/remotes/pr-bobby/fix"));
        w.work.git(&["branch", "pr-6-fix", &prepared.base_sha]);
        apply_upstream(&repo, "pr-6-fix", prepared.upstream.as_ref().unwrap()).unwrap();
        assert_eq!(w.work.git(&["config", "--get", "branch.pr-6-fix.remote"]).trim(), "pr-bobby");
        assert_eq!(w.work.git(&["config", "--get", "branch.pr-6-fix.merge"]).trim(), "refs/heads/fix");
    }

    #[test]
    fn fork_pr_without_a_clone_url_still_prepares_the_start_point() {
        let w = world();
        let repo = w.work.open();
        let p = local_plan(&w, &repo, &fork_pr(6, "fix", "bobby"));
        for url in [Ok(None), Err("gh failed".to_owned())] {
            let prepared = prepare(&repo, &p, || url).unwrap();
            assert_eq!(prepared.base_sha, w.fork_sha);
            assert_eq!(prepared.upstream, None);
            assert_eq!(prepared.notes.len(), 1);
        }
        assert!(w.work.try_git(&["config", "--get", "remote.pr-bobby.url"]).is_err());
    }

    #[test]
    fn suspicious_clone_urls_are_not_added_as_remotes() {
        let w = world();
        let repo = w.work.open();
        let p = local_plan(&w, &repo, &fork_pr(6, "fix", "bobby"));
        let prepared = prepare(&repo, &p, || Ok(Some("--upload-pack=evil".to_owned()))).unwrap();
        assert_eq!(prepared.upstream, None);
        assert!(w.work.try_git(&["config", "--get", "remote.pr-bobby.url"]).is_err());
    }

    #[test]
    fn existing_fork_remote_is_reused_and_not_marked_as_created() {
        let w = world();
        let repo = w.work.open();
        w.work.git(&["remote", "add", "bob", &w.fork]);
        let plan = Plan {
            branch: "fix".into(),
            reuse_branch: false,
            fetch: pull("origin", 6),
            fallback: None,
            upstream: UpstreamPlan::ForkExisting { remote: "bob".into(), branch: "fix".into() },
        };
        let prepared = prepare(&repo, &plan, no_url).unwrap();
        assert_eq!(prepared.upstream, Some(Upstream { remote: "bob".into(), branch: "fix".into() }));
        assert!(exists(&repo, "refs/remotes/bob/fix"));
        assert!(w.work.try_git(&["config", "--get", "remote.bob.sich-created"]).is_err());
    }

    #[test]
    fn invalid_branch_names_are_rejected_before_any_fetch() {
        let w = world();
        let repo = w.work.open();
        for bad in ["a..b", "a b", "x.lock", "a~b", "-x"] {
            let plan = Plan {
                branch: bad.into(),
                reuse_branch: false,
                fetch: pull("origin", 5),
                fallback: None,
                upstream: UpstreamPlan::None,
            };
            assert!(prepare(&repo, &plan, no_url).is_err(), "{bad}");
        }
        assert!(!exists(&repo, "refs/sich/pull/5"));
    }

    #[test]
    fn apply_upstream_falls_back_to_config_without_a_tracking_ref() {
        let w = world();
        let repo = w.work.open();
        w.work.git(&["branch", "solo", "main"]);
        let up = Upstream { remote: "origin".into(), branch: "not-fetched".into() };
        apply_upstream(&repo, "solo", &up).unwrap();
        assert_eq!(w.work.git(&["config", "--get", "branch.solo.merge"]).trim(), "refs/heads/not-fetched");
        assert!(apply_upstream(&repo, "-x", &up).is_err());
    }

    #[test]
    fn wait_for_branch_sees_a_late_branch_and_times_out() {
        let w = world();
        let repo = w.work.open();
        assert!(wait_for_branch(&repo, "main", Duration::from_millis(100), Duration::from_millis(10)));
        assert!(!wait_for_branch(&repo, "later", Duration::from_millis(60), Duration::from_millis(10)));
        let late = repo.clone();
        let handle = std::thread::spawn(move || {
            wait_for_branch(&late, "later", Duration::from_secs(5), Duration::from_millis(20))
        });
        std::thread::sleep(Duration::from_millis(100));
        w.work.git(&["branch", "later", "main"]);
        assert!(handle.join().unwrap());
    }
}
