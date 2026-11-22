//! Refresh policy of the PR store: how often a worktree's pull request is re-read and which
//! worktrees may start a `gh` fetch right now. Everything here is pure so it is unit-tested; the
//! store only feeds it the current state and executes the plan.

use std::time::{Duration, Instant};

use sich_git::forge::PrState;

use super::model::{PrEntry, PrKey, PrView};

/// Minimum time between two `gh` fetches of one worktree caused by anything but one of our own
/// actions.
pub(crate) const COOLDOWN: Duration = Duration::from_secs(10);
/// At most this many `gh` fetches run at once, over all worktrees. Our own PR actions (merge,
/// push, create...) are user-initiated and bypass the cap.
pub(crate) const MAX_IN_FLIGHT: usize = 3;
/// Target resolving is only local git calls, but a burst over many rows is still capped.
pub(crate) const MAX_RESOLVING: usize = 4;
/// A cached target (push remote, GitHub repo, default base) is re-read after this long.
pub(crate) const RESOLVE_TTL: Duration = Duration::from_secs(15 * 60);
/// How often the store timer re-evaluates the plan.
pub(crate) const TICK: Duration = Duration::from_secs(3);

const MIN: u64 = 60;

/// Whether the worktree is the selected one (shown in the right panel) or only a sidebar row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Role {
    Selected,
    Background,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct FetchState {
    pub in_flight: bool,
    /// An action finished while a fetch was running: fetch again afterwards.
    pub queued: bool,
    pub last_fetch: Option<Instant>,
    pub last_key: Option<PrKey>,
    /// A fetch somebody asked for that could not start yet (cooldown, concurrency cap).
    pub wanted: Option<PrTrigger>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PrTrigger {
    /// The section was first shown for this worktree (or the cache was invalidated).
    Shown,
    /// A status refresh saw a different branch / upstream / ahead / head.
    Changed,
    Periodic,
    /// Manual refresh button.
    Manual,
    /// One of our own PR/push actions finished: bypasses the cooldown and the concurrency cap.
    Action,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    Fetch,
    /// Remember to fetch again as soon as the running fetch finishes.
    Queue,
    Skip,
}

pub(crate) fn decide(trigger: PrTrigger, now: Instant, state: &FetchState) -> Decision {
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
///
/// Open/draft: selected 60 s while checks run, else 2 min; background rows 2 min / 5 min.
/// No PR: 5 min selected, 15 min background. Closed: 15 min. Merged: never. Errors: 5 min selected,
/// 15 min background; a missing or unauthenticated `gh` is never retried for background rows.
pub(crate) fn poll_interval(view: &PrView, role: Role) -> Option<Duration> {
    let selected = role == Role::Selected;
    let secs = |s: u64| Some(Duration::from_secs(s));
    match view {
        PrView::Hidden | PrView::Loading => None,
        PrView::Pr(pr) => match pr.state {
            PrState::Merged => None,
            PrState::Closed => secs(15 * MIN),
            PrState::Open | PrState::Draft => {
                let pending = pr.summary().pending > 0;
                match (selected, pending) {
                    (true, true) => secs(MIN),
                    (true, false) | (false, true) => secs(2 * MIN),
                    (false, false) => secs(5 * MIN),
                }
            }
        },
        PrView::NoPr => secs(if selected { 5 * MIN } else { 15 * MIN }),
        PrView::Error(_) => secs(if selected { 5 * MIN } else { 15 * MIN }),
        PrView::GhMissing | PrView::NotAuthenticated => selected.then(|| Duration::from_secs(5 * MIN)),
    }
}

/// Which trigger (if any) applies to a status refresh or timer tick.
pub(crate) fn sync_trigger(
    now: Instant,
    state: &FetchState,
    key: &PrKey,
    view: &PrView,
    role: Role,
) -> Option<PrTrigger> {
    let Some(last) = state.last_fetch else {
        return Some(PrTrigger::Shown);
    };
    if state.last_key.as_ref().is_none_or(|previous| previous.differs_from(key)) {
        return Some(PrTrigger::Changed);
    }
    let interval = poll_interval(view, role)?;
    (now.saturating_duration_since(last) >= interval).then_some(PrTrigger::Periodic)
}

// ---------------------------------------------------------------------------------------------
// Plans
// ---------------------------------------------------------------------------------------------

/// One tracked worktree as the planner sees it.
pub(crate) struct PlanRow<'a> {
    pub role: Role,
    pub entry: &'a PrEntry,
}

/// The cached target is missing, stale, for another branch, or a refresh asked for a new one.
pub(crate) fn wants_resolve(entry: &PrEntry, now: Instant) -> bool {
    let Some(branch) = entry.branch() else { return false };
    if entry.resolving {
        return false;
    }
    entry.force_resolve
        || entry.resolved_branch.as_deref() != Some(branch)
        || entry
            .resolved_at
            .is_none_or(|at| now.saturating_duration_since(at) >= RESOLVE_TTL)
}

/// Indices of the rows to resolve now, in row order (callers list the selected worktree first).
pub(crate) fn plan_resolves(now: Instant, rows: &[PlanRow], max_in_flight: usize) -> Vec<usize> {
    let running = rows.iter().filter(|row| row.entry.resolving).count();
    rows.iter()
        .enumerate()
        .filter(|(_, row)| wants_resolve(row.entry, now))
        .map(|(index, _)| index)
        .take(max_in_flight.saturating_sub(running))
        .collect()
}

/// The trigger under which this row may fetch right now, if any.
pub(crate) fn fetch_trigger(now: Instant, row: &PlanRow, gh_blocked: bool) -> Option<PrTrigger> {
    let entry = row.entry;
    let key = entry.key()?;
    let branch = entry.branch()?;
    entry.target.as_ref()?;
    if entry.resolving
        || entry.resolved_branch.as_deref() != Some(branch)
        || entry.on_default_branch()
        || (row.role == Role::Background && gh_blocked)
    {
        return None;
    }
    let trigger = entry
        .fetch
        .wanted
        .or_else(|| sync_trigger(now, &entry.fetch, &key, &entry.view, row.role))?;
    (decide(trigger, now, &entry.fetch) == Decision::Fetch).then_some(trigger)
}

/// Which rows start a `gh` fetch now, with the trigger. Own actions always start; the rest is
/// ordered selected first, then the longest-waiting row, and limited so that at most `cap`
/// fetches run overall.
pub(crate) fn plan_fetches(
    now: Instant,
    rows: &[PlanRow],
    gh_blocked: bool,
    cap: usize,
) -> Vec<(usize, PrTrigger)> {
    let in_flight = rows.iter().filter(|row| row.entry.fetch.in_flight).count();
    let mut due: Vec<(usize, PrTrigger)> = rows
        .iter()
        .enumerate()
        .filter_map(|(index, row)| fetch_trigger(now, row, gh_blocked).map(|t| (index, t)))
        .collect();
    due.sort_by_key(|&(index, trigger)| {
        let row = &rows[index];
        (
            trigger != PrTrigger::Action,
            row.role != Role::Selected,
            row.entry.fetch.last_fetch.is_some(),
            row.entry.fetch.last_fetch,
            index,
        )
    });
    let actions = due.iter().filter(|(_, trigger)| *trigger == PrTrigger::Action).count();
    let budget = cap.saturating_sub(in_flight + actions);
    let mut others = 0;
    due.retain(|(_, trigger)| {
        if *trigger == PrTrigger::Action {
            return true;
        }
        others += 1;
        others <= budget
    });
    due
}

#[cfg(test)]
mod tests {
    use sich_git::forge::{CheckState, PullRequest};

    use super::super::model::PrTarget;
    use super::super::testutil::{check, pr, remote};
    use super::*;

    const SEC: fn(u64) -> Duration = Duration::from_secs;

    fn open_with(checks: Vec<CheckState>) -> PrView {
        let mut p: PullRequest = pr(PrState::Open);
        p.checks = checks.into_iter().map(check).collect();
        PrView::Pr(Box::new(p))
    }

    fn view_in(state: PrState) -> PrView {
        PrView::Pr(Box::new(pr(state)))
    }

    #[test]
    fn decide_applies_cooldown_to_everything_but_actions() {
        let now = Instant::now();
        let mut state = FetchState::default();
        for trigger in [PrTrigger::Shown, PrTrigger::Changed, PrTrigger::Periodic, PrTrigger::Manual, PrTrigger::Action] {
            assert_eq!(decide(trigger, now, &state), Decision::Fetch, "{trigger:?} on a fresh state");
        }
        state.last_fetch = Some(now);
        let soon = now + SEC(3);
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
    fn selected_open_pr_polls_every_minute_while_checks_run_else_two_minutes() {
        let running = open_with(vec![CheckState::Pending, CheckState::Success]);
        let passed = open_with(vec![CheckState::Success]);
        assert_eq!(poll_interval(&running, Role::Selected), Some(SEC(60)));
        assert_eq!(poll_interval(&passed, Role::Selected), Some(SEC(120)));
        assert_eq!(poll_interval(&open_with(vec![]), Role::Selected), Some(SEC(120)));
        let mut draft = pr(PrState::Draft);
        draft.checks = vec![check(CheckState::Pending)];
        assert_eq!(poll_interval(&PrView::Pr(Box::new(draft)), Role::Selected), Some(SEC(60)));
    }

    #[test]
    fn background_open_pr_polls_every_two_minutes_while_checks_run_else_five() {
        let running = open_with(vec![CheckState::Pending]);
        let passed = open_with(vec![CheckState::Success]);
        assert_eq!(poll_interval(&running, Role::Background), Some(SEC(120)));
        assert_eq!(poll_interval(&passed, Role::Background), Some(SEC(300)));
        assert_eq!(poll_interval(&view_in(PrState::Draft), Role::Background), Some(SEC(300)));
    }

    #[test]
    fn no_pr_closed_merged_and_unknown_states() {
        assert_eq!(poll_interval(&PrView::NoPr, Role::Selected), Some(SEC(300)));
        assert_eq!(poll_interval(&PrView::NoPr, Role::Background), Some(SEC(900)));
        for role in [Role::Selected, Role::Background] {
            assert_eq!(poll_interval(&view_in(PrState::Closed), role), Some(SEC(900)));
            assert_eq!(poll_interval(&view_in(PrState::Merged), role), None);
            assert_eq!(poll_interval(&PrView::Hidden, role), None);
            assert_eq!(poll_interval(&PrView::Loading, role), None);
        }
    }

    #[test]
    fn errors_back_off_and_a_broken_gh_stops_background_rows() {
        let error = PrView::Error("boom".into());
        assert_eq!(poll_interval(&error, Role::Selected), Some(SEC(300)));
        assert_eq!(poll_interval(&error, Role::Background), Some(SEC(900)));
        assert_eq!(poll_interval(&PrView::GhMissing, Role::Selected), Some(SEC(300)));
        assert_eq!(poll_interval(&PrView::NotAuthenticated, Role::Selected), Some(SEC(300)));
        assert_eq!(poll_interval(&PrView::GhMissing, Role::Background), None);
        assert_eq!(poll_interval(&PrView::NotAuthenticated, Role::Background), None);
    }

    #[test]
    fn sync_trigger_covers_first_show_key_change_and_polling() {
        let now = Instant::now();
        let key = PrKey { branch: "feat".into(), upstream: Some("origin/feat".into()), ahead: 0, head_oid: Some("a".into()) };
        let mut state = FetchState::default();
        let sel = Role::Selected;
        assert_eq!(sync_trigger(now, &state, &key, &PrView::Loading, sel), Some(PrTrigger::Shown));

        state.last_fetch = Some(now);
        state.last_key = Some(key.clone());
        assert_eq!(sync_trigger(now, &state, &key, &PrView::NoPr, sel), None);

        let mut moved = key.clone();
        moved.ahead = 2;
        assert_eq!(sync_trigger(now, &state, &moved, &PrView::NoPr, sel), Some(PrTrigger::Changed));

        let open = open_with(vec![CheckState::Success]);
        assert_eq!(sync_trigger(now + SEC(119), &state, &key, &open, sel), None);
        assert_eq!(sync_trigger(now + SEC(120), &state, &key, &open, sel), Some(PrTrigger::Periodic));
        assert_eq!(sync_trigger(now + SEC(120), &state, &key, &open, Role::Background), None);
        assert_eq!(sync_trigger(now + SEC(300), &state, &key, &open, Role::Background), Some(PrTrigger::Periodic));

        let pending = open_with(vec![CheckState::Pending]);
        assert_eq!(sync_trigger(now + SEC(60), &state, &key, &pending, sel), Some(PrTrigger::Periodic));

        let merged = view_in(PrState::Merged);
        assert_eq!(sync_trigger(now + SEC(86_400), &state, &key, &merged, sel), None);
        // A branch change still refreshes a merged PR's cache.
        assert_eq!(sync_trigger(now, &state, &moved, &merged, sel), Some(PrTrigger::Changed));
        // A branch-only key (panel closed) is not a push.
        let partial = PrKey::branch_only("feat");
        assert_eq!(sync_trigger(now, &state, &partial, &PrView::NoPr, sel), None);
        assert_eq!(sync_trigger(now, &state, &PrKey::branch_only("x"), &PrView::NoPr, sel), Some(PrTrigger::Changed));
    }

    // ---- plans -------------------------------------------------------------------------------

    fn resolved(branch: &str, view: PrView) -> PrEntry {
        PrEntry {
            target: Some(PrTarget { remote: "origin".into(), repo: remote(), base: Some("main".into()) }),
            resolved_branch: Some(branch.into()),
            resolved_at: Some(Instant::now()),
            row_branch: Some(branch.into()),
            view,
            ..PrEntry::default()
        }
    }

    fn rows<'a>(entries: &'a [(Role, PrEntry)]) -> Vec<PlanRow<'a>> {
        entries.iter().map(|(role, entry)| PlanRow { role: *role, entry }).collect()
    }

    #[test]
    fn rows_without_a_cached_target_for_their_branch_are_resolved() {
        let now = Instant::now();
        let fresh = PrEntry { row_branch: Some("feat".into()), ..PrEntry::default() };
        let mut stale_branch = resolved("old", PrView::NoPr);
        stale_branch.row_branch = Some("new".into());
        let mut forced = resolved("feat", PrView::NoPr);
        forced.force_resolve = true;
        let ok = resolved("feat", PrView::NoPr);
        let mut running = resolved("feat", PrView::NoPr);
        running.force_resolve = true;
        running.resolving = true;
        let no_branch = PrEntry::default();
        let entries = [
            (Role::Selected, fresh),
            (Role::Background, stale_branch),
            (Role::Background, forced),
            (Role::Background, ok),
            (Role::Background, running),
            (Role::Background, no_branch),
        ];
        let rows = rows(&entries);
        assert_eq!(plan_resolves(now, &rows, 10), vec![0, 1, 2]);
        assert_eq!(plan_resolves(now, &rows, 2), vec![0], "one is already running");
        assert_eq!(plan_resolves(now, &rows, 1), Vec::<usize>::new());
    }

    #[test]
    fn a_resolved_target_is_refreshed_after_fifteen_minutes() {
        let now = Instant::now();
        let entry = resolved("feat", PrView::NoPr);
        assert!(!wants_resolve(&entry, now + SEC(14 * 60)));
        assert!(wants_resolve(&entry, now + RESOLVE_TTL + SEC(1)));
        let mut never = resolved("feat", PrView::NoPr);
        never.resolved_at = None;
        assert!(wants_resolve(&never, now));
    }

    #[test]
    fn the_selected_worktree_fetches_first_and_the_cap_is_three() {
        let now = Instant::now();
        let entries: Vec<_> = (0..6)
            .map(|i| (if i == 4 { Role::Selected } else { Role::Background }, resolved(&format!("b{i}"), PrView::Loading)))
            .collect();
        let rows = rows(&entries);
        let plan = plan_fetches(now, &rows, false, MAX_IN_FLIGHT);
        assert_eq!(plan.len(), 3);
        assert_eq!(plan[0], (4, PrTrigger::Shown), "selected first");
        assert_eq!(plan.iter().map(|(i, _)| *i).collect::<Vec<_>>(), vec![4, 0, 1]);
    }

    #[test]
    fn running_fetches_count_against_the_cap() {
        let now = Instant::now();
        let mut busy = resolved("a", PrView::Loading);
        busy.fetch.in_flight = true;
        let mut busy2 = resolved("b", PrView::Loading);
        busy2.fetch.in_flight = true;
        let entries = [
            (Role::Background, busy),
            (Role::Background, busy2),
            (Role::Background, resolved("c", PrView::Loading)),
            (Role::Background, resolved("d", PrView::Loading)),
        ];
        let rows = rows(&entries);
        assert_eq!(plan_fetches(now, &rows, false, 3), vec![(2, PrTrigger::Shown)]);
        assert!(plan_fetches(now, &rows, false, 2).is_empty());
    }

    #[test]
    fn own_actions_bypass_the_cap_and_the_cooldown() {
        let now = Instant::now();
        let mut acting = resolved("a", PrView::NoPr);
        acting.fetch.wanted = Some(PrTrigger::Action);
        acting.fetch.last_fetch = Some(now);
        acting.fetch.last_key = acting.key();
        let entries = [
            (Role::Background, resolved("b", PrView::Loading)),
            (Role::Background, acting),
        ];
        let rows = rows(&entries);
        assert_eq!(plan_fetches(now, &rows, false, 1), vec![(1, PrTrigger::Action)]);
        assert_eq!(
            plan_fetches(now, &rows, false, 2),
            vec![(1, PrTrigger::Action), (0, PrTrigger::Shown)]
        );
    }

    #[test]
    fn longest_waiting_background_row_goes_first() {
        let now = Instant::now();
        let mut recent = resolved("a", PrView::NoPr);
        recent.fetch.last_fetch = Some(now - SEC(100));
        recent.fetch.last_key = recent.key();
        let mut old = resolved("b", PrView::NoPr);
        old.fetch.last_fetch = Some(now - SEC(2000));
        old.fetch.last_key = old.key();
        let never = resolved("c", PrView::NoPr);
        let entries = [(Role::Background, recent), (Role::Background, old), (Role::Background, never)];
        let rows = rows(&entries);
        let plan = plan_fetches(now, &rows, false, 3);
        assert_eq!(plan.iter().map(|(i, _)| *i).collect::<Vec<_>>(), vec![2, 1], "recent is not due yet");
    }

    #[test]
    fn default_branch_unresolved_and_branchless_rows_never_fetch() {
        let now = Instant::now();
        let on_main = resolved("main", PrView::NoPr);
        let mut unresolved = resolved("feat", PrView::Loading);
        unresolved.resolved_branch = Some("other".into());
        let mut no_target = resolved("feat", PrView::Hidden);
        no_target.target = None;
        let mut resolving = resolved("feat", PrView::Loading);
        resolving.resolving = true;
        let mut detached = resolved("feat", PrView::Loading);
        detached.row_branch = None;
        let entries = [
            (Role::Background, on_main),
            (Role::Background, unresolved),
            (Role::Background, no_target),
            (Role::Background, resolving),
            (Role::Background, detached),
        ];
        assert!(plan_fetches(now, &rows(&entries), false, 3).is_empty());
    }

    #[test]
    fn a_broken_gh_stops_background_rows_but_not_the_selected_one() {
        let now = Instant::now();
        let entries = [
            (Role::Selected, resolved("a", PrView::Loading)),
            (Role::Background, resolved("b", PrView::Loading)),
        ];
        let rows = rows(&entries);
        assert_eq!(plan_fetches(now, &rows, true, 3), vec![(0, PrTrigger::Shown)]);
        assert_eq!(plan_fetches(now, &rows, false, 3).len(), 2);
    }

    #[test]
    fn cooldown_keeps_a_wanted_fetch_pending_until_it_passes() {
        let now = Instant::now();
        let mut entry = resolved("a", PrView::NoPr);
        entry.fetch.last_fetch = Some(now);
        entry.fetch.last_key = entry.key();
        entry.fetch.wanted = Some(PrTrigger::Manual);
        let entries = [(Role::Selected, entry)];
        let rows = rows(&entries);
        assert!(plan_fetches(now + SEC(3), &rows, false, 3).is_empty());
        assert_eq!(plan_fetches(now + COOLDOWN, &rows, false, 3), vec![(0, PrTrigger::Manual)]);
    }

    #[test]
    fn a_running_fetch_blocks_the_same_row() {
        let now = Instant::now();
        let mut entry = resolved("a", PrView::Loading);
        entry.fetch.in_flight = true;
        let entries = [(Role::Selected, entry)];
        assert!(plan_fetches(now, &rows(&entries), false, 3).is_empty());
    }
}
