//! State and policy of the PR tab's checks list: which rows are expanded, cached details, and when
//! the rollup is fetched again. Pure, so it is unit-tested; the store only runs the `gh` work.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use sich_git::forge::{CheckDetails, CheckRow, ChecksRollup, PullRequest, RowState, checks_poll_interval};

use super::model::PrView;

/// A failed rollup fetch is retried after this long.
pub(crate) const ERROR_RETRY: Duration = Duration::from_secs(60);

#[derive(Clone, Debug)]
pub(crate) enum DetailState {
    Loading,
    Loaded(Arc<CheckDetails>),
    Failed(String),
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ChecksState {
    /// Sorted by severity.
    pub rows: Vec<CheckRow>,
    pub loaded: bool,
    pub in_flight: bool,
    pub error: Option<String>,
    pub last_fetch: Option<Instant>,
    /// Polls in a row that found nothing new (backs the poll interval off).
    pub unchanged_polls: u32,
    /// Fetch as soon as possible (the PR changed, an action finished, manual refresh).
    pub wanted: bool,
    /// The summary list is folded.
    pub list_collapsed: bool,
    /// Rows the user opened / closed (overrides the auto-expanded first failure).
    pub open: HashSet<String>,
    pub closed: HashSet<String>,
    pub details: HashMap<String, DetailState>,
}

impl ChecksState {
    pub(crate) fn has_pending(&self) -> bool {
        self.rows.iter().any(|row| row.state == RowState::Pending)
    }

    pub(crate) fn is_expanded(&self, row: &CheckRow) -> bool {
        let key = row.key();
        if self.closed.contains(&key) {
            return false;
        }
        self.open.contains(&key) || auto_expand_key(&self.rows).is_some_and(|auto| auto == key)
    }

    /// Flips the expansion of `row` and returns whether it is expanded now.
    pub(crate) fn toggle(&mut self, row: &CheckRow) -> bool {
        let now_open = !self.is_expanded(row);
        let key = row.key();
        if now_open {
            self.closed.remove(&key);
            self.open.insert(key);
        } else {
            self.open.remove(&key);
            self.closed.insert(key);
        }
        now_open
    }

    /// Rows that are expanded but have no details yet (or failed to load them).
    pub(crate) fn rows_needing_details(&self) -> Vec<CheckRow> {
        self.rows
            .iter()
            .filter(|row| self.is_expanded(row))
            .filter(|row| matches!(self.details.get(&row.key()), None | Some(DetailState::Failed(_))))
            .cloned()
            .collect()
    }
}

/// The first failed row opens by itself.
pub(crate) fn auto_expand_key(rows: &[CheckRow]) -> Option<String> {
    rows.iter().find(|row| row.state.is_rerunnable_failure() && !row.pending_approval).map(CheckRow::key)
}

/// Whether the rollup should be fetched now.
pub(crate) fn should_fetch(state: &ChecksState, now: Instant, visible: bool) -> bool {
    if !visible || state.in_flight {
        return false;
    }
    if state.wanted {
        return true;
    }
    let Some(last) = state.last_fetch else { return true };
    let elapsed = now.saturating_duration_since(last);
    if state.error.is_some() {
        elapsed >= ERROR_RETRY
    } else if state.has_pending() {
        elapsed >= checks_poll_interval(state.unchanged_polls)
    } else {
        false
    }
}

/// Stores a fresh rollup. Details of rows that changed state are dropped so they are read again.
pub(crate) fn apply_rollup(state: &mut ChecksState, rollup: ChecksRollup, now: Instant) {
    let unchanged = state.loaded && state.rows == rollup.rows;
    state.unchanged_polls = if unchanged { state.unchanged_polls.saturating_add(1) } else { 0 };
    for old in &state.rows {
        let same = rollup
            .rows
            .iter()
            .any(|new| new.key() == old.key() && new.state == old.state && new.completed_at == old.completed_at);
        if !same {
            state.details.remove(&old.key());
        }
    }
    state.rows = rollup.rows;
    state.loaded = true;
    state.error = None;
    state.last_fetch = Some(now);
}

pub(crate) fn apply_error(state: &mut ChecksState, message: String, now: Instant) {
    state.error = Some(message);
    state.last_fetch = Some(now);
}

/// How a fresh PR differs from the one the checks list was built for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PrChange {
    Same,
    /// Same PR, new commit or different check counts: fetch the rollup again.
    Moved,
    /// Another PR (or none before): drop everything.
    Replaced,
}

pub(crate) fn pr_change(previous: &PrView, new: &PrView) -> PrChange {
    let (Some(old), Some(new)) = (previous.pr(), new.pr()) else {
        return if new.pr().is_some() { PrChange::Replaced } else { PrChange::Same };
    };
    if old.number != new.number {
        PrChange::Replaced
    } else if signature(old) != signature(new) {
        PrChange::Moved
    } else {
        PrChange::Same
    }
}

fn signature(pr: &PullRequest) -> (String, sich_git::forge::ChecksSummary, bool) {
    (pr.head_oid.clone(), pr.summary(), pr.is_open())
}

#[cfg(test)]
mod tests {
    use sich_git::forge::{PrState, RowState};

    use super::super::testutil::{check, pr};
    use super::*;

    fn row(name: &str, state: RowState) -> CheckRow {
        CheckRow {
            name: name.into(),
            workflow: None,
            state,
            check_run_id: Some(name.len() as u64),
            workflow_run_id: None,
            url: None,
            started_at: None,
            completed_at: None,
            pending_approval: false,
        }
    }

    fn rollup(rows: Vec<CheckRow>) -> ChecksRollup {
        ChecksRollup { head_oid: "abc".into(), rows }
    }

    #[test]
    fn first_failure_is_auto_expanded_until_the_user_closes_it() {
        let mut state = ChecksState::default();
        apply_rollup(&mut state, rollup(vec![row("bad", RowState::Failure), row("worse", RowState::TimedOut), row("ok", RowState::Success)]), Instant::now());
        assert!(state.is_expanded(&state.rows[0].clone()));
        assert!(!state.is_expanded(&state.rows[1].clone()));
        assert!(!state.is_expanded(&state.rows[2].clone()));
        let first = state.rows[0].clone();
        assert!(!state.toggle(&first));
        assert!(!state.is_expanded(&first));
        assert!(state.toggle(&first));
        assert!(state.is_expanded(&first));
        let third = state.rows[2].clone();
        assert!(state.toggle(&third));
        assert!(state.is_expanded(&third));
    }

    #[test]
    fn no_auto_expand_without_a_failure_or_for_approval_rows() {
        assert_eq!(auto_expand_key(&[row("ok", RowState::Success), row("run", RowState::Pending)]), None);
        let mut approval = row("wait", RowState::ActionRequired);
        approval.pending_approval = true;
        assert_eq!(auto_expand_key(&[approval]), None);
        assert_eq!(auto_expand_key(&[row("c", RowState::Cancelled)]), Some(row("c", RowState::Cancelled).key()));
    }

    #[test]
    fn expanded_rows_without_details_need_them() {
        let mut state = ChecksState::default();
        apply_rollup(&mut state, rollup(vec![row("bad", RowState::Failure)]), Instant::now());
        assert_eq!(state.rows_needing_details().len(), 1);
        let key = state.rows[0].key();
        state.details.insert(key.clone(), DetailState::Loading);
        assert!(state.rows_needing_details().is_empty());
        state.details.insert(key, DetailState::Failed("x".into()));
        assert_eq!(state.rows_needing_details().len(), 1, "failed loads are retried");
    }

    #[test]
    fn fetch_policy() {
        let now = Instant::now();
        let later = |secs| now + Duration::from_secs(secs);
        let mut state = ChecksState::default();
        assert!(!should_fetch(&state, now, false), "hidden tab never fetches");
        assert!(should_fetch(&state, now, true), "first show fetches");
        state.in_flight = true;
        assert!(!should_fetch(&state, now, true));
        state.in_flight = false;
        apply_rollup(&mut state, rollup(vec![row("run", RowState::Pending)]), now);
        assert!(!should_fetch(&state, later(59), true));
        assert!(should_fetch(&state, later(60), true), "pending polls every 60 s");
        apply_rollup(&mut state, rollup(vec![row("run", RowState::Pending)]), now);
        assert_eq!(state.unchanged_polls, 1);
        assert!(!should_fetch(&state, later(119), true));
        assert!(should_fetch(&state, later(120), true), "backs off to 120 s when unchanged");
        apply_rollup(&mut state, rollup(vec![row("run", RowState::Success)]), now);
        assert_eq!(state.unchanged_polls, 0);
        assert!(!should_fetch(&state, later(10_000), true), "nothing pending: no polling");
        state.wanted = true;
        assert!(should_fetch(&state, now, true), "wanted overrides");
    }

    #[test]
    fn errors_retry_slowly_and_keep_the_rows() {
        let now = Instant::now();
        let mut state = ChecksState::default();
        apply_rollup(&mut state, rollup(vec![row("ok", RowState::Success)]), now);
        apply_error(&mut state, "boom".into(), now);
        assert_eq!(state.rows.len(), 1);
        assert!(!should_fetch(&state, now + Duration::from_secs(59), true));
        assert!(should_fetch(&state, now + ERROR_RETRY, true));
        apply_rollup(&mut state, rollup(vec![row("ok", RowState::Success)]), now);
        assert_eq!(state.error, None);
    }

    #[test]
    fn details_of_changed_rows_are_dropped() {
        let now = Instant::now();
        let mut state = ChecksState::default();
        apply_rollup(&mut state, rollup(vec![row("a", RowState::Pending), row("bb", RowState::Failure)]), now);
        for r in state.rows.clone() {
            state.details.insert(r.key(), DetailState::Loading);
        }
        apply_rollup(&mut state, rollup(vec![row("a", RowState::Failure), row("bb", RowState::Failure)]), now);
        assert!(!state.details.contains_key(&row("a", RowState::Pending).key()));
        assert!(state.details.contains_key(&row("bb", RowState::Failure).key()));
    }

    #[test]
    fn pr_change_classification() {
        let view = |p: PullRequest| PrView::Pr(Box::new(p));
        let base = pr(PrState::Open);
        assert_eq!(pr_change(&PrView::Loading, &PrView::NoPr), PrChange::Same);
        assert_eq!(pr_change(&PrView::NoPr, &view(base.clone())), PrChange::Replaced);
        assert_eq!(pr_change(&view(base.clone()), &view(base.clone())), PrChange::Same);
        let mut moved = base.clone();
        moved.head_oid = "def".into();
        assert_eq!(pr_change(&view(base.clone()), &view(moved)), PrChange::Moved);
        let mut counts = base.clone();
        counts.checks = vec![check(sich_git::forge::CheckState::Failure)];
        assert_eq!(pr_change(&view(base.clone()), &view(counts)), PrChange::Moved);
        let mut other = base.clone();
        other.number = 8;
        assert_eq!(pr_change(&view(base.clone()), &view(other)), PrChange::Replaced);
        assert_eq!(pr_change(&view(base), &PrView::Error("x".into())), PrChange::Same);
    }
}
