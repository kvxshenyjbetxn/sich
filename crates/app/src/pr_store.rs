//! Shared pull request store. One GPUI entity owned by `AppView` keeps the pull request state of
//! every worktree that matters right now (the selected one plus every row the sidebar shows) and is
//! read by the Git panel, the right panel's tabs and the sidebar glyphs.
//!
//! Everything goes through the user's own `gh` CLI (`sich_git::forge`); Sich never reads or stores
//! GitHub credentials. The policy (what to fetch, how often, how many at once) lives in the pure
//! `schedule` module; this file only feeds it the current state and runs the resulting plan on a
//! background executor.

mod agent;
mod checks;
mod glyph;
mod model;
mod schedule;
#[cfg(test)]
pub(crate) mod testutil;
mod tracked;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use gpui_kit::{AppContext as _, Context, EventEmitter, Task};
use sich_git::forge::{
    CheckRow, ChecksRollup, CreatedPr, FailedJob, ForgeError, GitHub, MergeOptions, RemoteRepo,
};
use sich_git::Repo;

pub(crate) use checks::{ChecksState, DetailState};
pub(crate) use glyph::{Glyph, Tone, glyph, glyph_of_pr, tone_color};
pub(crate) use model::{PrAction, PrEntry, PrKey, PrTarget, PrView, is_conflicting, pr_key, section_visible};
pub(crate) use schedule::PrTrigger;
pub(crate) use tracked::{TrackedRow, tracked_worktrees};

use model::{Fetched, divergence, fetch_pr, merge_result, resolve_target, result_is_current, run_action};
use schedule::{
    MAX_IN_FLIGHT, MAX_RESOLVING, PlanRow, Role, TICK, plan_fetches, plan_resolves,
};

pub(crate) enum PrStoreEvent {
    /// Text for the user's agent terminal in `worktree`: pasted without Enter.
    SendToAgent { worktree: PathBuf, text: String },
    /// One of our own PR actions finished; after a merge the Git panel re-reads its status.
    ActionFinished { worktree: PathBuf, merged: bool },
}

#[derive(Default)]
pub(crate) struct PrStore {
    entries: HashMap<PathBuf, PrEntry>,
    selected: Option<PathBuf>,
    tracked: Vec<TrackedRow>,
    /// `github_prs` setting.
    enabled: bool,
    /// False while the main window is hidden in the tray: nothing is fetched then.
    active: bool,
    /// `gh` is missing or not signed in: background rows stop fetching until a manual refresh or a
    /// settings change (a successful fetch of the selected worktree also clears it).
    gh_blocked: bool,
    timer: Option<Task<()>>,
    /// The worktree whose PR tab is on screen: only its detailed checks are fetched and polled.
    checks_visible: Option<PathBuf>,
    /// Merge methods per repository (web URL, lowercase), cached for the session.
    merge_options: HashMap<String, MergeOptionsEntry>,
}

enum MergeOptionsEntry {
    Loading,
    Ready(MergeOptions),
    /// Failed at this time; retried after `MERGE_OPTIONS_RETRY`.
    Failed(Instant),
}

const MERGE_OPTIONS_RETRY: std::time::Duration = std::time::Duration::from_secs(120);

fn repo_key(repo: &RemoteRepo) -> String {
    repo.web_url().to_ascii_lowercase()
}

impl EventEmitter<PrStoreEvent> for PrStore {}

impl PrStore {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn entry(&self, path: &Path) -> Option<&PrEntry> {
        self.entries.get(path)
    }

    /// A resolve or fetch is running for `path`.
    pub(crate) fn fetching(&self, path: &Path) -> bool {
        self.entry(path).is_some_and(|entry| entry.fetch.in_flight || entry.resolving)
    }

    /// Whether the PR section (and so the "Pull request" tab) exists for `path`.
    pub(crate) fn section_visible_for(&self, path: Option<&Path>) -> bool {
        let entry = path.and_then(|path| self.entry(path));
        section_visible(self.enabled, entry.and_then(PrEntry::branch), entry)
    }

    /// Glyph of the pull request of `path`, if the worktree has one and it is not stale.
    pub(crate) fn glyph_for(&self, path: &Path) -> Option<Glyph> {
        if !self.enabled {
            return None;
        }
        let entry = self.entry(path)?;
        if entry.resolved_branch.as_deref() != entry.branch() || entry.on_default_branch() {
            return None;
        }
        glyph(&entry.view)
    }

    // ---- inputs from the app --------------------------------------------------------------------

    /// Tells the store what to track. `rows` are the worktrees the sidebar shows; `selected` is
    /// tracked in addition. Cheap and idempotent: the app calls it every frame.
    pub(crate) fn set_tracking(
        &mut self,
        selected: Option<PathBuf>,
        rows: Vec<TrackedRow>,
        enabled: bool,
        active: bool,
        cx: &mut Context<Self>,
    ) {
        let unchanged = self.selected == selected
            && self.tracked == rows
            && self.enabled == enabled
            && self.active == active;
        if unchanged {
            return;
        }
        if enabled && !self.enabled {
            self.gh_blocked = false;
        }
        self.enabled = enabled;
        self.active = active;
        for row in &rows {
            self.entries.entry(row.path.clone()).or_default().row_branch = row.branch.clone();
        }
        if let Some(path) = &selected {
            self.entries.entry(path.clone()).or_default();
        }
        self.selected = selected;
        self.tracked = rows;
        self.ensure_timer(cx);
        self.schedule(cx);
        cx.notify();
    }

    /// The Git panel read a fresh status of `path` (`None`: detached HEAD).
    pub(crate) fn report_status(&mut self, path: &Path, key: Option<PrKey>, cx: &mut Context<Self>) {
        let entry = self.entries.entry(path.to_path_buf()).or_default();
        let changed = entry.status_key != key;
        entry.status_key = key;
        // Commit counts against the base are cheap git calls, refreshed together with the status.
        if entry.view == PrView::NoPr && !entry.div_in_flight && entry.target.is_some() {
            self.start_divergence(path.to_path_buf(), cx);
        }
        if changed {
            cx.notify();
        }
        self.schedule(cx);
    }

    /// The Git panel stopped showing `path`: its status no longer overrides the sidebar's branch.
    pub(crate) fn clear_status(&mut self, path: &Path, cx: &mut Context<Self>) {
        if let Some(entry) = self.entries.get_mut(path)
            && entry.status_key.take().is_some()
        {
            cx.notify();
        }
    }

    /// Manual refresh: resolve the remote again and fetch (the 10 s cooldown still applies unless
    /// `trigger` is `Action`). Also lifts the "gh is broken" stop of background rows.
    pub(crate) fn refresh(&mut self, path: &Path, trigger: PrTrigger, cx: &mut Context<Self>) {
        if !self.enabled {
            return;
        }
        self.gh_blocked = false;
        let entry = self.entries.entry(path.to_path_buf()).or_default();
        entry.force_resolve = true;
        entry.fetch.wanted = Some(trigger);
        entry.checks.wanted = true;
        cx.notify();
        self.schedule(cx);
    }

    // ---- checks tab ----------------------------------------------------------------------------

    /// The PR tab of `path` is on screen (called on every render, cheap): detailed checks and merge
    /// options are fetched and kept fresh for it.
    pub(crate) fn pr_tab_shown(&mut self, path: &Path, cx: &mut Context<Self>) {
        if self.checks_visible.as_deref() == Some(path) {
            return;
        }
        self.checks_visible = Some(path.to_path_buf());
        self.schedule_checks(cx);
    }

    /// The PR tab is not on screen any more: polling stops.
    pub(crate) fn pr_tab_hidden(&mut self) {
        self.checks_visible = None;
    }

    pub(crate) fn merge_options_for(&self, path: &Path) -> Option<MergeOptions> {
        let target = self.entry(path)?.target.as_ref()?;
        match self.merge_options.get(&repo_key(&target.repo))? {
            MergeOptionsEntry::Ready(options) => Some(options.clone()),
            _ => None,
        }
    }

    fn schedule_checks(&mut self, cx: &mut Context<Self>) {
        if !self.enabled || !self.active {
            return;
        }
        let Some(path) = self.checks_visible.clone() else { return };
        let Some(entry) = self.entries.get(&path) else { return };
        let Some(pr) = entry.view.pr() else { return };
        let allowed = pr.is_open() || !entry.checks.loaded;
        if checks::should_fetch(&entry.checks, Instant::now(), allowed) {
            self.start_checks(path.clone(), cx);
        }
        self.ensure_merge_options(&path, cx);
    }

    fn start_checks(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get_mut(&path) else { return };
        let (Some(target), Some(pr)) = (entry.target.clone(), entry.view.pr()) else { return };
        let number = pr.number;
        entry.checks.in_flight = true;
        entry.checks.wanted = false;
        let dir = path.clone();
        let job = cx.background_spawn(async move {
            GitHub::new(dir, target.repo.clone()).check_rollup(number).map_err(|error| error.to_string())
        });
        cx.spawn(async move |store, cx| {
            let result = job.await;
            let _ = store.update(cx, |store, cx| store.checks_fetched(path, number, result, cx));
        })
        .detach();
        cx.notify();
    }

    fn checks_fetched(
        &mut self,
        path: PathBuf,
        number: u64,
        result: Result<ChecksRollup, String>,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.entries.get_mut(&path) else { return };
        entry.checks.in_flight = false;
        if entry.view.pr().map(|pr| pr.number) == Some(number) {
            let now = Instant::now();
            match result {
                Ok(rollup) => checks::apply_rollup(&mut entry.checks, rollup, now),
                Err(message) => checks::apply_error(&mut entry.checks, message, now),
            }
        }
        self.load_missing_details(&path, cx);
        cx.notify();
        self.schedule(cx);
    }

    /// Fetches the details of every expanded row that has none.
    fn load_missing_details(&mut self, path: &Path, cx: &mut Context<Self>) {
        let rows = self.entries.get(path).map(|entry| entry.checks.rows_needing_details()).unwrap_or_default();
        for row in rows {
            self.start_details(path.to_path_buf(), row, cx);
        }
    }

    fn start_details(&mut self, path: PathBuf, row: CheckRow, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get_mut(&path) else { return };
        let Some(target) = entry.target.clone() else { return };
        let key = row.key();
        entry.checks.details.insert(key.clone(), DetailState::Loading);
        let dir = path.clone();
        let job = cx.background_spawn(async move {
            GitHub::new(dir, target.repo.clone()).check_details(&row).map_err(|error| error.to_string())
        });
        cx.spawn(async move |store, cx| {
            let result = job.await;
            let _ = store.update(cx, |store, cx| {
                if let Some(entry) = store.entries.get_mut(&path) {
                    // The row may have disappeared or been dropped (it changed) meanwhile.
                    if matches!(entry.checks.details.get(&key), Some(DetailState::Loading)) {
                        let state = match result {
                            Ok(details) => DetailState::Loaded(std::sync::Arc::new(details)),
                            Err(message) => DetailState::Failed(message),
                        };
                        entry.checks.details.insert(key, state);
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Expands or folds one check; expanding loads its details.
    pub(crate) fn toggle_check(&mut self, path: &Path, row: &CheckRow, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get_mut(path) else { return };
        entry.checks.toggle(row);
        self.load_missing_details(path, cx);
        cx.notify();
    }

    pub(crate) fn toggle_checks_list(&mut self, path: &Path, cx: &mut Context<Self>) {
        if let Some(entry) = self.entries.get_mut(path) {
            entry.checks.list_collapsed = !entry.checks.list_collapsed;
            cx.notify();
        }
    }

    fn ensure_merge_options(&mut self, path: &Path, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get(path) else { return };
        let (Some(target), Some(pr)) = (entry.target.clone(), entry.view.pr()) else { return };
        if !pr.is_open() {
            return;
        }
        let key = repo_key(&target.repo);
        match self.merge_options.get(&key) {
            Some(MergeOptionsEntry::Loading | MergeOptionsEntry::Ready(_)) => return,
            Some(MergeOptionsEntry::Failed(at)) if at.elapsed() < MERGE_OPTIONS_RETRY => return,
            _ => {}
        }
        self.merge_options.insert(key.clone(), MergeOptionsEntry::Loading);
        let dir = path.to_path_buf();
        let job = cx.background_spawn(async move { GitHub::new(dir, target.repo.clone()).merge_options() });
        cx.spawn(async move |store, cx| {
            let result = job.await;
            let _ = store.update(cx, |store, cx| {
                let entry = match result {
                    Ok(options) => MergeOptionsEntry::Ready(options),
                    Err(_) => MergeOptionsEntry::Failed(Instant::now()),
                };
                store.merge_options.insert(key, entry);
                cx.notify();
            });
        })
        .detach();
    }

    /// Writes a report of the local conflicts of `path` and emits the one-line prompt for the agent.
    pub(crate) fn send_conflicts_to_agent(&mut self, path: &Path, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get_mut(path) else { return };
        let (Some(pr), Some(Ok(summary))) = (entry.view.pr().cloned(), entry.conflicts.clone()) else {
            return;
        };
        let path = path.to_path_buf();
        let job = cx.background_spawn(async move {
            let dir = agent::agent_prompts_dir().ok_or_else(|| "No local data directory for the report".to_owned())?;
            let unix_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis());
            agent::prepare_conflicts_line(&pr, &summary, &dir, unix_ms)
        });
        cx.spawn(async move |store, cx| {
            let result = job.await;
            let _ = store.update(cx, |store, cx| match result {
                Ok(text) => cx.emit(PrStoreEvent::SendToAgent { worktree: path, text }),
                Err(message) => store.report_error(&path, message, cx),
            });
        })
        .detach();
    }

    /// One of our own PR/push actions finished: fetch again now, bypassing the cooldown.
    pub(crate) fn request_action_refresh(&mut self, path: &Path, cx: &mut Context<Self>) {
        if let Some(entry) = self.entries.get_mut(path) {
            entry.fetch.wanted = Some(PrTrigger::Action);
            entry.checks.wanted = true;
        }
        self.schedule(cx);
    }

    // ---- scheduling -----------------------------------------------------------------------------

    fn ensure_timer(&mut self, cx: &mut Context<Self>) {
        if self.timer.is_some() {
            return;
        }
        self.timer = Some(cx.spawn(async move |store, cx| {
            loop {
                cx.background_executor().timer(TICK).await;
                if store.update(cx, |store, cx| store.schedule(cx)).is_err() {
                    break;
                }
            }
        }));
    }

    /// Tracked worktrees, the selected one first.
    fn order(&self) -> Vec<(PathBuf, Role)> {
        let mut order = Vec::new();
        if let Some(path) = self.selected.as_ref().filter(|path| self.entries.contains_key(*path)) {
            order.push((path.clone(), Role::Selected));
        }
        for row in &self.tracked {
            if self.selected.as_ref() != Some(&row.path) && self.entries.contains_key(&row.path) {
                order.push((row.path.clone(), Role::Background));
            }
        }
        order
    }

    fn plan_rows<'a>(&'a self, order: &[(PathBuf, Role)]) -> Vec<PlanRow<'a>> {
        order
            .iter()
            .filter_map(|(path, role)| self.entries.get(path).map(|entry| PlanRow { role: *role, entry }))
            .collect()
    }

    /// Starts whatever the policy allows right now. Cheap when there is nothing to do.
    fn schedule(&mut self, cx: &mut Context<Self>) {
        if !self.enabled || !self.active {
            return;
        }
        let now = Instant::now();
        let order = self.order();
        let resolves = plan_resolves(now, &self.plan_rows(&order), MAX_RESOLVING);
        for index in resolves {
            self.start_resolve(order[index].0.clone(), cx);
        }
        let fetches = plan_fetches(now, &self.plan_rows(&order), self.gh_blocked, MAX_IN_FLIGHT);
        for (index, _) in fetches {
            self.start_fetch(order[index].0.clone(), cx);
        }
        self.schedule_checks(cx);
    }

    fn start_resolve(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get_mut(&path) else { return };
        let Some(branch) = entry.branch().map(str::to_owned) else { return };
        entry.resolving = true;
        entry.force_resolve = false;
        let repo_path = path.clone();
        let job = cx.background_spawn(async move {
            Repo::open(&repo_path).ok().and_then(|repo| resolve_target(&repo))
        });
        cx.spawn(async move |store, cx| {
            let target = job.await;
            let _ = store.update(cx, |store, cx| store.resolved(path, branch, target, cx));
        })
        .detach();
        cx.notify();
    }

    fn resolved(&mut self, path: PathBuf, branch: String, target: Option<PrTarget>, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get_mut(&path) else { return };
        entry.resolving = false;
        let same_branch = entry.resolved_branch.as_deref() == Some(branch.as_str());
        let target_changed = entry.target != target;
        entry.resolved_branch = Some(branch);
        entry.resolved_at = Some(Instant::now());
        entry.target = target;
        if entry.target.is_none() {
            entry.view = PrView::Hidden;
            entry.divergence = None;
            entry.fetch.wanted = None;
        } else if !same_branch || target_changed {
            entry.view = PrView::Loading;
            entry.divergence = None;
            entry.conflicts = None;
            entry.checks = ChecksState::default();
            entry.action_error = None;
            entry.fetch = schedule::FetchState { in_flight: entry.fetch.in_flight, ..Default::default() };
        }
        if entry.on_default_branch() {
            // Nothing to open a pull request from: no fetch, no glyph.
            entry.view = PrView::NoPr;
            entry.fetch.wanted = None;
        }
        cx.notify();
        self.schedule(cx);
    }

    fn start_fetch(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get_mut(&path) else { return };
        let (Some(target), Some(key)) = (entry.target.clone(), entry.key()) else { return };
        let branch = key.branch.clone();
        entry.fetch.in_flight = true;
        entry.fetch.wanted = None;
        entry.fetch.last_key = Some(key);
        let repo_path = path.clone();
        let fetch_branch = branch.clone();
        let job = cx.background_spawn(async move {
            match Repo::open(&repo_path) {
                Ok(repo) => fetch_pr(&repo, &target, &fetch_branch),
                Err(error) => Fetched { view: PrView::Error(error.to_string()), divergence: None, conflicts: None },
            }
        });
        cx.spawn(async move |store, cx| {
            let fetched = job.await;
            let _ = store.update(cx, |store, cx| store.fetched(path, branch, fetched, cx));
        })
        .detach();
        cx.notify();
    }

    fn fetched(&mut self, path: PathBuf, branch: String, fetched: Fetched, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get_mut(&path) else { return };
        entry.fetch.in_flight = false;
        if result_is_current(entry.branch(), &branch) {
            entry.fetch.last_fetch = Some(Instant::now());
            match checks::pr_change(&entry.view, &fetched.view) {
                checks::PrChange::Replaced => entry.checks = ChecksState::default(),
                checks::PrChange::Moved => entry.checks.wanted = true,
                checks::PrChange::Same => {}
            }
            if matches!(fetched.view, PrView::Pr(_) | PrView::NoPr) {
                entry.conflicts = fetched.conflicts;
            }
            entry.view = merge_result(&entry.view, fetched.view);
            entry.divergence = fetched.divergence;
            match &entry.view {
                view if view.gh_unusable() => self.gh_blocked = true,
                PrView::NoPr | PrView::Pr(_) => self.gh_blocked = false,
                _ => {}
            }
        }
        let entry = self.entries.get_mut(&path).expect("entry exists");
        if std::mem::take(&mut entry.fetch.queued) {
            entry.fetch.wanted = Some(PrTrigger::Action);
        }
        cx.notify();
        self.schedule(cx);
    }

    fn start_divergence(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get_mut(&path) else { return };
        let Some(target) = entry.target.clone() else { return };
        entry.div_in_flight = true;
        let repo_path = path.clone();
        let job = cx.background_spawn(async move {
            Repo::open(&repo_path).ok().and_then(|repo| divergence(&repo, &target))
        });
        cx.spawn(async move |store, cx| {
            let counts = job.await;
            let _ = store.update(cx, |store, cx| {
                if let Some(entry) = store.entries.get_mut(&path) {
                    entry.div_in_flight = false;
                    if entry.divergence != counts {
                        entry.divergence = counts;
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    // ---- actions --------------------------------------------------------------------------------

    pub(crate) fn dismiss_error(&mut self, path: &Path, cx: &mut Context<Self>) {
        if let Some(entry) = self.entries.get_mut(path) {
            entry.action_error = None;
            cx.notify();
        }
    }

    /// Shows an error in the PR section of `path` (used when the app cannot deliver a prompt).
    pub(crate) fn report_error(&mut self, path: &Path, message: String, cx: &mut Context<Self>) {
        self.entries.entry(path.to_path_buf()).or_default().action_error = Some(message);
        cx.notify();
    }

    pub(crate) fn run_action(&mut self, path: &Path, action: PrAction, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get_mut(path) else { return };
        let (Some(target), Some(pr)) = (entry.target.clone(), entry.view.pr().cloned()) else {
            return;
        };
        if entry.busy {
            return;
        }
        entry.busy = true;
        entry.action_error = None;
        let merged = matches!(action, PrAction::Merge(_));
        let path = path.to_path_buf();
        let repo_path = path.clone();
        let job = cx.background_spawn(async move {
            let repo = Repo::open(&repo_path).map_err(|error| ForgeError::Failed(error.to_string()))?;
            run_action(&repo, &target, &pr, action)
        });
        cx.spawn(async move |store, cx| {
            let result = job.await;
            let _ = store.update(cx, |store, cx| {
                if let Some(entry) = store.entries.get_mut(&path) {
                    entry.busy = false;
                    entry.action_error = result.err().map(|error| error.to_string());
                    // Whoever shows this worktree next must not trust the cached PR.
                    entry.fetch.last_fetch = None;
                    entry.fetch.wanted = Some(PrTrigger::Action);
                    entry.checks.wanted = true;
                }
                cx.emit(PrStoreEvent::ActionFinished { worktree: path, merged });
                cx.notify();
                store.schedule(cx);
            });
        })
        .detach();
        cx.notify();
    }

    /// Fetches the failed-step logs of `jobs` in the background, writes the report and builds the
    /// one-line prompt, then emits `PrStoreEvent::SendToAgent`. The app pastes it into an agent
    /// terminal without Enter.
    pub(crate) fn send_to_agent(&mut self, path: &Path, jobs: Vec<FailedJob>, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get_mut(path) else { return };
        let (Some(target), Some(pr)) = (entry.target.clone(), entry.view.pr().cloned()) else {
            return;
        };
        if entry.busy || jobs.is_empty() {
            return;
        }
        entry.busy = true;
        entry.action_error = None;
        let path = path.to_path_buf();
        let repo_path = path.clone();
        let job = cx.background_spawn(async move {
            let repo = Repo::open(&repo_path).map_err(|error| error.to_string())?;
            let gh = GitHub::new(repo.root().to_path_buf(), target.repo.clone());
            let dir = agent::agent_prompts_dir()
                .ok_or_else(|| "No local data directory for the report".to_owned())?;
            let unix_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis());
            agent::prepare_agent_line(&gh, &pr, jobs, &dir, unix_ms)
        });
        cx.spawn(async move |store, cx| {
            let result = job.await;
            let _ = store.update(cx, |store, cx| {
                if let Some(entry) = store.entries.get_mut(&path) {
                    entry.busy = false;
                }
                match result {
                    Ok(text) => cx.emit(PrStoreEvent::SendToAgent { worktree: path, text }),
                    Err(message) => store.report_error(&path, message, cx),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// A pull request was created from the dialog (or already existed).
    pub(crate) fn created(&mut self, path: &Path, created: CreatedPr, cx: &mut Context<Self>) {
        if let Some(entry) = self.entries.get_mut(path) {
            if let CreatedPr::Existing(pr) = created {
                entry.view = PrView::Pr(Box::new(pr));
            }
            entry.action_error = None;
        }
        self.request_action_refresh(path, cx);
        cx.notify();
    }
}
