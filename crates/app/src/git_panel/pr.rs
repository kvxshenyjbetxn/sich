//! Pull request logic of the Git panel that depends on the panel's own state (status, commit
//! counts, unpushed commits) plus thin wrappers around the shared `PrStore`. The state, the refresh
//! policy and the `gh` work live in `crate::pr_store`.
//!
//! The pure pieces (chips, merge blockers, unpushed rules, create-button logic, compact row) are
//! separate functions so they are unit-tested.

use gpui_kit::App;
use sich_git::forge::{CheckState, MergeMethod, MergeState, Mergeable, PrState, PullRequest, ReviewDecision};

use super::*;
use crate::i18n::Tr;
use crate::pr_store::{Glyph, PrAction, PrEntry, PrView, Tone, glyph_of_pr, pr_key};
use crate::settings::PrMergeMethod;

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
// Compact row of the Git tab
// ---------------------------------------------------------------------------------------------

/// What the one-line pull request summary under the branch header shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CompactRow {
    pub glyph: Glyph,
    /// `#123`.
    pub number: String,
    pub title: String,
    /// Checks mini-chip, only while the PR is open and has checks worth showing.
    pub checks: Option<Chip>,
}

pub(super) fn compact_row(pr: &PullRequest) -> CompactRow {
    CompactRow {
        glyph: glyph_of_pr(pr),
        number: format!("#{}", pr.number),
        title: pr.title.clone(),
        checks: if pr.is_open() { checks_chip(pr) } else { None },
    }
}

/// What the Git tab shows for the pull request section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum GitTabPr {
    /// Nothing: the section is hidden, or the state (loading, no `gh`, not signed in, error) is
    /// only shown in the "Pull request" tab.
    Nothing,
    /// "Create pull request" row.
    Create,
    Compact(Box<CompactRow>),
}

pub(super) fn git_tab_pr(visible: bool, view: &PrView) -> GitTabPr {
    if !visible {
        return GitTabPr::Nothing;
    }
    match view {
        PrView::NoPr => GitTabPr::Create,
        PrView::Pr(pr) => GitTabPr::Compact(Box::new(compact_row(pr))),
        _ => GitTabPr::Nothing,
    }
}

// ---------------------------------------------------------------------------------------------
// Panel wrappers around the store
// ---------------------------------------------------------------------------------------------

impl GitPanel {
    /// A snapshot of the selected worktree's PR state.
    pub(super) fn pr_state(&self, cx: &App) -> Option<PrEntry> {
        let path = self.worktree.as_deref()?;
        self.store.read(cx).entry(path).cloned()
    }

    pub(super) fn pr_fetching(&self, cx: &App) -> bool {
        self.worktree.as_deref().is_some_and(|path| self.store.read(cx).fetching(path))
    }

    /// Called after every status refresh: tells the store the branch/upstream/head the PR depends on.
    pub(super) fn pr_sync_status(&mut self, cx: &mut Context<Self>) {
        let (Some(path), Some(status)) = (self.worktree.clone(), self.status.as_ref()) else {
            return;
        };
        let key = pr_key(status);
        self.store.update(cx, |store, cx| store.report_status(&path, key, cx));
    }

    /// Manual refresh: re-resolve the remote, then fetch (the 10 s cooldown still applies).
    pub(super) fn pr_manual(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = self.worktree.clone() {
            self.store
                .update(cx, |store, cx| store.refresh(&path, crate::pr_store::PrTrigger::Manual, cx));
        }
    }

    /// Explicit refresh from a menu, bypassing the cooldown.
    pub(super) fn pr_refresh_now(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = self.worktree.clone() {
            self.store
                .update(cx, |store, cx| store.refresh(&path, crate::pr_store::PrTrigger::Action, cx));
        }
    }

    /// One of our own actions (push, amend, ...) finished: fetch again now.
    pub(super) fn pr_request_action(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = self.worktree.clone() {
            self.store.update(cx, |store, cx| store.request_action_refresh(&path, cx));
        }
    }

    pub(super) fn pr_dismiss_error(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = self.worktree.clone() {
            self.store.update(cx, |store, cx| store.dismiss_error(&path, cx));
        }
    }

    pub(super) fn pr_run_action(&mut self, action: PrAction, cx: &mut Context<Self>) {
        if let Some(path) = self.worktree.clone() {
            self.store.update(cx, |store, cx| store.run_action(&path, action, cx));
        }
    }

    pub(super) fn pr_send_to_agent(&mut self, jobs: Vec<sich_git::forge::FailedJob>, cx: &mut Context<Self>) {
        if let Some(path) = self.worktree.clone() {
            self.store.update(cx, |store, cx| store.send_to_agent(&path, jobs, cx));
        }
    }

    /// Shows an error in the PR section of `path` (used when the app cannot deliver a prompt).
    pub(crate) fn pr_report_error(&mut self, path: &Path, message: String, cx: &mut Context<Self>) {
        self.store.update(cx, |store, cx| store.report_error(path, message, cx));
    }

    /// A pull request was created from the dialog (or already existed).
    pub(super) fn pr_created(&mut self, path: &Path, created: sich_git::forge::CreatedPr, cx: &mut Context<Self>) {
        self.store.update(cx, |store, cx| store.created(path, created, cx));
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
        let Some(target) = self.store.read(cx).entry(&path).and_then(|state| state.target.clone()) else {
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
    use super::*;
    use crate::pr_store::testutil::{check, pr};

    fn status(branch: Option<&str>) -> RepoStatus {
        RepoStatus {
            branch: branch.map(str::to_owned),
            head_oid: Some("aaaa".into()),
            upstream: Some("origin/feat".into()),
            ..RepoStatus::default()
        }
    }

    #[test]
    fn compact_row_shows_number_title_glyph_and_the_checks_chip() {
        let mut p = pr(PrState::Open);
        p.checks = vec![check(CheckState::Success), check(CheckState::Failure)];
        let row = compact_row(&p);
        assert_eq!(row.number, "#7");
        assert_eq!(row.title, "Add thing");
        assert_eq!(row.checks, Some(Chip::Checks(ChecksKind::Failing, 1)));
        assert_eq!(row.glyph.tone, Tone::Danger);
        p.checks.clear();
        assert_eq!(compact_row(&p).checks, None, "no checks, no chip");
    }

    #[test]
    fn compact_row_has_no_checks_chip_once_the_pr_is_merged_or_closed() {
        for state in [PrState::Merged, PrState::Closed] {
            let mut p = pr(state);
            p.checks = vec![check(CheckState::Success)];
            assert_eq!(compact_row(&p).checks, None, "{state:?}");
        }
        let mut draft = pr(PrState::Draft);
        draft.checks = vec![check(CheckState::Success)];
        assert_eq!(compact_row(&draft).checks, Some(Chip::Checks(ChecksKind::Passed, 1)));
    }

    #[test]
    fn git_tab_shows_the_row_only_for_a_pr_and_create_only_without_one() {
        let found = PrView::Pr(Box::new(pr(PrState::Open)));
        assert!(matches!(git_tab_pr(true, &found), GitTabPr::Compact(row) if row.number == "#7"));
        assert_eq!(git_tab_pr(true, &PrView::NoPr), GitTabPr::Create);
        for view in [
            PrView::Hidden,
            PrView::Loading,
            PrView::GhMissing,
            PrView::NotAuthenticated,
            PrView::Error("boom".into()),
        ] {
            assert_eq!(git_tab_pr(true, &view), GitTabPr::Nothing, "{view:?}");
        }
        assert_eq!(git_tab_pr(false, &found), GitTabPr::Nothing);
        assert_eq!(git_tab_pr(false, &PrView::NoPr), GitTabPr::Nothing);
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
}
