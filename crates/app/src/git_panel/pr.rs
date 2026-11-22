//! Pull request state for the Git tab. Everything goes through the user's own `gh` CLI
//! (`sich_git::forge`); Sich never reads or stores GitHub credentials.
//!
//! The pure pieces (visibility, refresh policy, chips, merge blockers) are separate functions so
//! they are unit-tested; the `impl GitPanel` part only schedules background work and applies
//! generation-checked results.

use std::time::{Duration, Instant};

use sich_git::forge::{
    CheckState, FailedJob, ForgeError, GhExec, GitHub, LOG_TAIL_BYTES, LOG_TAIL_LINES, MergeMethod,
    MergeState, Mergeable, PrState, PullRequest, RemoteRepo, ReviewDecision, agent_prompt, agent_prompt_line, log_tail,
    write_report,
};

use super::*;
use crate::i18n::Tr;
use crate::settings::{self, PrMergeMethod};

/// Minimum time between two `gh` fetches caused by anything but one of our own actions.
pub(super) const COOLDOWN: Duration = Duration::from_secs(10);
/// Polling interval for an open PR whose checks are still running.
pub(super) const POLL_PENDING: Duration = Duration::from_secs(60);
/// Polling interval for everything else that can still change.
pub(super) const POLL_IDLE: Duration = Duration::from_secs(300);
/// How often the panel timer re-evaluates the refresh policy.
const TICK: Duration = Duration::from_secs(5);

/// Where pull requests of a worktree live: the push remote, its GitHub repository and the
/// branch pull requests normally target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PrTarget {
    pub remote: String,
    pub repo: RemoteRepo,
    pub base: Option<String>,
}

impl PrTarget {
    /// `<remote>/<base>`, the ref commits are counted against.
    pub(super) fn base_ref(&self) -> Option<String> {
        self.base.as_ref().map(|base| format!("{}/{base}", self.remote))
    }
}

/// What a PR fetch depends on. When it changes after a status refresh, something moved the
/// branch (an agent pushed, ran `gh pr create`, switched branch...).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PrKey {
    pub branch: String,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub head_oid: Option<String>,
}

/// `None` on a detached HEAD: pull requests need a branch.
pub(super) fn pr_key(status: &RepoStatus) -> Option<PrKey> {
    Some(PrKey {
        branch: status.branch.clone()?,
        upstream: status.upstream.clone(),
        ahead: status.ahead,
        head_oid: status.head_oid.clone(),
    })
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) enum PrView {
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
    pub(super) fn pr(&self) -> Option<&PullRequest> {
        match self {
            Self::Pr(pr) => Some(pr),
            _ => None,
        }
    }
}

/// Maps the result of `gh pr view` to what the section shows.
pub(super) fn view_from_result(result: Result<Option<PullRequest>, ForgeError>) -> PrView {
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
pub(super) fn merge_result(previous: &PrView, new: PrView) -> PrView {
    match (previous, &new) {
        (PrView::Pr(_), PrView::Error(_)) => previous.clone(),
        _ => new,
    }
}

pub(super) fn fetch_view<E: GhExec>(gh: &GitHub<E>, branch: &str) -> PrView {
    view_from_result(gh.pr_for_branch(branch))
}

/// The section exists only for a branch of a GitHub repository, and not on the default branch.
pub(super) fn section_visible(enabled: bool, branch: Option<&str>, state: Option<&PrEntry>) -> bool {
    let (true, Some(branch), Some(state)) = (enabled, branch, state) else {
        return false;
    };
    if state.view == PrView::Hidden {
        return false;
    }
    state.target.as_ref().and_then(|target| target.base.as_deref()) != Some(branch)
}

// ---------------------------------------------------------------------------------------------
// Refresh policy
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub(super) struct FetchState {
    pub in_flight: bool,
    /// An action finished while a fetch was running: fetch again afterwards.
    pub queued: bool,
    pub last_fetch: Option<Instant>,
    pub last_key: Option<PrKey>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PrTrigger {
    /// The section was first shown for this worktree (or the cache was invalidated).
    Shown,
    /// A status refresh saw a different branch / upstream / ahead / head.
    Changed,
    Periodic,
    /// Manual refresh button.
    Manual,
    /// One of our own PR/push actions finished: bypasses the cooldown.
    Action,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Decision {
    Fetch,
    /// Remember to fetch again as soon as the running fetch finishes.
    Queue,
    Skip,
}

pub(super) fn decide(trigger: PrTrigger, now: Instant, state: &FetchState) -> Decision {
    let cooled = state
        .last_fetch
        .is_none_or(|last| now.saturating_duration_since(last) >= COOLDOWN);
    let wanted = trigger == PrTrigger::Action || cooled;
    if !wanted {
        Decision::Skip
    } else if state.in_flight {
        if trigger == PrTrigger::Action { Decision::Queue } else { Decision::Skip }
    } else {
        Decision::Fetch
    }
}

/// How often a PR in this state is re-read in the background; `None` = never.
pub(super) fn poll_interval(view: &PrView) -> Option<Duration> {
    match view {
        PrView::Hidden | PrView::Loading => None,
        PrView::Pr(pr) => match pr.state {
            PrState::Merged | PrState::Closed => None,
            PrState::Open | PrState::Draft => {
                if pr.summary().pending > 0 { Some(POLL_PENDING) } else { Some(POLL_IDLE) }
            }
        },
        PrView::GhMissing | PrView::NotAuthenticated | PrView::Error(_) | PrView::NoPr => Some(POLL_IDLE),
    }
}

/// Which trigger (if any) applies to a status refresh or timer tick.
pub(super) fn sync_trigger(
    now: Instant,
    state: &FetchState,
    key: &PrKey,
    view: &PrView,
) -> Option<PrTrigger> {
    let Some(last) = state.last_fetch else {
        return Some(PrTrigger::Shown);
    };
    if state.last_key.as_ref() != Some(key) {
        return Some(PrTrigger::Changed);
    }
    let interval = poll_interval(view)?;
    (now.saturating_duration_since(last) >= interval).then_some(PrTrigger::Periodic)
}

// ---------------------------------------------------------------------------------------------
// Derived UI state
// ---------------------------------------------------------------------------------------------

/// Local commits the remote does not have yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Unpushed {
    Commits(u32),
    NoUpstream,
}

pub(super) fn unpushed(status: &RepoStatus) -> Option<Unpushed> {
    if status.upstream.is_none() {
        Some(Unpushed::NoUpstream)
    } else if status.ahead > 0 {
        Some(Unpushed::Commits(status.ahead))
    } else {
        None
    }
}

/// Like `unpushed`, but a branch without upstream whose head is exactly the PR's head commit
/// counts as pushed (e.g. pushed by `gh pr create` without setting the upstream).
pub(super) fn unpushed_for_pr(status: &RepoStatus, pr: Option<&PullRequest>) -> Option<Unpushed> {
    if status.upstream.is_none() {
        let same_head = pr.is_some_and(|pr| {
            !pr.head_oid.is_empty() && status.head_oid.as_deref() == Some(pr.head_oid.as_str())
        });
        if same_head {
            return None;
        }
    }
    unpushed(status)
}

pub(super) fn needs_push(status: &RepoStatus) -> bool {
    unpushed(status).is_some()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CreateBlock {
    InProgress,
    NoCommits,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CreateButton {
    pub block: Option<CreateBlock>,
    /// Commits ahead of the base, when known.
    pub ahead: Option<u32>,
    pub will_push: bool,
}

impl CreateButton {
    pub(super) fn enabled(&self) -> bool {
        self.block.is_none()
    }
}

pub(super) fn create_button(status: &RepoStatus, divergence: Option<(u32, u32)>) -> CreateButton {
    let block = if status.in_progress.is_some() {
        Some(CreateBlock::InProgress)
    } else if matches!(divergence, Some((0, _))) {
        Some(CreateBlock::NoCommits)
    } else {
        None
    };
    CreateButton { block, ahead: divergence.map(|(ahead, _)| ahead), will_push: needs_push(status) }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Tone {
    Success,
    Warning,
    Danger,
    Muted,
    Merged,
}

pub(super) fn state_pill(state: PrState) -> (Tr, Tone) {
    match state {
        PrState::Open => (Tr::PrStateOpen, Tone::Success),
        PrState::Draft => (Tr::PrStateDraft, Tone::Muted),
        PrState::Merged => (Tr::PrStateMerged, Tone::Merged),
        PrState::Closed => (Tr::PrStateClosed, Tone::Danger),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ChecksKind {
    Failing,
    Running,
    Passed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum MergeChip {
    Conflicts,
    Behind(String),
    Blocked,
    /// Mergeable, but some checks fail (GitHub "unstable").
    ReadyChecksFailing,
    Ready,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Chip {
    Checks(ChecksKind, u32),
    Review(ReviewDecision),
    Merge(MergeChip),
    Base(String),
}

impl Chip {
    pub(super) fn tone(&self) -> Tone {
        match self {
            Self::Checks(ChecksKind::Failing, _) => Tone::Danger,
            Self::Checks(ChecksKind::Running, _) => Tone::Warning,
            Self::Checks(ChecksKind::Passed, _) => Tone::Success,
            Self::Review(ReviewDecision::Approved) => Tone::Success,
            Self::Review(ReviewDecision::ChangesRequested) => Tone::Danger,
            Self::Review(ReviewDecision::ReviewRequired) => Tone::Muted,
            Self::Merge(MergeChip::Conflicts) => Tone::Danger,
            Self::Merge(MergeChip::Behind(_)) => Tone::Warning,
            Self::Merge(MergeChip::Blocked) => Tone::Muted,
            Self::Merge(MergeChip::ReadyChecksFailing) => Tone::Warning,
            Self::Merge(MergeChip::Ready) => Tone::Success,
            Self::Base(_) => Tone::Muted,
        }
    }
}

pub(super) fn checks_chip(pr: &PullRequest) -> Option<Chip> {
    let summary = pr.summary();
    match summary.overall()? {
        CheckState::Failure => Some(Chip::Checks(ChecksKind::Failing, summary.failed)),
        CheckState::Pending => Some(Chip::Checks(ChecksKind::Running, summary.pending)),
        // Only skipped/neutral checks: nothing worth a chip.
        _ if summary.passed == 0 => None,
        _ => Some(Chip::Checks(ChecksKind::Passed, summary.passed)),
    }
}

pub(super) fn merge_chip(pr: &PullRequest) -> Option<MergeChip> {
    if pr.state != PrState::Open {
        return None;
    }
    if pr.merge_state == MergeState::Dirty || pr.mergeable == Mergeable::Conflicting {
        return Some(MergeChip::Conflicts);
    }
    match pr.merge_state {
        MergeState::Behind => Some(MergeChip::Behind(pr.base.clone())),
        MergeState::Blocked => Some(MergeChip::Blocked),
        MergeState::Unstable => Some(MergeChip::ReadyChecksFailing),
        MergeState::Clean | MergeState::HasHooks => Some(MergeChip::Ready),
        MergeState::Draft | MergeState::Unknown | MergeState::Dirty => None,
    }
}

/// Checks, review and merge chips only matter while the PR can still change.
pub(super) fn chips(pr: &PullRequest) -> Vec<Chip> {
    let mut chips = Vec::new();
    if pr.is_open() {
        chips.extend(checks_chip(pr));
        chips.extend(pr.review.map(Chip::Review));
        chips.extend(merge_chip(pr).map(Chip::Merge));
    }
    chips.push(Chip::Base(pr.base.clone()));
    chips
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MergeBlock {
    Draft,
    Conflicts,
    Blocked,
    PushFirst,
}

impl MergeBlock {
    pub(super) fn reason(self) -> Tr {
        match self {
            Self::Draft => Tr::PrReasonDraft,
            Self::Conflicts => Tr::PrReasonConflicts,
            Self::Blocked => Tr::PrReasonBlocked,
            Self::PushFirst => Tr::PrReasonPush,
        }
    }
}

/// Why the Merge button is disabled (first reason wins), `None` when merging is possible.
pub(super) fn merge_block(pr: &PullRequest, unpushed: Option<Unpushed>) -> Option<MergeBlock> {
    if pr.state == PrState::Draft {
        Some(MergeBlock::Draft)
    } else if pr.merge_state == MergeState::Dirty || pr.mergeable == Mergeable::Conflicting {
        Some(MergeBlock::Conflicts)
    } else if pr.merge_state == MergeState::Blocked {
        Some(MergeBlock::Blocked)
    } else if unpushed.is_some() {
        Some(MergeBlock::PushFirst)
    } else {
        None
    }
}

pub(super) fn forge_method(method: PrMergeMethod) -> MergeMethod {
    match method {
        PrMergeMethod::Squash => MergeMethod::Squash,
        PrMergeMethod::Merge => MergeMethod::Merge,
        PrMergeMethod::Rebase => MergeMethod::Rebase,
    }
}

pub(super) fn method_label(method: PrMergeMethod) -> Tr {
    match method {
        PrMergeMethod::Squash => Tr::PrMergeSquash,
        PrMergeMethod::Merge => Tr::PrMergeMerge,
        PrMergeMethod::Rebase => Tr::PrMergeRebase,
    }
}

pub(super) fn merge_confirm_text(lang: Language, pr: &PullRequest, method: PrMergeMethod) -> String {
    tr(lang, Tr::PrMergeConfirm)
        .replace("{n}", &pr.number.to_string())
        .replace("{base}", &pr.base)
        .replace("{method}", tr(lang, method_label(method)))
}

pub(super) fn close_confirm_text(lang: Language, pr: &PullRequest) -> String {
    tr(lang, Tr::PrCloseConfirm).replace("{n}", &pr.number.to_string())
}

pub(super) fn ahead_hint(lang: Language, ahead: u32, base: &str) -> String {
    if ahead == 1 {
        tr(lang, Tr::PrAheadOne).replace("{base}", base)
    } else {
        tr(lang, Tr::PrAhead).replace("{n}", &ahead.to_string()).replace("{base}", base)
    }
}

pub(super) fn unpushed_text(lang: Language, unpushed: Unpushed) -> String {
    match unpushed {
        Unpushed::NoUpstream => tr(lang, Tr::PrNotPushed).to_owned(),
        Unpushed::Commits(1) => tr(lang, Tr::PrUnpushedOne).to_owned(),
        Unpushed::Commits(n) => tr(lang, Tr::PrUnpushed).replace("{n}", &n.to_string()),
    }
}

pub(super) fn chip_text(lang: Language, chip: &Chip) -> String {
    let count = |key: Tr, n: u32| tr(lang, key).replace("{n}", &n.to_string());
    match chip {
        Chip::Checks(ChecksKind::Failing, n) => count(Tr::PrChecksFailing, *n),
        Chip::Checks(ChecksKind::Running, n) => count(Tr::PrChecksRunning, *n),
        Chip::Checks(ChecksKind::Passed, n) => count(Tr::PrChecksPassed, *n),
        Chip::Review(ReviewDecision::Approved) => tr(lang, Tr::PrReviewApproved).to_owned(),
        Chip::Review(ReviewDecision::ChangesRequested) => tr(lang, Tr::PrReviewChanges).to_owned(),
        Chip::Review(ReviewDecision::ReviewRequired) => tr(lang, Tr::PrReviewRequired).to_owned(),
        Chip::Merge(MergeChip::Conflicts) => tr(lang, Tr::PrMergeConflicts).to_owned(),
        Chip::Merge(MergeChip::Behind(base)) => tr(lang, Tr::PrMergeBehind).replace("{base}", base),
        Chip::Merge(MergeChip::Blocked) => tr(lang, Tr::PrMergeBlocked).to_owned(),
        Chip::Merge(MergeChip::ReadyChecksFailing) => tr(lang, Tr::PrMergeReadyChecksFailing).to_owned(),
        Chip::Merge(MergeChip::Ready) => tr(lang, Tr::PrMergeReady).to_owned(),
        Chip::Base(base) => format!("\u{2192} {base}"),
    }
}

/// Short names of the push remote's branches, without the current one and `HEAD`.
/// `names` are `Branch::name()` values of remote branches (`origin/main`).
pub(super) fn remote_base_names<'a>(
    names: impl IntoIterator<Item = &'a str>,
    remote: &str,
    current: &str,
) -> Vec<String> {
    let prefix = format!("{remote}/");
    let mut bases: Vec<String> = names
        .into_iter()
        .filter_map(|name| name.strip_prefix(&prefix))
        .filter(|short| !short.is_empty() && *short != "HEAD" && *short != current)
        .map(str::to_owned)
        .collect();
    bases.sort();
    bases.dedup();
    bases
}

/// A prefilled field is overwritten only while the user has not edited it.
pub(super) fn should_overwrite(current: &str, last_prefill: &str) -> bool {
    current == last_prefill || current.trim().is_empty()
}

// ---------------------------------------------------------------------------------------------
// Background work
// ---------------------------------------------------------------------------------------------

/// Finds the push remote and its GitHub repository (git only, no network).
pub(super) fn resolve_target(repo: &Repo) -> Option<PrTarget> {
    let remote = repo.push_remote().ok().flatten()?;
    let remote_repo = repo.remote_repo().ok().flatten().filter(RemoteRepo::is_github)?;
    let base = repo.default_base(&remote).ok().flatten();
    Some(PrTarget { remote, repo: remote_repo, base })
}

pub(super) struct Fetched {
    pub view: PrView,
    pub divergence: Option<(u32, u32)>,
}

pub(super) fn divergence(repo: &Repo, target: &PrTarget) -> Option<(u32, u32)> {
    repo.base_divergence(&target.base_ref()?).ok()
}

pub(super) fn fetch_pr(repo: &Repo, target: &PrTarget, branch: &str) -> Fetched {
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
pub(super) enum PrAction {
    Merge(MergeMethod),
    Ready(bool),
    Close,
    Reopen,
    Rerun(Vec<u64>),
}

fn run_action(repo: &Repo, target: &PrTarget, pr: &PullRequest, action: PrAction) -> Result<(), ForgeError> {
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

/// Builds the agent prompt for `jobs`, fetching each job's failed-step log through `gh`.
/// A log that cannot be fetched (no job id, gh error, empty output) becomes "(log not available)".
pub(super) fn build_agent_prompt<E: GhExec>(
    gh: &GitHub<E>,
    pr: &PullRequest,
    jobs: Vec<FailedJob>,
) -> String {
    let failures: Vec<(FailedJob, Option<String>)> = jobs
        .into_iter()
        .map(|job| {
            let tail = job
                .job_id
                .and_then(|id| gh.failed_job_log(id).ok())
                .map(|raw| log_tail(&raw, LOG_TAIL_LINES, LOG_TAIL_BYTES))
                .filter(|tail| !tail.is_empty());
            (job, tail)
        })
        .collect();
    agent_prompt(pr, &failures)
}

/// Builds the full report, writes it to `<reports_dir>/pr-<number>-<unix_ms>.md` and returns the
/// single line to paste into the agent terminal. Never contains a line break.
pub(super) fn prepare_agent_line<E: GhExec>(
    gh: &GitHub<E>,
    pr: &PullRequest,
    jobs: Vec<FailedJob>,
    reports_dir: &Path,
    unix_ms: u128,
) -> Result<String, String> {
    let names: Vec<String> = jobs.iter().map(|job| job.name.clone()).collect();
    let report = build_agent_prompt(gh, pr, jobs);
    let path = write_report(reports_dir, pr.number, unix_ms, &report)
        .map_err(|err| format!("Could not write the report to {}: {err}", reports_dir.display()))?;
    Ok(agent_prompt_line(pr, &names, &path))
}

/// `<sich data dir>/sich/agent-prompts`, honoring `SICH_DATA_DIR`.
fn agent_prompts_dir() -> Option<PathBuf> {
    sich_daemon::data_local_dir().map(|dir| dir.join("sich").join("agent-prompts"))
}

// ---------------------------------------------------------------------------------------------
// Panel state and driver
// ---------------------------------------------------------------------------------------------

/// Per-worktree pull request state, cached by worktree path.
#[derive(Default)]
pub(super) struct PrEntry {
    pub target: Option<PrTarget>,
    pub resolved_branch: Option<String>,
    pub resolving: bool,
    pub view: PrView,
    pub fetch: FetchState,
    pub divergence: Option<(u32, u32)>,
    pub div_in_flight: bool,
    pub checks_open: bool,
    pub busy: bool,
    pub action_error: Option<String>,
}

impl PrEntry {
    /// Results of work started under an older selection are dropped, so forget that it runs.
    fn reset_in_flight(&mut self) {
        self.fetch.in_flight = false;
        self.fetch.queued = false;
        self.resolving = false;
        self.div_in_flight = false;
        self.busy = false;
    }
}

impl GitPanel {
    pub(super) fn pr_state(&self) -> Option<&PrEntry> {
        self.worktree.as_ref().and_then(|path| self.pr.get(path))
    }

    pub(super) fn pr_reset_in_flight(&mut self) {
        self.pr.values_mut().for_each(PrEntry::reset_in_flight);
    }

    pub(super) fn pr_fetching(&self) -> bool {
        self.pr_state().is_some_and(|state| state.fetch.in_flight || state.resolving)
    }

    /// Timer owned by the panel; it ends with the selection (generation).
    pub(super) fn start_pr_timer(&mut self, cx: &mut Context<Self>) {
        let generation = self.generation;
        self.pr_task = Some(cx.spawn(async move |panel, cx| {
            loop {
                cx.background_executor().timer(TICK).await;
                let alive = panel.update(cx, |panel, cx| {
                    if panel.generation != generation {
                        return false;
                    }
                    panel.pr_sync(false, cx);
                    true
                });
                if !matches!(alive, Ok(true)) {
                    break;
                }
            }
        }));
    }

    /// Called after every status refresh (`from_status`) and by the timer.
    pub(super) fn pr_sync(&mut self, from_status: bool, cx: &mut Context<Self>) {
        if !settings::get(cx).github_prs {
            return;
        }
        let (Some(path), Some(status)) = (self.worktree.clone(), self.status.as_ref()) else {
            return;
        };
        let Some(key) = pr_key(status) else { return };
        let state = self.pr.entry(path).or_default();
        if state.resolved_branch.as_deref() != Some(key.branch.as_str()) {
            if !state.resolving {
                self.pr_resolve(PrTrigger::Shown, false, cx);
            }
            return;
        }
        if state.target.is_none() {
            return;
        }
        let trigger = sync_trigger(Instant::now(), &state.fetch, &key, &state.view);
        let wants_divergence = from_status && state.view == PrView::NoPr && !state.div_in_flight;
        if wants_divergence {
            self.pr_refresh_divergence(cx);
        }
        if let Some(trigger) = trigger {
            self.pr_request(trigger, cx);
        }
    }

    /// Manual refresh: re-resolve the remote, then fetch (the 10 s cooldown still applies).
    pub(super) fn pr_manual(&mut self, cx: &mut Context<Self>) {
        if settings::get(cx).github_prs {
            self.pr_resolve(PrTrigger::Manual, true, cx);
        }
    }

    /// Explicit refresh from a menu, bypassing the cooldown.
    pub(super) fn pr_refresh_now(&mut self, cx: &mut Context<Self>) {
        if settings::get(cx).github_prs {
            self.pr_resolve(PrTrigger::Action, true, cx);
        }
    }

    fn pr_resolve(&mut self, then: PrTrigger, keep_view: bool, cx: &mut Context<Self>) {
        let (Some(path), Some(repo), Some(branch)) = (
            self.worktree.clone(),
            self.repo.clone(),
            self.status.as_ref().and_then(|status| status.branch.clone()),
        ) else {
            return;
        };
        let state = self.pr.entry(path.clone()).or_default();
        if state.resolving {
            return;
        }
        state.resolving = true;
        let generation = self.generation;
        let job = cx.background_spawn(async move { resolve_target(&repo) });
        cx.spawn(async move |panel, cx| {
            let target = job.await;
            let _ = panel.update(cx, |panel, cx| {
                panel.generation.apply(generation, || {
                    panel.pr_resolved(path, branch, target, then, keep_view, cx)
                });
            });
        })
        .detach();
        cx.notify();
    }

    fn pr_resolved(
        &mut self,
        path: PathBuf,
        branch: String,
        target: Option<PrTarget>,
        then: PrTrigger,
        keep_view: bool,
        cx: &mut Context<Self>,
    ) {
        let state = self.pr.entry(path).or_default();
        state.resolving = false;
        let same_branch = state.resolved_branch.as_deref() == Some(branch.as_str());
        state.resolved_branch = Some(branch);
        state.target = target;
        if state.target.is_none() {
            state.view = PrView::Hidden;
            state.divergence = None;
        } else if !(same_branch && keep_view) {
            state.view = PrView::Loading;
            state.divergence = None;
            state.fetch = FetchState::default();
            state.checks_open = false;
            state.action_error = None;
        }
        let found = state.target.is_some();
        cx.notify();
        if found {
            self.pr_request(then, cx);
        }
    }

    pub(super) fn pr_request(&mut self, trigger: PrTrigger, cx: &mut Context<Self>) {
        let (Some(path), Some(repo), Some(status)) =
            (self.worktree.clone(), self.repo.clone(), self.status.as_ref())
        else {
            return;
        };
        let Some(key) = pr_key(status) else { return };
        let Some(state) = self.pr.get_mut(&path) else { return };
        let Some(target) = state.target.clone() else { return };
        match decide(trigger, Instant::now(), &state.fetch) {
            Decision::Skip => return,
            Decision::Queue => {
                state.fetch.queued = true;
                return;
            }
            Decision::Fetch => {}
        }
        state.fetch.in_flight = true;
        state.fetch.last_key = Some(key.clone());
        let generation = self.generation;
        let branch = key.branch;
        let job = cx.background_spawn(async move { fetch_pr(&repo, &target, &branch) });
        cx.spawn(async move |panel, cx| {
            let fetched = job.await;
            let _ = panel.update(cx, |panel, cx| {
                panel.generation.apply(generation, || panel.pr_fetched(path, fetched, cx));
            });
        })
        .detach();
        cx.notify();
    }

    fn pr_fetched(&mut self, path: PathBuf, fetched: Fetched, cx: &mut Context<Self>) {
        let Some(state) = self.pr.get_mut(&path) else { return };
        state.fetch.in_flight = false;
        state.fetch.last_fetch = Some(Instant::now());
        state.view = merge_result(&state.view, fetched.view);
        state.divergence = fetched.divergence;
        let queued = std::mem::take(&mut state.fetch.queued);
        cx.notify();
        if queued {
            self.pr_request(PrTrigger::Action, cx);
        }
    }

    /// Commit counts against the base are cheap git calls, refreshed together with status.
    fn pr_refresh_divergence(&mut self, cx: &mut Context<Self>) {
        let (Some(path), Some(repo)) = (self.worktree.clone(), self.repo.clone()) else {
            return;
        };
        let Some(state) = self.pr.get_mut(&path) else { return };
        let Some(target) = state.target.clone() else { return };
        state.div_in_flight = true;
        let generation = self.generation;
        let job = cx.background_spawn(async move { divergence(&repo, &target) });
        cx.spawn(async move |panel, cx| {
            let counts = job.await;
            let _ = panel.update(cx, |panel, cx| {
                panel.generation.apply(generation, || {
                    if let Some(state) = panel.pr.get_mut(&path) {
                        state.div_in_flight = false;
                        if state.divergence != counts {
                            state.divergence = counts;
                            cx.notify();
                        }
                    }
                });
            });
        })
        .detach();
    }

    pub(super) fn pr_toggle_checks(&mut self, cx: &mut Context<Self>) {
        if let Some(state) = self.worktree.clone().and_then(|path| self.pr.get_mut(&path)) {
            state.checks_open = !state.checks_open;
            cx.notify();
        }
    }

    pub(super) fn pr_dismiss_error(&mut self, cx: &mut Context<Self>) {
        if let Some(state) = self.worktree.clone().and_then(|path| self.pr.get_mut(&path)) {
            state.action_error = None;
            cx.notify();
        }
    }

    pub(super) fn pr_run_action(&mut self, action: PrAction, cx: &mut Context<Self>) {
        let (Some(path), Some(repo)) = (self.worktree.clone(), self.repo.clone()) else {
            return;
        };
        let Some(state) = self.pr.get_mut(&path) else { return };
        let (Some(target), Some(pr)) = (state.target.clone(), state.view.pr().cloned()) else {
            return;
        };
        if state.busy {
            return;
        }
        state.busy = true;
        state.action_error = None;
        let merged = matches!(action, PrAction::Merge(_));
        let generation = self.generation;
        let job = cx.background_spawn(async move { run_action(&repo, &target, &pr, action) });
        cx.spawn(async move |panel, cx| {
            let result = job.await;
            let _ = panel.update(cx, |panel, cx| {
                if let Some(state) = panel.pr.get_mut(&path) {
                    state.busy = false;
                    state.action_error = result.err().map(|error| error.to_string());
                    // Whoever shows this worktree next must not trust the cached PR.
                    state.fetch.last_fetch = None;
                }
                panel.generation.apply(generation, || {
                    panel.pr_request(PrTrigger::Action, cx);
                    if merged {
                        panel.request_refresh(true);
                    }
                    cx.notify();
                });
            });
        })
        .detach();
        cx.notify();
    }

    /// Fetches the failed-step logs of `jobs` in the background, builds the prompt and emits
    /// `GitPanelEvent::SendToAgent`. The app pastes it into an agent terminal without Enter.
    pub(super) fn pr_send_to_agent(&mut self, jobs: Vec<FailedJob>, cx: &mut Context<Self>) {
        let (Some(path), Some(repo)) = (self.worktree.clone(), self.repo.clone()) else {
            return;
        };
        let Some(state) = self.pr.get_mut(&path) else { return };
        let (Some(target), Some(pr)) = (state.target.clone(), state.view.pr().cloned()) else {
            return;
        };
        if state.busy || jobs.is_empty() {
            return;
        }
        state.busy = true;
        state.action_error = None;
        let generation = self.generation;
        let job = cx.background_spawn(async move {
            let gh = GitHub::new(repo.root().to_path_buf(), target.repo.clone());
            let dir = agent_prompts_dir().ok_or_else(|| "No local data directory for the report".to_owned())?;
            let unix_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis());
            prepare_agent_line(&gh, &pr, jobs, &dir, unix_ms)
        });
        cx.spawn(async move |panel, cx| {
            let result = job.await;
            let _ = panel.update(cx, |panel, cx| {
                if let Some(state) = panel.pr.get_mut(&path) {
                    state.busy = false;
                }
                panel.generation.apply(generation, || match result {
                    Ok(text) => cx.emit(GitPanelEvent::SendToAgent { worktree: path, text }),
                    Err(message) => panel.pr_report_error(&path, message, cx),
                });
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Shows an error in the PR section of `path` (used when the app cannot deliver a prompt).
    pub(crate) fn pr_report_error(&mut self, path: &Path, message: String, cx: &mut Context<Self>) {
        self.pr.entry(path.to_path_buf()).or_default().action_error = Some(message);
        cx.notify();
    }

    /// A pull request was created from the dialog (or already existed).
    pub(super) fn pr_created(&mut self, path: &Path, created: sich_git::forge::CreatedPr, cx: &mut Context<Self>) {
        if let Some(state) = self.pr.get_mut(path) {
            if let sich_git::forge::CreatedPr::Existing(pr) = created {
                state.view = PrView::Pr(Box::new(pr));
            }
            state.action_error = None;
        }
        self.pr_request(PrTrigger::Action, cx);
        self.request_refresh(true);
        cx.notify();
    }

    /// Opens the Create pull request dialog for the selected worktree.
    pub(super) fn pr_open_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(path), Some(repo), Some(status)) =
            (self.worktree.clone(), self.repo.clone(), self.status.as_ref())
        else {
            return;
        };
        let Some(target) = self.pr.get(&path).and_then(|state| state.target.clone()) else {
            return;
        };
        let Some(branch) = status.branch.clone() else { return };
        let ctx = super::pr_dialog::DialogCtx {
            panel: cx.weak_entity(),
            generation: self.generation,
            worktree: path,
            repo,
            branch,
            remote: target.remote.clone(),
            gh_repo: target.repo.clone(),
            default_base: target.base.clone(),
            will_push: needs_push(status),
            uncommitted: status.staged.len() + status.unstaged.len(),
        };
        super::pr_dialog::open(ctx, window, cx);
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use sich_git::forge::{Check, GhOutput};

    use super::*;

    fn pr(state: PrState) -> PullRequest {
        PullRequest {
            number: 7,
            title: "Add thing".into(),
            url: "https://github.com/o/r/pull/7".into(),
            state,
            mergeable: Mergeable::Mergeable,
            merge_state: MergeState::Clean,
            review: None,
            base: "main".into(),
            head: "feat".into(),
            head_oid: "abc".into(),
            checks: Vec::new(),
            updated_at: String::new(),
        }
    }

    fn check(state: CheckState) -> Check {
        Check { name: "ci".into(), workflow: None, state, url: None }
    }

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
            target: Some(PrTarget {
                remote: "origin".into(),
                repo: RemoteRepo { host: "github.com".into(), owner: "o".into(), name: "r".into() },
                base: base.map(str::to_owned),
            }),
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

    struct Scripted(GhOutput);

    impl GhExec for Scripted {
        fn run(&self, _: &Path, _: &[String], _: Duration) -> Result<GhOutput, ForgeError> {
            Ok(self.0.clone())
        }
    }

    #[test]
    fn fetch_view_classifies_gh_output() {
        let remote = RemoteRepo { host: "github.com".into(), owner: "o".into(), name: "r".into() };
        let run = |code, stdout: &str, stderr: &str| {
            let output = GhOutput { code: Some(code), stdout: stdout.into(), stderr: stderr.into() };
            fetch_view(&GitHub::with_exec(Scripted(output), ".".into(), remote.clone()), "feat")
        };
        assert_eq!(run(1, "", "no pull requests found for branch \"feat\""), PrView::NoPr);
        assert!(matches!(run(1, "", "boom"), PrView::Error(_) | PrView::NotAuthenticated));
        assert!(matches!(run(0, "not json", ""), PrView::Error(_)));
    }

    fn gh_with(code: i32, stdout: &str) -> GitHub<Scripted> {
        let output = GhOutput { code: Some(code), stdout: stdout.into(), stderr: "no logs".into() };
        let remote = RemoteRepo { host: "github.com".into(), owner: "o".into(), name: "r".into() };
        GitHub::with_exec(Scripted(output), ".".into(), remote)
    }

    fn job(name: &str, job_id: Option<u64>) -> FailedJob {
        FailedJob { name: name.into(), url: None, job_id }
    }

    #[test]
    fn agent_prompt_includes_the_cleaned_log_tail() {
        let gh = gh_with(0, "build\tstep\t2026-11-22T10:00:00Z error: boom\n");
        let text = build_agent_prompt(&gh, &pr(PrState::Open), vec![job("CI / test", Some(5))]);
        assert!(text.contains("PR #7"), "{text}");
        assert!(text.contains("Failed check: CI / test"), "{text}");
        assert!(text.contains("```\nerror: boom\n```"), "{text}");
        assert!(!text.contains("(log not available)"));
    }

    #[test]
    fn agent_prompt_falls_back_when_the_log_cannot_be_fetched() {
        let failing = build_agent_prompt(&gh_with(1, ""), &pr(PrState::Open), vec![job("a", Some(5))]);
        assert!(failing.contains("(log not available)"), "{failing}");
        let no_id = build_agent_prompt(&gh_with(0, "x"), &pr(PrState::Open), vec![job("a", None)]);
        assert!(no_id.contains("(log not available)"), "{no_id}");
        let empty = build_agent_prompt(&gh_with(0, "\n\n"), &pr(PrState::Open), vec![job("a", Some(1))]);
        assert!(empty.contains("(log not available)"), "{empty}");
        let several = build_agent_prompt(
            &gh_with(0, "j\ts\t2026-11-22T10:00:00Z line"),
            &pr(PrState::Open),
            vec![job("a", Some(1)), job("b", None)],
        );
        assert_eq!(several.matches("Failed check: ").count(), 2);
        assert_eq!(several.matches("(log not available)").count(), 1);
        assert!(!several.ends_with('\n'), "never ends with a newline");
    }

    #[test]
    fn prepared_agent_line_is_single_line_and_report_goes_to_file() {
        let dir = tempfile::tempdir().unwrap();
        let reports = dir.path().join("agent-prompts");
        let gh = gh_with(0, "j\ts\t2026-11-22T10:00:00Z rm -rf /\nj\ts\t2026-11-22T10:00:00Z second\r\n");
        let jobs = vec![job("CI / test", Some(5)), job("lint\nx", None)];
        let line = prepare_agent_line(&gh, &pr(PrState::Open), jobs, &reports, 42).unwrap();
        assert!(!line.contains('\n') && !line.contains('\r'), "{line:?}");
        assert!(line.contains("pr-7-42.md"), "{line}");
        let report = std::fs::read_to_string(reports.join("pr-7-42.md")).unwrap();
        assert!(report.contains("rm -rf /\nsecond"), "{report}");
        assert!(report.contains("Failed check: CI / test"));
    }

    #[test]
    fn prepared_agent_line_reports_write_failure() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("file");
        std::fs::write(&blocker, "x").unwrap();
        let gh = gh_with(0, "x");
        let err = prepare_agent_line(&gh, &pr(PrState::Open), vec![job("a", None)], &blocker.join("sub"), 1)
            .unwrap_err();
        assert!(err.contains("Could not write the report"), "{err}");
    }

    #[test]
    fn decide_applies_cooldown_to_everything_but_actions() {
        let now = Instant::now();
        let mut state = FetchState::default();
        for trigger in [PrTrigger::Shown, PrTrigger::Changed, PrTrigger::Periodic, PrTrigger::Manual, PrTrigger::Action] {
            assert_eq!(decide(trigger, now, &state), Decision::Fetch, "{trigger:?} on a fresh state");
        }
        state.last_fetch = Some(now);
        let soon = now + Duration::from_secs(3);
        for trigger in [PrTrigger::Shown, PrTrigger::Changed, PrTrigger::Periodic, PrTrigger::Manual] {
            assert_eq!(decide(trigger, soon, &state), Decision::Skip, "{trigger:?} inside the cooldown");
        }
        assert_eq!(decide(PrTrigger::Action, soon, &state), Decision::Fetch);
        let later = now + COOLDOWN;
        assert_eq!(decide(PrTrigger::Manual, later, &state), Decision::Fetch);
    }

    #[test]
    fn decide_allows_one_fetch_in_flight_and_queues_actions() {
        let now = Instant::now();
        let state = FetchState { in_flight: true, ..FetchState::default() };
        assert_eq!(decide(PrTrigger::Shown, now, &state), Decision::Skip);
        assert_eq!(decide(PrTrigger::Manual, now, &state), Decision::Skip);
        assert_eq!(decide(PrTrigger::Changed, now, &state), Decision::Skip);
        assert_eq!(decide(PrTrigger::Action, now, &state), Decision::Queue);
    }

    #[test]
    fn poll_interval_depends_on_pr_state() {
        let mut pending = pr(PrState::Open);
        pending.checks = vec![check(CheckState::Pending), check(CheckState::Success)];
        assert_eq!(poll_interval(&PrView::Pr(Box::new(pending.clone()))), Some(POLL_PENDING));
        pending.state = PrState::Draft;
        assert_eq!(poll_interval(&PrView::Pr(Box::new(pending))), Some(POLL_PENDING));
        let mut passed = pr(PrState::Open);
        passed.checks = vec![check(CheckState::Success)];
        assert_eq!(poll_interval(&PrView::Pr(Box::new(passed))), Some(POLL_IDLE));
        assert_eq!(poll_interval(&PrView::Pr(Box::new(pr(PrState::Open)))), Some(POLL_IDLE));
        assert_eq!(poll_interval(&PrView::NoPr), Some(POLL_IDLE));
        assert_eq!(poll_interval(&PrView::Error("x".into())), Some(POLL_IDLE));
        assert_eq!(poll_interval(&PrView::Pr(Box::new(pr(PrState::Merged)))), None);
        assert_eq!(poll_interval(&PrView::Pr(Box::new(pr(PrState::Closed)))), None);
        assert_eq!(poll_interval(&PrView::Hidden), None);
        assert_eq!(poll_interval(&PrView::Loading), None);
    }

    #[test]
    fn sync_trigger_covers_first_show_key_change_and_polling() {
        let now = Instant::now();
        let key = pr_key(&status(Some("feat"))).unwrap();
        let mut state = FetchState::default();
        assert_eq!(sync_trigger(now, &state, &key, &PrView::Loading), Some(PrTrigger::Shown));

        state.last_fetch = Some(now);
        state.last_key = Some(key.clone());
        assert_eq!(sync_trigger(now, &state, &key, &PrView::NoPr), None);

        let mut moved = key.clone();
        moved.ahead = 2;
        assert_eq!(sync_trigger(now, &state, &moved, &PrView::NoPr), Some(PrTrigger::Changed));

        let open = PrView::Pr(Box::new(pr(PrState::Open)));
        assert_eq!(sync_trigger(now + Duration::from_secs(299), &state, &key, &open), None);
        assert_eq!(sync_trigger(now + POLL_IDLE, &state, &key, &open), Some(PrTrigger::Periodic));

        let mut pending = pr(PrState::Open);
        pending.checks = vec![check(CheckState::Pending)];
        let pending = PrView::Pr(Box::new(pending));
        assert_eq!(sync_trigger(now + POLL_PENDING, &state, &key, &pending), Some(PrTrigger::Periodic));

        let merged = PrView::Pr(Box::new(pr(PrState::Merged)));
        assert_eq!(sync_trigger(now + Duration::from_secs(86_400), &state, &key, &merged), None);
        // A branch change still refreshes a merged PR's cache.
        assert_eq!(sync_trigger(now, &state, &moved, &merged), Some(PrTrigger::Changed));
    }

    #[test]
    fn create_button_is_blocked_by_in_progress_operations_and_empty_diffs() {
        let mut status = status(Some("feat"));
        let ready = create_button(&status, Some((3, 0)));
        assert!(ready.enabled());
        assert_eq!(ready.ahead, Some(3));
        assert!(!ready.will_push);

        assert_eq!(create_button(&status, Some((0, 4))).block, Some(CreateBlock::NoCommits));
        let unknown = create_button(&status, None);
        assert!(unknown.enabled());
        assert_eq!(unknown.ahead, None);

        status.in_progress = Some(InProgress::Rebase);
        assert_eq!(create_button(&status, Some((3, 0))).block, Some(CreateBlock::InProgress));
        assert_eq!(create_button(&status, Some((0, 0))).block, Some(CreateBlock::InProgress));
    }

    #[test]
    fn will_push_when_there_is_no_upstream_or_unpushed_commits() {
        let mut status = status(Some("feat"));
        assert!(!needs_push(&status));
        assert_eq!(unpushed(&status), None);
        status.ahead = 2;
        assert_eq!(unpushed(&status), Some(Unpushed::Commits(2)));
        assert!(create_button(&status, Some((2, 0))).will_push);
        status.ahead = 0;
        status.upstream = None;
        assert_eq!(unpushed(&status), Some(Unpushed::NoUpstream));
        assert!(needs_push(&status));
    }

    #[test]
    fn unpushed_for_pr_treats_a_missing_upstream_as_pushed_when_the_pr_head_matches() {
        let mut s = status(Some("feat"));
        s.upstream = None;
        s.head_oid = Some("abc".into());
        let same = pr(PrState::Open); // head_oid "abc"
        assert_eq!(unpushed_for_pr(&s, Some(&same)), None);
        s.head_oid = Some("zzz".into());
        assert_eq!(unpushed_for_pr(&s, Some(&same)), Some(Unpushed::NoUpstream));
        assert_eq!(unpushed_for_pr(&s, None), Some(Unpushed::NoUpstream));
        s.head_oid = None;
        assert_eq!(unpushed_for_pr(&s, Some(&same)), Some(Unpushed::NoUpstream));
        let mut s = status(Some("feat"));
        s.ahead = 3;
        assert_eq!(unpushed_for_pr(&s, Some(&same)), Some(Unpushed::Commits(3)));
        s.ahead = 0;
        assert_eq!(unpushed_for_pr(&s, Some(&same)), None);
    }

    #[test]
    fn unstable_chip_is_a_warning_with_its_own_text() {
        let chip = Chip::Merge(MergeChip::ReadyChecksFailing);
        assert_eq!(chip.tone(), Tone::Warning);
        assert_eq!(chip_text(Language::En, &chip), "Mergeable with warnings");
        assert_eq!(Chip::Merge(MergeChip::Ready).tone(), Tone::Success);
    }

    #[test]
    fn state_pills_use_distinct_tones() {
        assert_eq!(state_pill(PrState::Open), (Tr::PrStateOpen, Tone::Success));
        assert_eq!(state_pill(PrState::Draft), (Tr::PrStateDraft, Tone::Muted));
        assert_eq!(state_pill(PrState::Merged), (Tr::PrStateMerged, Tone::Merged));
        assert_eq!(state_pill(PrState::Closed), (Tr::PrStateClosed, Tone::Danger));
    }

    #[test]
    fn checks_chip_prefers_failures_then_pending_and_hides_when_empty() {
        let mut p = pr(PrState::Open);
        assert_eq!(checks_chip(&p), None);
        p.checks = vec![check(CheckState::Success), check(CheckState::Success)];
        assert_eq!(checks_chip(&p), Some(Chip::Checks(ChecksKind::Passed, 2)));
        p.checks.push(check(CheckState::Pending));
        assert_eq!(checks_chip(&p), Some(Chip::Checks(ChecksKind::Running, 1)));
        p.checks.push(check(CheckState::Failure));
        p.checks.push(check(CheckState::Failure));
        assert_eq!(checks_chip(&p), Some(Chip::Checks(ChecksKind::Failing, 2)));
        p.checks = vec![check(CheckState::Skipped), check(CheckState::Neutral)];
        assert_eq!(checks_chip(&p), None);
    }

    #[test]
    fn merge_chip_follows_merge_state_and_only_applies_to_open_prs() {
        let mut p = pr(PrState::Open);
        let cases = [
            (MergeState::Dirty, Some(MergeChip::Conflicts)),
            (MergeState::Behind, Some(MergeChip::Behind("main".into()))),
            (MergeState::Blocked, Some(MergeChip::Blocked)),
            (MergeState::Clean, Some(MergeChip::Ready)),
            (MergeState::Unstable, Some(MergeChip::ReadyChecksFailing)),
            (MergeState::HasHooks, Some(MergeChip::Ready)),
            (MergeState::Unknown, None),
            (MergeState::Draft, None),
        ];
        for (state, expected) in cases {
            p.merge_state = state;
            assert_eq!(merge_chip(&p), expected, "{state:?}");
        }
        p.merge_state = MergeState::Clean;
        p.mergeable = Mergeable::Conflicting;
        assert_eq!(merge_chip(&p), Some(MergeChip::Conflicts));
        p.mergeable = Mergeable::Mergeable;
        for state in [PrState::Draft, PrState::Merged, PrState::Closed] {
            p.state = state;
            assert_eq!(merge_chip(&p), None, "{state:?}");
        }
    }

    #[test]
    fn chips_list_everything_for_open_prs_and_only_the_base_otherwise() {
        let mut p = pr(PrState::Open);
        p.checks = vec![check(CheckState::Failure)];
        p.review = Some(ReviewDecision::Approved);
        assert_eq!(
            chips(&p),
            vec![
                Chip::Checks(ChecksKind::Failing, 1),
                Chip::Review(ReviewDecision::Approved),
                Chip::Merge(MergeChip::Ready),
                Chip::Base("main".into()),
            ]
        );
        p.state = PrState::Merged;
        assert_eq!(chips(&p), vec![Chip::Base("main".into())]);
        assert_eq!(Chip::Checks(ChecksKind::Failing, 1).tone(), Tone::Danger);
        assert_eq!(Chip::Checks(ChecksKind::Running, 1).tone(), Tone::Warning);
        assert_eq!(Chip::Review(ReviewDecision::ReviewRequired).tone(), Tone::Muted);
        assert_eq!(Chip::Merge(MergeChip::Behind("main".into())).tone(), Tone::Warning);
        assert_eq!(Chip::Base("main".into()).tone(), Tone::Muted);
    }

    #[test]
    fn merge_block_reports_the_first_reason() {
        let mut p = pr(PrState::Open);
        assert_eq!(merge_block(&p, None), None);
        assert_eq!(merge_block(&p, Some(Unpushed::Commits(1))), Some(MergeBlock::PushFirst));
        assert_eq!(merge_block(&p, Some(Unpushed::NoUpstream)), Some(MergeBlock::PushFirst));
        p.merge_state = MergeState::Blocked;
        assert_eq!(merge_block(&p, Some(Unpushed::Commits(1))), Some(MergeBlock::Blocked));
        p.merge_state = MergeState::Dirty;
        assert_eq!(merge_block(&p, None), Some(MergeBlock::Conflicts));
        p.merge_state = MergeState::Clean;
        p.mergeable = Mergeable::Conflicting;
        assert_eq!(merge_block(&p, None), Some(MergeBlock::Conflicts));
        p.state = PrState::Draft;
        assert_eq!(merge_block(&p, None), Some(MergeBlock::Draft));
        p.merge_state = MergeState::Behind;
        p.mergeable = Mergeable::Mergeable;
        p.state = PrState::Open;
        assert_eq!(merge_block(&p, None), None, "behind alone does not block");
    }

    #[test]
    fn methods_map_to_forge_methods_and_labels() {
        assert_eq!(forge_method(PrMergeMethod::Squash), MergeMethod::Squash);
        assert_eq!(forge_method(PrMergeMethod::Merge), MergeMethod::Merge);
        assert_eq!(forge_method(PrMergeMethod::Rebase), MergeMethod::Rebase);
        assert_eq!(tr(Language::En, method_label(PrMergeMethod::Squash)), "Squash and merge");
        assert_eq!(tr(Language::En, method_label(PrMergeMethod::Rebase)), "Rebase and merge");
    }

    #[test]
    fn texts_fill_in_their_parameters_in_every_language() {
        let p = pr(PrState::Open);
        for lang in Language::ALL {
            let merge = merge_confirm_text(lang, &p, PrMergeMethod::Squash);
            assert!(merge.contains("#7") && merge.contains("main"), "{merge}");
            assert!(!merge.contains('{'), "{merge}");
            assert!(close_confirm_text(lang, &p).contains("#7"));
            assert!(ahead_hint(lang, 3, "main").contains('3') && ahead_hint(lang, 3, "main").contains("main"));
            assert!(ahead_hint(lang, 1, "main").contains("main"));
            assert!(!ahead_hint(lang, 1, "main").contains('{'));
            assert!(unpushed_text(lang, Unpushed::Commits(4)).contains('4'));
            assert!(!unpushed_text(lang, Unpushed::Commits(1)).contains('{'));
            assert!(!unpushed_text(lang, Unpushed::NoUpstream).is_empty());
            assert!(chip_text(lang, &Chip::Merge(MergeChip::Behind("dev".into()))).contains("dev"));
            assert!(chip_text(lang, &Chip::Checks(ChecksKind::Failing, 5)).contains('5'));
        }
        assert_eq!(ahead_hint(Language::En, 1, "main"), "1 commit ahead of main");
        assert_eq!(ahead_hint(Language::En, 2, "main"), "2 commits ahead of main");
        assert_eq!(chip_text(Language::En, &Chip::Base("main".into())), "\u{2192} main");
    }

    #[test]
    fn remote_base_names_strip_the_remote_and_skip_current_head_and_other_remotes() {
        let names = ["origin/main", "origin/HEAD", "origin/feat", "origin/dev", "upstream/main", "origin/main", "origin/a/b"];
        assert_eq!(remote_base_names(names, "origin", "feat"), ["a/b", "dev", "main"]);
        assert_eq!(remote_base_names(names, "upstream", "x"), ["main"]);
        assert!(remote_base_names(["origin/"], "origin", "x").is_empty());
        assert!(remote_base_names([], "origin", "x").is_empty());
    }

    #[test]
    fn prefill_never_overwrites_edited_fields() {
        assert!(should_overwrite("", ""));
        assert!(should_overwrite("Title from commits", "Title from commits"));
        assert!(!should_overwrite("My own title", "Title from commits"));
        assert!(should_overwrite("  \n", "Title from commits"), "a cleared field may be refilled");
        assert!(!should_overwrite("typed", ""));
    }

    #[test]
    fn target_base_ref_joins_remote_and_base() {
        let state = state_with(PrView::NoPr, Some("main"));
        assert_eq!(state.target.as_ref().unwrap().base_ref().as_deref(), Some("origin/main"));
        let state = state_with(PrView::NoPr, None);
        assert_eq!(state.target.as_ref().unwrap().base_ref(), None);
    }

    #[test]
    fn reset_in_flight_clears_work_started_under_an_older_selection() {
        let mut state = state_with(PrView::NoPr, Some("main"));
        state.fetch.in_flight = true;
        state.fetch.queued = true;
        state.resolving = true;
        state.div_in_flight = true;
        state.busy = true;
        state.fetch.last_fetch = Some(Instant::now());
        state.reset_in_flight();
        assert!(!state.fetch.in_flight && !state.fetch.queued);
        assert!(!state.resolving && !state.div_in_flight && !state.busy);
        assert!(state.fetch.last_fetch.is_some(), "cached data stays usable");
    }
}
