//! Data model of the PR store: what a fetch depends on, what a worktree's PR looks like, and the
//! blocking git/`gh` work (run on a background executor by the store).

use std::time::Instant;

use sich_git::forge::{ForgeError, GhExec, GitHub, MergeMethod, PullRequest, RemoteRepo};
use sich_git::{Repo, RepoStatus};

use super::schedule::FetchState;

/// Where pull requests of a worktree live: the push remote, its GitHub repository and the
/// branch pull requests normally target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PrTarget {
    pub remote: String,
    pub repo: RemoteRepo,
    pub base: Option<String>,
}

impl PrTarget {
    /// `<remote>/<base>`, the ref commits are counted against.
    pub(crate) fn base_ref(&self) -> Option<String> {
        self.base.as_ref().map(|base| format!("{}/{base}", self.remote))
    }
}

/// What a PR fetch depends on. When it changes after a status refresh, something moved the
/// branch (an agent pushed, ran `gh pr create`, switched branch...). Background rows only know
/// the branch (`head_oid` is `None`: a "partial" key).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PrKey {
    pub branch: String,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub head_oid: Option<String>,
}

impl PrKey {
    /// A key built from the branch name alone (rows of the sidebar without a Git status).
    pub(crate) fn branch_only(branch: &str) -> Self {
        Self { branch: branch.to_owned(), upstream: None, ahead: 0, head_oid: None }
    }

    fn is_full(&self) -> bool {
        self.head_oid.is_some()
    }

    /// Whether `self` (the key of the last fetch) differs from `now` in a way that matters.
    /// A partial key can only tell the branch apart, so it never looks like a push happened.
    pub(crate) fn differs_from(&self, now: &Self) -> bool {
        if self.branch != now.branch {
            return true;
        }
        self.is_full() && now.is_full() && self != now
    }
}

/// `None` on a detached HEAD: pull requests need a branch.
pub(crate) fn pr_key(status: &RepoStatus) -> Option<PrKey> {
    Some(PrKey {
        branch: status.branch.clone()?,
        upstream: status.upstream.clone(),
        ahead: status.ahead,
        head_oid: status.head_oid.clone(),
    })
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum PrView {
    /// Not a GitHub repository, no remote, or nothing to show yet.
    #[default]
    Hidden,
    Loading,
    GhMissing,
    NotAuthenticated,
    Error(String),
    NoPr,
    Pr(Box<PullRequest>),
}

impl PrView {
    pub(crate) fn pr(&self) -> Option<&PullRequest> {
        match self {
            Self::Pr(pr) => Some(pr),
            _ => None,
        }
    }

    /// `gh` itself is unusable: fetching more would only repeat the failure.
    pub(crate) fn gh_unusable(&self) -> bool {
        matches!(self, Self::GhMissing | Self::NotAuthenticated)
    }
}

/// Maps the result of `gh pr view` to what the section shows.
pub(crate) fn view_from_result(result: Result<Option<PullRequest>, ForgeError>) -> PrView {
    match result {
        Ok(Some(pr)) => PrView::Pr(Box::new(pr)),
        Ok(None) => PrView::NoPr,
        Err(ForgeError::GhMissing) => PrView::GhMissing,
        Err(ForgeError::NotAuthenticated) => PrView::NotAuthenticated,
        Err(ForgeError::Unsupported | ForgeError::NoBranch) => PrView::Hidden,
        Err(error) => PrView::Error(error.to_string()),
    }
}

/// A transient failure while refreshing keeps showing the data we already have.
pub(crate) fn merge_result(previous: &PrView, new: PrView) -> PrView {
    match (previous, &new) {
        (PrView::Pr(_), PrView::Error(_)) => previous.clone(),
        _ => new,
    }
}

pub(crate) fn fetch_view<E: GhExec>(gh: &GitHub<E>, branch: &str) -> PrView {
    view_from_result(gh.pr_for_branch(branch))
}

/// Per-worktree pull request state, cached by worktree path.
#[derive(Clone, Default)]
pub(crate) struct PrEntry {
    pub target: Option<PrTarget>,
    pub resolved_branch: Option<String>,
    pub resolved_at: Option<Instant>,
    pub resolving: bool,
    /// Manual refresh: resolve the target again even if the cache is fresh.
    pub force_resolve: bool,
    pub view: PrView,
    pub fetch: FetchState,
    pub divergence: Option<(u32, u32)>,
    pub div_in_flight: bool,
    pub busy: bool,
    pub action_error: Option<String>,
    /// Branch from the sidebar's worktree list (cheap, may lag behind a checkout).
    pub row_branch: Option<String>,
    /// Key from the Git panel's latest status; present only while the panel shows this worktree.
    pub status_key: Option<PrKey>,
}

impl PrEntry {
    /// The freshest known branch: the Git status wins over the sidebar's list.
    pub(crate) fn branch(&self) -> Option<&str> {
        self.status_key
            .as_ref()
            .map(|key| key.branch.as_str())
            .or(self.row_branch.as_deref())
            .filter(|branch| !branch.is_empty())
    }

    pub(crate) fn key(&self) -> Option<PrKey> {
        self.status_key.clone().or_else(|| self.branch().map(PrKey::branch_only))
    }

    /// On the repository's default branch: there is nothing to open a pull request from.
    pub(crate) fn on_default_branch(&self) -> bool {
        match (self.branch(), self.target.as_ref().and_then(|target| target.base.as_deref())) {
            (Some(branch), Some(base)) => branch == base,
            _ => false,
        }
    }
}

/// The section exists only for a branch of a GitHub repository, and not on the default branch.
pub(crate) fn section_visible(enabled: bool, branch: Option<&str>, state: Option<&PrEntry>) -> bool {
    let (true, Some(branch), Some(state)) = (enabled, branch, state) else {
        return false;
    };
    if state.view == PrView::Hidden {
        return false;
    }
    state.target.as_ref().and_then(|target| target.base.as_deref()) != Some(branch)
}

/// Results computed for another branch than the one the worktree is on now are dropped.
pub(crate) fn result_is_current(entry_branch: Option<&str>, result_branch: &str) -> bool {
    entry_branch == Some(result_branch)
}

// ---------------------------------------------------------------------------------------------
// Background work
// ---------------------------------------------------------------------------------------------

/// Finds the push remote and its GitHub repository (git only, no network).
pub(crate) fn resolve_target(repo: &Repo) -> Option<PrTarget> {
    let remote = repo.push_remote().ok().flatten()?;
    let remote_repo = repo.remote_repo().ok().flatten().filter(RemoteRepo::is_github)?;
    let base = repo.default_base(&remote).ok().flatten();
    Some(PrTarget { remote, repo: remote_repo, base })
}

pub(crate) struct Fetched {
    pub view: PrView,
    pub divergence: Option<(u32, u32)>,
}

pub(crate) fn divergence(repo: &Repo, target: &PrTarget) -> Option<(u32, u32)> {
    repo.base_divergence(&target.base_ref()?).ok()
}

pub(crate) fn fetch_pr(repo: &Repo, target: &PrTarget, branch: &str) -> Fetched {
    let divergence = divergence(repo, target);
    let view = if target.base.as_deref() == Some(branch) {
        PrView::NoPr
    } else {
        let gh = GitHub::new(repo.root().to_path_buf(), target.repo.clone());
        fetch_view(&gh, branch)
    };
    Fetched { view, divergence }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PrAction {
    Merge(MergeMethod),
    Ready(bool),
    Close,
    Reopen,
    Rerun(Vec<u64>),
}

pub(crate) fn run_action(
    repo: &Repo,
    target: &PrTarget,
    pr: &PullRequest,
    action: PrAction,
) -> Result<(), ForgeError> {
    let gh = GitHub::new(repo.root().to_path_buf(), target.repo.clone());
    match action {
        PrAction::Merge(method) => {
            gh.merge_pr(pr.number, method, &pr.head_oid)?;
            // Bring the merge result (and pruned branch) into the local remote-tracking refs.
            let _ = repo.fetch();
            Ok(())
        }
        PrAction::Ready(ready) => gh.set_ready(pr.number, ready),
        PrAction::Close => gh.close_pr(pr.number),
        PrAction::Reopen => gh.reopen_pr(pr.number),
        PrAction::Rerun(ids) => gh.rerun_failed(&ids),
    }
}

#[cfg(test)]
mod tests {
    use sich_git::forge::{GhOutput, PrState};

    use super::super::testutil::{Scripted, pr, remote};
    use super::*;

    fn status(branch: Option<&str>) -> RepoStatus {
        RepoStatus {
            branch: branch.map(str::to_owned),
            head_oid: Some("aaaa".into()),
            upstream: Some("origin/feat".into()),
            ..RepoStatus::default()
        }
    }

    fn state_with(view: PrView, base: Option<&str>) -> PrEntry {
        PrEntry {
            target: Some(PrTarget { remote: "origin".into(), repo: remote(), base: base.map(str::to_owned) }),
            view,
            ..PrEntry::default()
        }
    }

    #[test]
    fn key_needs_a_branch_and_tracks_head_upstream_and_ahead() {
        assert!(pr_key(&status(None)).is_none());
        let a = pr_key(&status(Some("feat"))).unwrap();
        let mut moved = status(Some("feat"));
        moved.ahead = 1;
        assert_ne!(a, pr_key(&moved).unwrap());
        let mut pushed = status(Some("feat"));
        pushed.head_oid = Some("bbbb".into());
        assert_ne!(a, pr_key(&pushed).unwrap());
        let mut no_upstream = status(Some("feat"));
        no_upstream.upstream = None;
        assert_ne!(a, pr_key(&no_upstream).unwrap());
        assert_eq!(a, pr_key(&status(Some("feat"))).unwrap());
    }

    #[test]
    fn partial_keys_only_notice_a_branch_change() {
        let full = pr_key(&status(Some("feat"))).unwrap();
        let partial = PrKey::branch_only("feat");
        assert!(!full.differs_from(&partial), "closing the panel is not a push");
        assert!(!partial.differs_from(&full));
        assert!(partial.differs_from(&PrKey::branch_only("other")));
        assert!(full.differs_from(&PrKey::branch_only("other")));
        let mut moved = full.clone();
        moved.ahead = 2;
        assert!(full.differs_from(&moved), "two full keys compare completely");
        assert!(!full.differs_from(&full.clone()));
    }

    #[test]
    fn entry_prefers_the_status_branch_and_knows_the_default_branch() {
        let mut entry = state_with(PrView::NoPr, Some("main"));
        assert_eq!(entry.branch(), None);
        assert!(entry.key().is_none());
        entry.row_branch = Some("feat".into());
        assert_eq!(entry.branch(), Some("feat"));
        assert_eq!(entry.key(), Some(PrKey::branch_only("feat")));
        entry.status_key = pr_key(&status(Some("other")));
        assert_eq!(entry.branch(), Some("other"));
        assert!(!entry.on_default_branch());
        entry.row_branch = Some("main".into());
        entry.status_key = None;
        assert!(entry.on_default_branch());
        entry.row_branch = Some(String::new());
        assert_eq!(entry.branch(), None);
        assert!(!state_with(PrView::NoPr, None).on_default_branch());
    }

    #[test]
    fn section_is_hidden_without_setting_branch_github_or_on_the_default_branch() {
        let shown = state_with(PrView::NoPr, Some("main"));
        assert!(section_visible(true, Some("feat"), Some(&shown)));
        assert!(!section_visible(false, Some("feat"), Some(&shown)));
        assert!(!section_visible(true, None, Some(&shown)));
        assert!(!section_visible(true, Some("feat"), None));
        assert!(!section_visible(true, Some("main"), Some(&shown)));
        let hidden = state_with(PrView::Hidden, Some("main"));
        assert!(!section_visible(true, Some("feat"), Some(&hidden)));
        let unknown_base = state_with(PrView::Loading, None);
        assert!(section_visible(true, Some("main"), Some(&unknown_base)));
    }

    #[test]
    fn gh_results_map_to_views() {
        assert_eq!(view_from_result(Ok(None)), PrView::NoPr);
        assert!(matches!(view_from_result(Ok(Some(pr(PrState::Open)))), PrView::Pr(_)));
        assert_eq!(view_from_result(Err(ForgeError::GhMissing)), PrView::GhMissing);
        assert_eq!(view_from_result(Err(ForgeError::NotAuthenticated)), PrView::NotAuthenticated);
        assert_eq!(view_from_result(Err(ForgeError::Unsupported)), PrView::Hidden);
        assert_eq!(view_from_result(Err(ForgeError::NoBranch)), PrView::Hidden);
        assert!(matches!(view_from_result(Err(ForgeError::TimedOut)), PrView::Error(text) if !text.is_empty()));
        assert_eq!(view_from_result(Err(ForgeError::Failed("boom".into()))), PrView::Error("boom".into()));
    }

    #[test]
    fn a_failed_refresh_keeps_the_known_pr_but_other_states_replace_it() {
        let known = PrView::Pr(Box::new(pr(PrState::Open)));
        assert_eq!(merge_result(&known, PrView::Error("x".into())), known);
        assert_eq!(merge_result(&known, PrView::NoPr), PrView::NoPr);
        assert_eq!(merge_result(&known, PrView::NotAuthenticated), PrView::NotAuthenticated);
        assert_eq!(merge_result(&PrView::Loading, PrView::Error("x".into())), PrView::Error("x".into()));
    }

    #[test]
    fn gh_unusable_covers_only_missing_and_unauthenticated() {
        assert!(PrView::GhMissing.gh_unusable());
        assert!(PrView::NotAuthenticated.gh_unusable());
        assert!(!PrView::Error("x".into()).gh_unusable());
        assert!(!PrView::NoPr.gh_unusable());
        assert!(!PrView::Loading.gh_unusable());
    }

    #[test]
    fn results_for_an_old_branch_are_not_current() {
        assert!(result_is_current(Some("feat"), "feat"));
        assert!(!result_is_current(Some("other"), "feat"));
        assert!(!result_is_current(None, "feat"));
    }

    #[test]
    fn fetch_view_classifies_gh_output() {
        let run = |code, stdout: &str, stderr: &str| {
            let output = GhOutput { code: Some(code), stdout: stdout.into(), stderr: stderr.into() };
            fetch_view(&GitHub::with_exec(Scripted(output), ".".into(), remote()), "feat")
        };
        assert_eq!(run(1, "", "no pull requests found for branch \"feat\""), PrView::NoPr);
        assert!(matches!(run(1, "", "boom"), PrView::Error(_) | PrView::NotAuthenticated));
        assert!(matches!(run(0, "not json", ""), PrView::Error(_)));
    }

    #[test]
    fn target_base_ref_joins_remote_and_base() {
        let state = state_with(PrView::NoPr, Some("main"));
        assert_eq!(state.target.as_ref().unwrap().base_ref().as_deref(), Some("origin/main"));
        let state = state_with(PrView::NoPr, None);
        assert_eq!(state.target.as_ref().unwrap().base_ref(), None);
    }
}
